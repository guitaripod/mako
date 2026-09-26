use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use worker::{Env, Result, Fetch, Request as WorkerRequest, Method, Headers};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use crate::error::AppError;
use crate::deployment::{DeploymentConfig, DeploymentMode};
use super::{ImageProvider, UnifiedImageRequest, UnifiedEditRequest, ProviderResponse, ImageBytes, CostEstimate, ProviderFeatures};
use crate::models::ImageUsage;

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const MAX_IMAGE_ATTEMPTS: usize = 3;

/// The Gemini image tiers mako sells. Every legacy Nano Banana id a shipped client
/// still sends rides the current flash model, so old app versions keep working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiImageModel {
    NanoBanana2,
    NanoBananaPro,
}

impl GeminiImageModel {
    pub fn from_model_id(model: &str) -> Option<Self> {
        match model {
            "gemini-2.5-flash" | "gemini-2.5-flash-image-preview" | "gemini-2.5-flash-image"
            | "gemini-3.1-flash" | "gemini-3.1-flash-image" => Some(Self::NanoBanana2),
            "gemini-3-pro-image" | "gemini-3-pro-image-preview" | "nano-banana-pro" => Some(Self::NanoBananaPro),
            _ => None,
        }
    }

    fn api_model(self) -> &'static str {
        match self {
            Self::NanoBanana2 => "gemini-3.1-flash-image",
            Self::NanoBananaPro => "gemini-3-pro-image",
        }
    }

    /// Google's 1K list price × 3 × 100, rounded up: Nano Banana 2 $0.067 → 21,
    /// Nano Banana Pro $0.134 → 41.
    pub fn credits_per_image(self) -> u32 {
        match self {
            Self::NanoBanana2 => 21,
            Self::NanoBananaPro => 41,
        }
    }
}

/// Splits a `data:<mime>;base64,<payload>` input into its MIME type and payload. A bare
/// base64 string is treated as JPEG, which is what clients sent before data URLs.
fn split_image_input(input: &str) -> std::result::Result<(String, String), AppError> {
    let Some(rest) = input.strip_prefix("data:") else {
        return Ok(("image/jpeg".to_string(), input.to_string()));
    };
    let Some((header, payload)) = rest.split_once(',') else {
        return Err(AppError::BadRequest("Invalid image data URL".to_string()));
    };
    let mime = header.split(';').next().unwrap_or("").trim();
    let mime = if mime.starts_with("image/") { mime } else { "image/jpeg" };
    Ok((mime.to_string(), payload.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GenerationConfig {
    #[serde(rename = "responseModalities")]
    response_modalities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiContent {
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum GeminiPart {
    Text { text: String },
    Image {
        #[serde(rename = "inlineData")]
        inline_data: InlineData
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "promptFeedback", default)]
    prompt_feedback: Option<PromptFeedback>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiContent>,
    #[serde(rename = "finishReason", default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PromptFeedback {
    #[serde(rename = "blockReason", default)]
    block_reason: Option<String>,
}

pub struct GeminiProvider {
    api_key: String,
    model: GeminiImageModel,
}

impl GeminiProvider {
    pub fn new(env: &Env, model: GeminiImageModel) -> Result<Self> {
        let deployment_config = DeploymentConfig::from_env(env)
            .map_err(|e| worker::Error::from(AppError::InternalError(format!("Deployment config error: {:?}", e))))?;

        let api_key = match deployment_config.mode {
            DeploymentMode::Official => {
                env.secret("GEMINI_API_KEY")
                    .map_err(|_| worker::Error::RustError("GEMINI_API_KEY not configured".to_string()))?
                    .to_string()
            }
            DeploymentMode::SelfHosted => {
                return Err(worker::Error::RustError("Self-hosted Gemini not supported yet".to_string()));
            }
        };

        Ok(Self { api_key, model })
    }

    pub fn with_api_key(api_key: String) -> Self {
        Self { api_key, model: GeminiImageModel::NanoBanana2 }
    }

    fn image_generation_config() -> Option<GenerationConfig> {
        Some(GenerationConfig {
            response_modalities: vec!["TEXT".to_string(), "IMAGE".to_string()],
        })
    }

    async fn call_gemini_api(&self, request_body: &GeminiRequest) -> Result<GeminiResponse> {
        let headers = Headers::new();
        headers.set("x-goog-api-key", &self.api_key)?;
        headers.set("Content-Type", "application/json")?;

        let mut init = worker::RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&serde_json::to_string(request_body)?)));

        let url = format!("{}/{}:generateContent", GEMINI_API_BASE, self.model.api_model());
        let request = WorkerRequest::new_with_init(&url, &init)?;
        let mut response = Fetch::Request(request).send().await?;

        if response.status_code() >= 400 {
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(AppError::InternalError(format!("Gemini API error: {}", error_text)).into());
        }

        let gemini_response: GeminiResponse = response.json().await
            .map_err(|e| AppError::InternalError(format!("Failed to parse Gemini response: {}", e)))?;

        Ok(gemini_response)
    }

    async fn generate_images(&self, parts: Vec<GeminiPart>) -> Result<Vec<ImageBytes>> {
        let request_body = GeminiRequest {
            contents: vec![GeminiContent { parts }],
            generation_config: Self::image_generation_config(),
        };

        let mut diagnostics = "no response".to_string();
        for attempt in 1..=MAX_IMAGE_ATTEMPTS {
            match self.call_gemini_api(&request_body).await {
                Ok(response) => {
                    let images = Self::collect_images(&response);
                    if !images.is_empty() {
                        return Ok(images);
                    }
                    diagnostics = Self::diagnose(&response);
                }
                Err(e) => {
                    diagnostics = format!("{:?}", e);
                }
            }
            worker::console_log!(
                "Gemini image empty (attempt {}/{}): {}",
                attempt, MAX_IMAGE_ATTEMPTS, diagnostics
            );
        }

        Err(AppError::InternalError(format!(
            "No images returned by Gemini after {} attempts ({})",
            MAX_IMAGE_ATTEMPTS, diagnostics
        )).into())
    }

    fn collect_images(response: &GeminiResponse) -> Vec<ImageBytes> {
        let mut images = Vec::new();
        for candidate in &response.candidates {
            let Some(content) = &candidate.content else { continue };
            for part in &content.parts {
                if let GeminiPart::Image { inline_data } = part {
                    if let Ok(data) = BASE64.decode(&inline_data.data) {
                        let format = match inline_data.mime_type.as_str() {
                            "image/jpeg" => "jpeg",
                            _ => "png",
                        };
                        images.push(ImageBytes { data, format: format.to_string() });
                    }
                }
            }
        }
        images
    }

    fn diagnose(response: &GeminiResponse) -> String {
        let finish = response.candidates.first()
            .and_then(|c| c.finish_reason.clone())
            .unwrap_or_else(|| "none".to_string());
        let block = response.prompt_feedback.as_ref()
            .and_then(|f| f.block_reason.clone())
            .unwrap_or_else(|| "none".to_string());
        format!(
            "candidates={}, finishReason={}, blockReason={}",
            response.candidates.len(), finish, block
        )
    }
}

#[async_trait(?Send)]
impl ImageProvider for GeminiProvider {
    async fn generate_image(&self, request: &UnifiedImageRequest) -> Result<ProviderResponse> {
        let n = request.n.unwrap_or(1);
        let mut all_images = Vec::new();
        let mut all_prompts = Vec::new();

        for _ in 0..n {
            let parts = vec![GeminiPart::Text { text: request.prompt.clone() }];
            let images = self.generate_images(parts).await?;
            for image in images {
                all_images.push(image);
                all_prompts.push(Some(request.prompt.clone()));
            }
        }

        let usage = Some(ImageUsage {
            total_tokens: 1000,
            input_tokens: 500,
            output_tokens: 500,
            input_tokens_details: crate::models::InputTokenDetails {
                text_tokens: 500,
                image_tokens: 0,
            },
        });

        Ok(ProviderResponse {
            images: all_images,
            usage,
            revised_prompts: all_prompts,
        })
    }

    async fn edit_image(&self, request: &UnifiedEditRequest) -> Result<ProviderResponse> {
        if request.image.is_empty() {
            return Err(AppError::BadRequest("No input image provided".to_string()).into());
        }

        let n = request.n.unwrap_or(1);
        let mut all_images = Vec::new();
        let mut all_prompts = Vec::new();

        let (input_mime_type, input_image_data) = split_image_input(&request.image[0])?;

        for _ in 0..n {
            let parts = vec![
                GeminiPart::Text { text: request.prompt.clone() },
                GeminiPart::Image {
                    inline_data: InlineData {
                        mime_type: input_mime_type.clone(),
                        data: input_image_data.clone(),
                    },
                },
            ];
            let images = self.generate_images(parts).await?;
            for image in images {
                all_images.push(image);
                all_prompts.push(Some(request.prompt.clone()));
            }
        }

        let usage = Some(ImageUsage {
            total_tokens: 1500,
            input_tokens: 750,
            output_tokens: 750,
            input_tokens_details: crate::models::InputTokenDetails {
                text_tokens: 250,
                image_tokens: 500,
            },
        });

        Ok(ProviderResponse {
            images: all_images,
            usage,
            revised_prompts: all_prompts,
        })
    }

    fn estimate_cost(&self, request: &UnifiedImageRequest) -> CostEstimate {
        let n = request.n.unwrap_or(1) as u32;
        CostEstimate {
            credits: self.model.credits_per_image() * n,
            provider: "gemini".to_string(),
        }
    }

    fn estimate_edit_cost(&self, request: &UnifiedEditRequest) -> CostEstimate {
        let n = request.n.unwrap_or(1) as u32;
        CostEstimate {
            credits: self.model.credits_per_image() * n,
            provider: "gemini".to_string(),
        }
    }

    fn get_supported_features(&self) -> ProviderFeatures {
        ProviderFeatures {
            supports_size: false,
            supports_quality: false,
            supports_background: false,
            supports_moderation: false,
            supports_edit: true,
            supports_multiple_outputs: true,
            max_outputs: 8,
        }
    }

    fn get_name(&self) -> &str {
        "gemini"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_current_flash_ids_resolve_to_nano_banana_2() {
        for id in ["gemini-2.5-flash", "gemini-2.5-flash-image", "gemini-3.1-flash-image"] {
            assert_eq!(GeminiImageModel::from_model_id(id), Some(GeminiImageModel::NanoBanana2));
        }
        assert_eq!(GeminiImageModel::NanoBanana2.api_model(), "gemini-3.1-flash-image");
        assert_eq!(GeminiImageModel::NanoBanana2.credits_per_image(), 21);
    }

    #[test]
    fn pro_ids_resolve_to_nano_banana_pro() {
        for id in ["gemini-3-pro-image", "gemini-3-pro-image-preview", "nano-banana-pro"] {
            assert_eq!(GeminiImageModel::from_model_id(id), Some(GeminiImageModel::NanoBananaPro));
        }
        assert_eq!(GeminiImageModel::NanoBananaPro.api_model(), "gemini-3-pro-image");
        assert_eq!(GeminiImageModel::NanoBananaPro.credits_per_image(), 41);
        assert_eq!(GeminiImageModel::from_model_id("gpt-image-2"), None);
    }

    #[test]
    fn data_url_keeps_its_mime_type() {
        let (mime, data) = split_image_input("data:image/png;base64,AAAA").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(data, "AAAA");
        let (mime, _) = split_image_input("data:image/webp;base64,BBBB").unwrap();
        assert_eq!(mime, "image/webp");
    }

    #[test]
    fn bare_base64_and_odd_headers_fall_back_to_jpeg() {
        assert_eq!(split_image_input("CCCC").unwrap(), ("image/jpeg".to_string(), "CCCC".to_string()));
        assert_eq!(split_image_input("data:;base64,DDDD").unwrap().0, "image/jpeg");
        assert!(split_image_input("data:image/png;base64").is_err());
    }
}
