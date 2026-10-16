use worker::{console_error, Request, D1Database};
use crate::api_keys::{self, NewCredentials, PlaintextFallback};
use crate::error::AppError;

pub fn validate_api_key(req: &Request) -> Result<String, AppError> {
    let auth_header = req.headers()
        .get("Authorization")
        .ok()
        .flatten()
        .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".to_string()))?;

    if !auth_header.starts_with("Bearer ") {
        return Err(AppError::Unauthorized("Invalid Authorization header format".to_string()));
    }

    let api_key = auth_header.trim_start_matches("Bearer ");
    if api_key.is_empty() {
        return Err(AppError::Unauthorized("Empty API key".to_string()));
    }

    Ok(api_key.to_string())
}

pub fn resolve_app_id(req: &Request) -> String {
    req.headers()
        .get("X-App-ID")
        .ok()
        .flatten()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "pixie".to_string())
}

pub struct AuthedUser {
    pub user_id: String,
    pub app_id: String,
    pub is_admin: bool,
    #[allow(dead_code)]
    pub preferred_model: Option<String>,
}

fn authed_user_from(value: &serde_json::Value, app_id: String) -> AuthedUser {
    AuthedUser {
        user_id: value.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        app_id,
        is_admin: value.get("is_admin").and_then(|v| v.as_i64()).map(|v| v != 0).unwrap_or(false),
        preferred_model: value.get("preferred_model").and_then(|v| v.as_str()).map(|s| s.to_string()),
    }
}

/// Looks the presented key up by its SHA-256, the path every hashed row takes.
async fn find_by_hash(db: &D1Database, app_id: &str, api_key: &str) -> Result<Option<serde_json::Value>, AppError> {
    let hash = api_keys::hash_api_key(api_key);
    let row = db
        .prepare("SELECT id, is_admin, preferred_model, api_key_hash FROM users WHERE app_id = ? AND api_key_hash = ?")
        .bind(&[app_id.into(), hash.into()])
        .map_err(AppError::from)?
        .first::<serde_json::Value>(None)
        .await
        .map_err(AppError::from)?;
    Ok(row.filter(|r| {
        r.get("api_key_hash")
            .and_then(|v| v.as_str())
            .map(|stored| api_keys::hash_matches(stored, api_key))
            .unwrap_or(false)
    }))
}

/// Looks the presented key up in the legacy plaintext column and, on a match,
/// writes its hash in the same request and replaces the plaintext with the
/// placeholder. Placeholders never match.
async fn find_by_plaintext_and_upgrade(db: &D1Database, app_id: &str, api_key: &str) -> Result<Option<serde_json::Value>, AppError> {
    if api_keys::skips_plaintext_lookup(api_key) {
        return Ok(None);
    }
    let row = db
        .prepare(&format!(
            "SELECT id, is_admin, preferred_model, api_key FROM users WHERE app_id = ? AND api_key = ? AND api_key NOT LIKE '{}'",
            api_keys::PLACEHOLDER_LIKE
        ))
        .bind(&[app_id.into(), api_key.into()])
        .map_err(AppError::from)?
        .first::<serde_json::Value>(None)
        .await
        .map_err(AppError::from)?;
    let Some(row) = row else { return Ok(None) };
    let stored = row.get("api_key").and_then(|v| v.as_str()).unwrap_or("");
    let PlaintextFallback::Upgrade { api_key_hash } = api_keys::plaintext_fallback(api_key, stored) else {
        return Ok(None);
    };
    let user_id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if let Err(e) = store_hash_for_plaintext_row(db, user_id, stored, &api_key_hash).await {
        console_error!("api key upgrade failed: {:?}", e);
    }
    Ok(Some(row))
}

/// Sets the hash and the placeholder for one legacy row. Guarded on the old
/// plaintext and an empty hash, so a concurrent request or the backfill can
/// win without either being undone.
async fn store_hash_for_plaintext_row(db: &D1Database, user_id: &str, plaintext: &str, api_key_hash: &str) -> Result<(), AppError> {
    db.prepare("UPDATE users SET api_key_hash = ?, api_key = ? WHERE id = ? AND api_key = ? AND api_key_hash IS NULL")
        .bind(&[api_key_hash.into(), api_keys::placeholder_for(user_id).into(), user_id.into(), plaintext.into()])
        .map_err(AppError::from)?
        .run()
        .await
        .map_err(AppError::from)?;
    Ok(())
}

pub async fn authenticate(req: &Request, db: &D1Database) -> Result<AuthedUser, AppError> {
    let api_key = validate_api_key(req)?;
    let app_id = resolve_app_id(req);

    let row = match find_by_hash(db, &app_id, &api_key).await? {
        Some(row) => Some(row),
        None => find_by_plaintext_and_upgrade(db, &app_id, &api_key).await?,
    };

    let value = row.ok_or_else(|| AppError::Unauthorized("Invalid API key".to_string()))?;
    Ok(authed_user_from(&value, app_id))
}

/// Issues a new key for an existing user and stores only its hash. The old key
/// stops working. Used wherever a login has to hand a key back and the database
/// no longer holds one.
pub async fn issue_api_key(db: &D1Database, app_id: &str, user_id: &str) -> Result<String, AppError> {
    let creds = NewCredentials::generate(user_id);
    let now = chrono::Utc::now().to_rfc3339();
    let result = db
        .prepare("UPDATE users SET api_key = ?, api_key_hash = ?, updated_at = ? WHERE app_id = ? AND id = ?")
        .bind(&[creds.placeholder.clone().into(), creds.api_key_hash.clone().into(), now.into(), app_id.into(), user_id.into()])
        .map_err(AppError::from)?
        .run()
        .await
        .map_err(AppError::from)?;
    let changed = result.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0) > 0;
    if !changed {
        return Err(AppError::InternalError("User vanished while issuing an API key".to_string()));
    }
    Ok(creds.api_key)
}

/// Hashes up to `BACKFILL_BATCH` legacy plaintext keys and swaps each for the
/// placeholder in one UPDATE guarded on the old plaintext and an empty hash, so
/// reruns and a concurrent upgrade-on-use cannot clash. Returns how many rows
/// it changed; no key value is ever logged.
pub async fn backfill_api_key_hashes(db: &D1Database) -> Result<u32, AppError> {
    let rows = db
        .prepare(&format!(
            "SELECT id, api_key FROM users WHERE api_key_hash IS NULL AND api_key NOT LIKE '{}' LIMIT {}",
            api_keys::PLACEHOLDER_LIKE,
            api_keys::BACKFILL_BATCH
        ))
        .all()
        .await
        .map_err(AppError::from)?
        .results::<serde_json::Value>()
        .map_err(AppError::from)?;

    let mut statements = Vec::with_capacity(rows.len());
    for row in &rows {
        let user_id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let plaintext = row.get("api_key").and_then(|v| v.as_str()).unwrap_or("");
        if api_keys::is_placeholder(plaintext) {
            continue;
        }
        let api_key_hash = api_keys::hash_api_key(plaintext);
        statements.push(
            db.prepare("UPDATE users SET api_key_hash = ?, api_key = ? WHERE id = ? AND api_key = ? AND api_key_hash IS NULL")
                .bind(&[api_key_hash.into(), api_keys::placeholder_for(user_id).into(), user_id.into(), plaintext.into()])
                .map_err(AppError::from)?,
        );
    }
    if statements.is_empty() {
        return Ok(0);
    }

    let results = db.batch(statements).await.map_err(AppError::from)?;
    let changed = results
        .iter()
        .map(|r| r.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0) as u32)
        .sum();
    Ok(changed)
}
