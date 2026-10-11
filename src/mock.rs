use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use worker::{Env, Result};
use crate::chat_alias::{sample_for_schema, AliasConfig, Reply, ReplyError, Turn};
use crate::error::AppError;
use crate::models::{ImageUsage, InputTokenDetails};
use crate::providers::{
    CostEstimate, ImageBytes, ImageProvider, ProviderFeatures, ProviderResponse, UnifiedEditRequest,
    UnifiedImageRequest,
};

const ONE_PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
pub const TRUNCATE_MARKER: &str = "MOCK_TRUNCATE";
pub const NO_IMAGE_MARKER: &str = "MOCK_NO_IMAGE";
const MOCK_IMAGE_CREDITS: u32 = 21;

/// Mock providers answer instead of the real ones only outside production and
/// only when `MOCK_PROVIDERS=1` is set (in `.dev.vars`, never in `wrangler.toml`).
/// A missing `ENVIRONMENT` counts as production.
pub fn mock_allowed(environment: Option<&str>, flag: Option<&str>) -> bool {
    matches!(environment, Some(e) if e != "production") && flag == Some("1")
}

pub fn enabled(env: &Env) -> bool {
    let read = |name: &str| {
        env.var(name)
            .map(|v| v.to_string())
            .or_else(|_| env.secret(name).map(|v| v.to_string()))
            .ok()
    };
    mock_allowed(read("ENVIRONMENT").as_deref(), read("MOCK_PROVIDERS").as_deref())
}

pub fn legacy_chat_reply() -> (String, u64, u64) {
    ("mock reply".to_string(), 5, 5)
}

/// A deterministic alias answer: JSON that satisfies the alias's schema, or a
/// truncation when the last user turn contains `MOCK_TRUNCATE`.
pub fn alias_reply(cfg: &AliasConfig, turns: &[Turn]) -> std::result::Result<Reply, ReplyError> {
    let asked_to_truncate = turns.last().map(|t| t.content.contains(TRUNCATE_MARKER)).unwrap_or(false);
    if asked_to_truncate {
        return Err(ReplyError::Truncated);
    }
    let text = match &cfg.response_schema {
        Some(schema) => sample_for_schema(schema).to_string(),
        None => "mock reply".to_string(),
    };
    Ok(Reply { text, prompt_tokens: 5, output_tokens: 5 })
}

pub struct MockImageProvider;

#[async_trait(?Send)]
impl ImageProvider for MockImageProvider {
    async fn generate_image(&self, request: &UnifiedImageRequest) -> Result<ProviderResponse> {
        if request.prompt.contains(NO_IMAGE_MARKER) {
            return Err(AppError::InternalError("No images returned by Gemini after 3 attempts (mock)".to_string()).into());
        }
        let count = request.n.unwrap_or(1) as usize;
        let data = BASE64.decode(ONE_PIXEL_PNG).unwrap_or_default();
        Ok(ProviderResponse {
            images: (0..count).map(|_| ImageBytes { data: data.clone(), format: "png".to_string() }).collect(),
            usage: Some(ImageUsage {
                total_tokens: 10,
                input_tokens: 5,
                output_tokens: 5,
                input_tokens_details: InputTokenDetails { text_tokens: 5, image_tokens: 0 },
            }),
            revised_prompts: (0..count).map(|_| None).collect(),
        })
    }

    async fn edit_image(&self, _request: &UnifiedEditRequest) -> Result<ProviderResponse> {
        Err(AppError::BadRequest("The mock provider does not edit images".to_string()).into())
    }

    fn estimate_cost(&self, request: &UnifiedImageRequest) -> CostEstimate {
        CostEstimate {
            credits: MOCK_IMAGE_CREDITS * request.n.unwrap_or(1) as u32,
            provider: "mock".to_string(),
        }
    }

    fn estimate_edit_cost(&self, request: &UnifiedEditRequest) -> CostEstimate {
        CostEstimate {
            credits: MOCK_IMAGE_CREDITS * request.n.unwrap_or(1) as u32,
            provider: "mock".to_string(),
        }
    }

    fn get_supported_features(&self) -> ProviderFeatures {
        ProviderFeatures {
            supports_size: false,
            supports_quality: false,
            supports_background: false,
            supports_moderation: false,
            supports_edit: false,
            supports_multiple_outputs: true,
            max_outputs: 2,
        }
    }

    fn get_name(&self) -> &str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_alias::Role;
    use serde_json::json;

    fn cfg(schema: Option<serde_json::Value>) -> AliasConfig {
        AliasConfig {
            alias: "test".into(),
            model: "m".into(),
            system_prompt: "s".into(),
            response_schema: schema,
            max_tokens: None,
            temperature: None,
            flat_credits: 1,
            weekly_free: 0,
            premium_bypass: false,
            version: 1,
        }
    }

    #[test]
    fn mocks_never_run_in_production_or_without_the_flag() {
        assert!(!mock_allowed(Some("production"), Some("1")));
        assert!(!mock_allowed(None, Some("1")));
        assert!(!mock_allowed(Some("development"), None));
        assert!(!mock_allowed(Some("development"), Some("0")));
        assert!(!mock_allowed(Some("development"), Some("true")));
        assert!(mock_allowed(Some("development"), Some("1")));
        assert!(mock_allowed(Some("local"), Some("1")));
    }

    #[test]
    fn the_mock_reply_matches_the_alias_schema() {
        let turns = vec![Turn { role: Role::User, content: "hello".into() }];
        let schema = json!({ "type": "object", "properties": { "essence": { "type": "string" } } });
        let reply = alias_reply(&cfg(Some(schema)), &turns).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&reply.text).unwrap(), json!({ "essence": "mock" }));
        assert_eq!(alias_reply(&cfg(None), &turns).unwrap().text, "mock reply");
    }

    #[test]
    fn the_truncate_marker_simulates_a_cut_off_reply() {
        let turns = vec![Turn { role: Role::User, content: format!("please {}", TRUNCATE_MARKER) }];
        assert_eq!(alias_reply(&cfg(None), &turns), Err(ReplyError::Truncated));
    }

    #[test]
    fn the_embedded_png_decodes() {
        let bytes = BASE64.decode(ONE_PIXEL_PNG).unwrap();
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
    }
}
