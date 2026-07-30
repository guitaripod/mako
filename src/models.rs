use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub provider: AuthProvider,
    pub provider_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub api_key: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthProvider {
    Apple,
    GitHub,
    Google,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredImage {
    pub id: String,
    pub user_id: String,
    pub r2_key: String,
    pub url: String,
    pub prompt: String,
    pub model: String,
    pub size: String,
    pub quality: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshToken {
    pub id: String,
    pub user_id: String,
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_n")]
    pub n: u8,
    #[serde(default = "default_quality")]
    pub quality: String,
    #[serde(default = "default_size")]
    pub size: String,
    #[serde(default = "default_background")]
    pub background: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moderation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_compression: Option<u8>,
    #[serde(default = "default_output_format")]
    pub output_format: String,
    #[serde(default = "default_partial_images")]
    pub partial_images: u8,
    #[serde(default = "default_stream")]
    pub stream: bool,
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openai_api_key: Option<String>, // For self-hosted deployments
    /// When false, the image is stored privately and never appears in the public
    /// gallery feed. Absent/None preserves the historical default of public.
    #[serde(default)]
    pub is_public: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEditRequest {
    pub image: Vec<String>,
    pub prompt: String,
    pub mask: Option<String>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_n")]
    pub n: u8,
    #[serde(default = "default_size")]
    pub size: String,
    #[serde(default = "default_quality")]
    pub quality: String,
    #[serde(default = "default_background")]
    pub background: String,
    #[serde(default = "default_input_fidelity")]
    pub input_fidelity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_compression: Option<u8>,
    #[serde(default = "default_output_format")]
    pub output_format: String,
    #[serde(default = "default_partial_images")]
    pub partial_images: u8,
    #[serde(default = "default_stream")]
    pub stream: bool,
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openai_api_key: Option<String>, // For self-hosted deployments
    /// When false, the edited image is stored privately and never appears in the
    /// public gallery feed. Absent/None preserves the historical default of public.
    #[serde(default)]
    pub is_public: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageResponse {
    pub created: u64,
    pub data: Vec<ImageData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ImageUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUsage {
    pub total_tokens: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub input_tokens_details: InputTokenDetails,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputTokenDetails {
    pub text_tokens: u32,
    pub image_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    pub param: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: u64,
    pub iat: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
}

/// What Apple's AdServices attribution endpoint says produced an install.
/// `attributed == false` is Apple's organic answer and carries no campaign data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdServicesAttribution {
    pub attributed: bool,
    pub org_id: Option<i64>,
    pub campaign_id: Option<i64>,
    pub ad_group_id: Option<i64>,
    pub keyword_id: Option<i64>,
    pub ad_id: Option<i64>,
    pub country_or_region: Option<String>,
    pub click_date: Option<String>,
    pub conversion_type: Option<String>,
}

/// A persisted install attribution, as stored against an existing mako identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallAttribution {
    pub app_id: String,
    pub user_id: String,
    pub attributed: bool,
    pub campaign_id: Option<i64>,
    pub ad_group_id: Option<i64>,
    pub keyword_id: Option<i64>,
    pub country_or_region: Option<String>,
    pub created_at: String,
}

/// Spend-relevant counts for one keyword (or for a campaign's non-keyword
/// placement, where `keyword_id` is null).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeywordPerformance {
    pub campaign_id: Option<i64>,
    pub ad_group_id: Option<i64>,
    pub keyword_id: Option<i64>,
    pub country_or_region: Option<String>,
    pub installs: i64,
    pub payers: i64,
    pub purchases: i64,
    pub revenue_usd_cents: i64,
    pub reported_net_usd_cents: i64,
    pub purchases_missing_net: i64,
    pub revenue_missing_net_usd_cents: i64,
    pub net_revenue_usd_cents: Option<i64>,
    pub net_per_payer_usd_cents: Option<i64>,
    pub revenue_per_payer_usd_cents: Option<i64>,
    pub first_install_at: Option<String>,
    pub last_install_at: Option<String>,
}

/// Window-wide totals accompanying a keyword report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributionTotals {
    pub total_installs: i64,
    pub attributed_installs: i64,
    pub organic_installs: i64,
    pub payers: i64,
    pub purchases: i64,
    pub revenue_usd_cents: i64,
    pub reported_net_usd_cents: i64,
    pub purchases_missing_net: i64,
    pub revenue_missing_net_usd_cents: i64,
    pub net_revenue_usd_cents: Option<i64>,
    pub net_per_payer_usd_cents: Option<i64>,
    pub revenue_per_payer_usd_cents: Option<i64>,
    pub store_net_share: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub id: String,
    pub user_id: String,
    pub request_type: String,
    pub model: String,
    pub prompt: String,
    pub image_size: String,
    pub image_quality: String,
    pub image_count: u8,
    pub input_images_count: Option<u8>,
    pub total_tokens: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub text_tokens: u32,
    pub image_tokens: u32,
    pub r2_keys: Vec<String>,
    pub response_time_ms: u32,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

fn default_model() -> String {
    "gemini-2.5-flash".to_string()
}

fn default_n() -> u8 {
    1
}

fn default_size() -> String {
    "auto".to_string()
}

fn default_quality() -> String {
    "auto".to_string()
}

fn default_background() -> String {
    "auto".to_string()
}

fn default_output_format() -> String {
    "png".to_string()
}

fn default_partial_images() -> u8 {
    0
}

fn default_stream() -> bool {
    false
}

fn default_input_fidelity() -> String {
    "low".to_string()
}