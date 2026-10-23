use worker::{Env, Result};
use crate::models::StoredImage;
use crate::error::AppError;
use uuid::Uuid;
use chrono::{Utc, Duration};
use base64::{Engine as _, engine::general_purpose};

pub const DEFAULT_PUBLIC_BASE_URL: &str = "https://api.midgarcorp.cc/v1/intelligence";

/// The URL clients load a stored image from: the `PUBLIC_BASE_URL` variable, or the public gateway's
/// intelligence mount when it is unset or blank, followed by `/r2/` and the object key.
pub fn public_image_url(env: &Env, r2_key: &str) -> String {
    let configured = env.var("PUBLIC_BASE_URL").ok().map(|v| v.to_string());
    image_url(configured.as_deref(), r2_key)
}

/// Joins a configured base (or the default when `None` or blank) with `/r2/{r2_key}`, tolerating trailing slashes.
pub fn image_url(base: Option<&str>, r2_key: &str) -> String {
    let base = base.map(str::trim).filter(|b| !b.is_empty()).unwrap_or(DEFAULT_PUBLIC_BASE_URL);
    format!("{}/r2/{}", base.trim_end_matches('/'), r2_key)
}

pub async fn store_image_in_r2(
    env: &Env,
    user_id: &str,
    base64_data: &str,
    prompt: &str,
    model: &str,
    size: &str,
    quality: Option<&str>,
) -> Result<StoredImage> {
    let r2 = env.bucket("IMAGES").map_err(|e| AppError::InternalError(format!("Failed to get R2 bucket: {}", e)))?;
    
    let image_id = Uuid::new_v4().to_string();
    let r2_key = format!("{}/{}.png", user_id, image_id);
    
    let image_bytes = general_purpose::STANDARD
        .decode(base64_data)
        .map_err(|e| AppError::BadRequest(format!("Invalid base64 image data: {}", e)))?;
    
    r2.put(&r2_key, image_bytes)
        .execute()
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to store image in R2: {}", e)))?;
    
    let url = public_image_url(env, &r2_key);
    
    let now = Utc::now();
    let expires_at = now + Duration::days(7);
    
    let stored_image = StoredImage {
        id: image_id,
        user_id: user_id.to_string(),
        r2_key,
        url,
        prompt: prompt.to_string(),
        model: model.to_string(),
        size: size.to_string(),
        quality: quality.map(|q| q.to_string()),
        created_at: now,
        expires_at,
    };
    
    Ok(stored_image)
}

pub async fn store_image_from_bytes(
    env: &Env,
    user_id: &str,
    image_bytes: &[u8],
    prompt: &str,
    model: &str,
    size: &str,
    quality: Option<&str>,
    _provider: &str,
) -> Result<StoredImage> {
    let r2 = env.bucket("IMAGES").map_err(|e| AppError::InternalError(format!("Failed to get R2 bucket: {}", e)))?;
    
    let image_id = Uuid::new_v4().to_string();
    let r2_key = format!("{}/{}.png", user_id, image_id);
    
    r2.put(&r2_key, image_bytes.to_vec())
        .execute()
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to store image in R2: {}", e)))?;
    
    let url = public_image_url(env, &r2_key);
    
    let now = Utc::now();
    let expires_at = now + Duration::days(7);
    
    let stored_image = StoredImage {
        id: image_id,
        user_id: user_id.to_string(),
        r2_key,
        url,
        prompt: prompt.to_string(),
        model: model.to_string(),
        size: size.to_string(),
        quality: quality.map(|q| q.to_string()),
        created_at: now,
        expires_at,
    };
    
    Ok(stored_image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_base_is_the_public_gateway() {
        assert_eq!(
            image_url(None, "u1/i1.png"),
            "https://api.midgarcorp.cc/v1/intelligence/r2/u1/i1.png"
        );
    }

    #[test]
    fn a_configured_base_replaces_the_default() {
        assert_eq!(
            image_url(Some("https://example.test/v1/intelligence"), "u1/i1.png"),
            "https://example.test/v1/intelligence/r2/u1/i1.png"
        );
    }

    #[test]
    fn trailing_slashes_and_blank_values_are_tolerated() {
        assert_eq!(
            image_url(Some("https://example.test/base//"), "u1/i1.png"),
            "https://example.test/base/r2/u1/i1.png"
        );
        assert_eq!(image_url(Some("   "), "u1/i1.png"), image_url(None, "u1/i1.png"));
        assert_eq!(image_url(Some(""), "u1/i1.png"), image_url(None, "u1/i1.png"));
    }
}
