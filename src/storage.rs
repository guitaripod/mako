use worker::{D1Database, Env, Result};
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

/// Most images one bulk hide moves to fresh keys in a single request: each move costs three R2 calls and one D1
/// write, which keeps the request inside the Workers subrequest limit. The rest are reported as pending.
pub const MAX_KEYS_RETIRED_PER_REQUEST: usize = 200;

/// A fresh object key for one of `user_id`'s stored images: a new random name, so no URL of an earlier key reaches it.
pub fn fresh_image_key(user_id: &str) -> String {
    format!("{}/{}.png", user_id, Uuid::new_v4())
}

/// Whether a visibility change retires the image's key: only a public image being hidden, because a link to it could
/// only have been shared while it was public (decision D28).
pub fn retires_key(was_public: bool, now_public: bool) -> bool {
    was_public && !now_public
}

/// Moves one stored image to a fresh R2 key and returns the key, or `None` when the object is already gone or the row
/// moved meanwhile. It copies the object, points `stored_images` at the copy only while the row still names `old_key`,
/// then deletes the old object, so every URL the image had stops working.
pub async fn retire_image_key(
    env: &Env,
    db: &D1Database,
    app_id: &str,
    user_id: &str,
    image_id: &str,
    old_key: &str,
) -> Result<Option<String>> {
    let bucket = env.bucket("IMAGES").map_err(|e| AppError::InternalError(format!("Failed to get R2 bucket: {}", e)))?;
    let Some(object) = bucket.get(old_key).execute().await? else { return Ok(None) };
    let Some(body) = object.body() else { return Ok(None) };
    let bytes = body.bytes().await?;
    let new_key = fresh_image_key(user_id);
    bucket
        .put(&new_key, bytes)
        .execute()
        .await
        .map_err(|e| AppError::InternalError(format!("Failed to copy image in R2: {}", e)))?;
    let moved = db
        .prepare("UPDATE stored_images SET r2_key = ?1 WHERE app_id = ?2 AND id = ?3 AND user_id = ?4 AND r2_key = ?5")
        .bind(&[new_key.clone().into(), app_id.into(), image_id.into(), user_id.into(), old_key.into()])?
        .run()
        .await?;
    if moved.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0) == 0 {
        bucket.delete(&new_key).await?;
        return Ok(None);
    }
    if let Err(e) = bucket.delete(old_key).await {
        crate::log_error!("Failed to delete a retired image key", serde_json::json!({ "error": e.to_string(), "r2_key": old_key }));
    }
    Ok(Some(new_key))
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

    #[test]
    fn a_fresh_key_stays_under_the_owner_and_is_new_every_time() {
        let first = fresh_image_key("u1");
        let second = fresh_image_key("u1");
        assert!(first.starts_with("u1/") && first.ends_with(".png"));
        assert_ne!(first, second);
        assert_eq!(first.len(), "u1/".len() + 36 + ".png".len());
    }

    #[test]
    fn only_hiding_a_public_image_retires_its_key() {
        assert!(retires_key(true, false));
        assert!(!retires_key(false, false));
        assert!(!retires_key(true, true));
        assert!(!retires_key(false, true));
    }
}
