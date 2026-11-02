use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use worker::{Env, Result, Fetch, Request as WorkerRequest, Method, Headers};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use crate::error::AppError;
use crate::deployment::{DeploymentConfig, DeploymentMode};
use super::route::Routing;
use crate::handlers::chat::Provider;
use super::{ImageProvider, UnifiedImageRequest, UnifiedEditRequest, ProviderResponse, ImageBytes, CostEstimate, ProviderFeatures};
use crate::models::ImageUsage;

const MAX_IMAGE_ATTEMPTS: usize = 3;

/// Starts the error text of a Gemini call that failed, which `provider_failure_response`
/// answers with 503 `provider_unavailable` rather than blaming the request.
pub const GEMINI_UNAVAILABLE: &str = "Gemini unavailable";

/// Why one Gemini model produced no image.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ImageFailure {
    /// Google replied without an image, usually its safety filter on a photo of a person.
    Declined(String),
    /// The call failed: Google's HTTP status, or none when no readable reply came back.
    Unavailable { status: Option<u16>, detail: String },
}

impl ImageFailure {
    /// A decline, a rate limit, a server error or a lost reply can pass on another try; any
    /// other 4xx (billing, key, model, request) fails the same way every time.
    fn is_retriable(&self) -> bool {
        match self {
            Self::Declined(_) => true,
            Self::Unavailable { status, .. } => matches!(status, None | Some(408) | Some(429) | Some(500..)),
        }
    }

    /// Google refused the model or the request shape it takes, so an older model may serve it.
    fn model_refused(&self) -> bool {
        matches!(self, Self::Unavailable { status: Some(400 | 404), .. })
    }

    fn into_error(self) -> worker::Error {
        let message = match self {
            Self::Declined(diagnostics) => format!(
                "No images returned by Gemini after {} attempts ({})",
                MAX_IMAGE_ATTEMPTS, diagnostics
            ),
            Self::Unavailable { detail, .. } => format!("{} ({})", GEMINI_UNAVAILABLE, detail),
        };
        AppError::InternalError(message).into()
    }
}

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
            | "gemini-3.1-flash" | "gemini-3.1-flash-image" | "gemini-nano-banana-2.1" => Some(Self::NanoBanana2),
            "gemini-3-pro-image" | "gemini-3-pro-image-preview" | "nano-banana-pro" => Some(Self::NanoBananaPro),
            _ => None,
        }
    }

    /// The models to ask, newest first. Nano Banana 2.1 replaces Nano Banana 2 at half its
    /// list price; 2 stays behind it for a request Google refuses on 2.1.
    fn api_models(self) -> &'static [&'static str] {
        match self {
            Self::NanoBanana2 => &["gemini-nano-banana-2.1", "gemini-3.1-flash-image"],
            Self::NanoBananaPro => &["gemini-3-pro-image"],
        }
    }

    /// Google's 1K list price × 3 × 100, rounded up, on the dearest model a request can
    /// land on: Nano Banana 2 $0.067 → 21 (2.1 lists at $0.0336), Nano Banana Pro $0.134 → 41.
    pub fn credits_per_image(self) -> u32 {
        match self {
            Self::NanoBanana2 => 21,
            Self::NanoBananaPro => 41,
        }
    }
}

/// Splits a `data:<mime>;base64,<payload>` input into its MIME type and payload. A bare
/// base64 string is treated as JPEG, which is what clients sent before data URLs.
pub(crate) fn split_image_input(input: &str) -> std::result::Result<(String, String), AppError> {
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
    routing: Routing,
}

impl GeminiProvider {
    pub fn new(env: &Env, model: GeminiImageModel, routing: Routing) -> Result<Self> {
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

        Ok(Self { api_key, model, routing })
    }

    pub fn with_api_key(api_key: String) -> Self {
        Self { api_key, model: GeminiImageModel::NanoBanana2, routing: Routing::direct() }
    }

    fn image_generation_config() -> Option<GenerationConfig> {
        Some(GenerationConfig {
            response_modalities: vec!["TEXT".to_string(), "IMAGE".to_string()],
        })
    }

    /// Posts one generateContent request for `model` along the configured route.
    async fn send(&self, model: &str, request_body: &GeminiRequest) -> Result<worker::Response> {
        let headers = Headers::new();
        headers.set("x-goog-api-key", &self.api_key)?;
        headers.set("Content-Type", "application/json")?;
        let route = self.routing.route(&Provider::Gemini, &format!("{}:generateContent", model));
        route.apply(&headers)?;

        let mut init = worker::RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&serde_json::to_string(request_body)?)));

        let request = WorkerRequest::new_with_init(&route.url, &init)?;
        Fetch::Request(request).send().await
    }

    async fn call_gemini_api(&self, model: &str, request_body: &GeminiRequest) -> std::result::Result<GeminiResponse, ImageFailure> {
        let mut response = self.send(model, request_body).await
            .map_err(|e| ImageFailure::Unavailable { status: None, detail: format!("{:?}", e) })?;

        let status = response.status_code();
        if status >= 400 {
            let error_text = response.text().await.unwrap_or_default();
            return Err(ImageFailure::Unavailable {
                status: Some(status),
                detail: crate::redact::provider_error_summary(status, &error_text),
            });
        }

        response.json().await
            .map_err(|e| ImageFailure::Unavailable { status: None, detail: format!("Failed to parse Gemini response: {}", e) })
    }

    /// The images and the model that made them.
    async fn generate_images(&self, parts: Vec<GeminiPart>) -> Result<(Vec<ImageBytes>, &'static str)> {
        let request_body = GeminiRequest {
            contents: vec![GeminiContent { parts }],
            generation_config: Self::image_generation_config(),
        };

        let mut failure = ImageFailure::Declined("no response".to_string());
        for model in self.model.api_models() {
            match self.images_from(model, &request_body).await {
                Ok(images) => return Ok((images, model)),
                Err(refused) if refused.model_refused() => {
                    worker::console_log!("Gemini {} refused the request, trying the next model: {:?}", model, refused);
                    failure = refused;
                }
                Err(other) => return Err(other.into_error()),
            }
        }
        Err(failure.into_error())
    }

    /// Asks one model up to `MAX_IMAGE_ATTEMPTS` times, stopping early on a failure a retry cannot fix.
    async fn images_from(&self, model: &str, request_body: &GeminiRequest) -> std::result::Result<Vec<ImageBytes>, ImageFailure> {
        let mut failure = ImageFailure::Declined("no response".to_string());
        for attempt in 1..=MAX_IMAGE_ATTEMPTS {
            failure = match self.call_gemini_api(model, request_body).await {
                Ok(response) => {
                    let images = Self::collect_images(&response);
                    if !images.is_empty() {
                        return Ok(images);
                    }
                    ImageFailure::Declined(Self::diagnose(&response))
                }
                Err(call) => call,
            };
            worker::console_log!(
                "Gemini {} gave no image (attempt {}/{}): {:?}",
                model, attempt, MAX_IMAGE_ATTEMPTS, failure
            );
            if !failure.is_retriable() {
                break;
            }
        }
        Err(failure)
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
        let mut served_by = None;

        for _ in 0..n {
            let parts = vec![GeminiPart::Text { text: request.prompt.clone() }];
            let (images, model) = self.generate_images(parts).await?;
            served_by = Some(model.to_string());
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
            served_by,
        })
    }

    async fn edit_image(&self, request: &UnifiedEditRequest) -> Result<ProviderResponse> {
        if request.image.is_empty() {
            return Err(AppError::BadRequest("No input image provided".to_string()).into());
        }

        let n = request.n.unwrap_or(1);
        let mut all_images = Vec::new();
        let mut all_prompts = Vec::new();
        let mut served_by = None;

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
            let (images, model) = self.generate_images(parts).await?;
            served_by = Some(model.to_string());
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
            served_by,
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
        for id in ["gemini-2.5-flash", "gemini-2.5-flash-image", "gemini-3.1-flash-image", "gemini-nano-banana-2.1"] {
            assert_eq!(GeminiImageModel::from_model_id(id), Some(GeminiImageModel::NanoBanana2));
        }
        assert_eq!(GeminiImageModel::NanoBanana2.api_models(), ["gemini-nano-banana-2.1", "gemini-3.1-flash-image"]);
        assert_eq!(GeminiImageModel::NanoBanana2.credits_per_image(), 21);
    }

    fn unavailable(status: Option<u16>) -> ImageFailure {
        ImageFailure::Unavailable { status, detail: String::new() }
    }

    #[test]
    fn billing_and_request_errors_are_not_retried_but_outages_and_declines_are() {
        for status in [Some(400), Some(401), Some(402), Some(403), Some(404)] {
            assert!(!unavailable(status).is_retriable(), "{status:?}");
        }
        for status in [None, Some(408), Some(429), Some(500), Some(503)] {
            assert!(unavailable(status).is_retriable(), "{status:?}");
        }
        assert!(ImageFailure::Declined("finishReason=STOP".into()).is_retriable());
    }

    #[test]
    fn only_a_refused_model_falls_back_to_the_older_one() {
        assert!(unavailable(Some(400)).model_refused());
        assert!(unavailable(Some(404)).model_refused());
        for failure in [unavailable(Some(402)), unavailable(Some(429)), unavailable(None), ImageFailure::Declined(String::new())] {
            assert!(!failure.model_refused(), "{failure:?}");
        }
    }

    #[test]
    fn a_failed_call_reads_as_unavailable_and_a_decline_keeps_its_no_image_text() {
        let failed = format!("{:?}", unavailable(Some(402)).into_error());
        assert!(failed.contains("Gemini unavailable (") && !failed.contains("No images returned by Gemini"));
        let declined = format!("{:?}", ImageFailure::Declined("blockReason=none".into()).into_error());
        assert!(declined.contains("No images returned by Gemini after 3 attempts (blockReason=none)"));
    }

    #[test]
    fn pro_ids_resolve_to_nano_banana_pro() {
        for id in ["gemini-3-pro-image", "gemini-3-pro-image-preview", "nano-banana-pro"] {
            assert_eq!(GeminiImageModel::from_model_id(id), Some(GeminiImageModel::NanoBananaPro));
        }
        assert_eq!(GeminiImageModel::NanoBananaPro.api_models(), ["gemini-3-pro-image"]);
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
