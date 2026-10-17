use worker::{console_error, D1Database, Env, Request};
use crate::api_keys::{self, LookupStep};
use crate::error::AppError;
use crate::ledgers::Account;
use crate::midgar_token::{self, MidgarCaller};

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

/// The tenant a request is for: `X-App-ID` (default `pixie`), accepted as either mako's `apps.app_id` or the
/// registry slug and always returned as the `apps.app_id` every table stores.
pub fn resolve_app_id(req: &Request) -> String {
    let raw = req.headers()
        .get("X-App-ID")
        .ok()
        .flatten()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "pixie".to_string());
    crate::tenant::canonical_app_id(&raw)
}

pub struct AuthedUser {
    pub user_id: String,
    pub app_id: String,
    pub is_admin: bool,
    pub account: Account,
    /// The Midgar identity behind a valid `X-Midgar-Token`, when the request carried one. Nothing routes on it
    /// yet: every account stays `Account::Local` until linking lands.
    #[allow(dead_code)]
    pub midgar: Option<MidgarCaller>,
    #[allow(dead_code)]
    pub preferred_model: Option<String>,
}

fn authed_user_from(value: &serde_json::Value, app_id: String) -> AuthedUser {
    let user_id = value.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    AuthedUser {
        account: Account::local(&app_id, &user_id),
        midgar: None,
        user_id,
        app_id,
        is_admin: value.get("is_admin").and_then(|v| v.as_i64()).map(|v| v != 0).unwrap_or(false),
        preferred_model: value.get("preferred_model").and_then(|v| v.as_str()).map(|s| s.to_string()),
    }
}

/// Looks the presented key up in the per-device `api_keys` table by its
/// SHA-256 and refreshes `last_used_at` at most once an hour.
async fn find_in_key_table(db: &D1Database, app_id: &str, api_key: &str) -> Result<Option<serde_json::Value>, AppError> {
    let hash = api_keys::hash_api_key(api_key);
    let row = db
        .prepare(
            "SELECT u.id, u.is_admin, u.preferred_model, k.key_hash, k.last_used_at
             FROM api_keys k JOIN users u ON u.id = k.user_id AND u.app_id = k.app_id
             WHERE k.app_id = ? AND k.key_hash = ?",
        )
        .bind(&[app_id.into(), hash.clone().into()])
        .map_err(AppError::from)?
        .first::<serde_json::Value>(None)
        .await
        .map_err(AppError::from)?
        .filter(|r| stored_hash_matches(r, "key_hash", api_key));
    if let Some(row) = &row {
        let last_used = row.get("last_used_at").and_then(|v| v.as_str());
        if api_keys::should_refresh_last_used(last_used, chrono::Utc::now()) {
            touch_last_used(db, &hash).await;
        }
    }
    Ok(row)
}

/// Records that a key was just used. A failure only costs the timestamp, so it
/// is logged and never fails the request.
async fn touch_last_used(db: &D1Database, key_hash: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let outcome = match db.prepare("UPDATE api_keys SET last_used_at = ? WHERE key_hash = ?").bind(&[now.into(), key_hash.into()]) {
        Ok(statement) => statement.run().await.map(|_| ()),
        Err(e) => Err(e),
    };
    if let Err(e) = outcome {
        console_error!("api key last_used_at update failed: {:?}", e);
    }
}

fn stored_hash_matches(row: &serde_json::Value, column: &str, api_key: &str) -> bool {
    row.get(column)
        .and_then(|v| v.as_str())
        .map(|stored| api_keys::hash_matches(stored, api_key))
        .unwrap_or(false)
}

/// Looks the presented key up by the single legacy hash on `users`.
async fn find_by_legacy_hash(db: &D1Database, app_id: &str, api_key: &str) -> Result<Option<serde_json::Value>, AppError> {
    let hash = api_keys::hash_api_key(api_key);
    let row = db
        .prepare("SELECT id, is_admin, preferred_model, api_key_hash FROM users WHERE app_id = ? AND api_key_hash = ?")
        .bind(&[app_id.into(), hash.into()])
        .map_err(AppError::from)?
        .first::<serde_json::Value>(None)
        .await
        .map_err(AppError::from)?;
    Ok(row.filter(|r| stored_hash_matches(r, "api_key_hash", api_key)))
}

pub async fn authenticate(req: &Request, db: &D1Database) -> Result<AuthedUser, AppError> {
    let api_key = validate_api_key(req)?;
    let app_id = resolve_app_id(req);

    let mut row = None;
    for step in api_keys::lookup_steps() {
        row = match step {
            LookupStep::KeyTable => find_in_key_table(db, &app_id, &api_key).await?,
            LookupStep::LegacyHash => find_by_legacy_hash(db, &app_id, &api_key).await?,
        };
        if row.is_some() {
            break;
        }
    }

    let value = row.ok_or_else(|| AppError::Unauthorized("Invalid API key".to_string()))?;
    Ok(authed_user_from(&value, app_id))
}

/// Authenticates by API key like `authenticate`, then looks at `X-Midgar-Token`. A valid token is recorded on
/// the user; a bad or expired one is logged and ignored, so the request proceeds on its API key alone. A request
/// with a token and no valid API key is rejected exactly as before.
pub async fn authenticate_with_token(req: &Request, env: &Env, db: &D1Database) -> Result<AuthedUser, AppError> {
    let mut user = authenticate(req, db).await?;
    user.midgar = midgar_token::caller_from_request(req, env, &user.app_id).await;
    Ok(user)
}

/// Issues an additional key for an existing user and stores only its hash in
/// `api_keys`. Every other key of that user, on any device, keeps working. Used
/// wherever a login has to hand a key back and the database holds no plaintext.
pub async fn issue_api_key(db: &D1Database, app_id: &str, user_id: &str) -> Result<String, AppError> {
    let api_key = api_keys::generate_api_key();
    let now = chrono::Utc::now().to_rfc3339();
    db.prepare("INSERT INTO api_keys (key_hash, user_id, app_id, created_at) VALUES (?, ?, ?, ?)")
        .bind(&[api_keys::hash_api_key(&api_key).into(), user_id.into(), app_id.into(), now.into()])
        .map_err(AppError::from)?
        .run()
        .await
        .map_err(AppError::from)?;
    Ok(api_key)
}

/// Deletes up to `PRUNE_BATCH` keys whose last use, or creation when never
/// used, is older than `PRUNE_AFTER_DAYS`, oldest first, in one bounded
/// statement. Touches nothing but `api_keys`: a pruned device logs in again and
/// finds the same user and balance. Returns how many rows it deleted; no key
/// value is ever logged.
pub async fn prune_unused_api_keys(db: &D1Database) -> Result<u32, AppError> {
    let cutoff = api_keys::prune_cutoff(chrono::Utc::now());
    let result = db
        .prepare(
            "DELETE FROM api_keys WHERE key_hash IN (
                SELECT key_hash FROM api_keys
                WHERE COALESCE(last_used_at, created_at) < ?1
                ORDER BY COALESCE(last_used_at, created_at) LIMIT ?2
            )",
        )
        .bind(&[cutoff.into(), api_keys::PRUNE_BATCH.into()])
        .map_err(AppError::from)?
        .run()
        .await
        .map_err(AppError::from)?;
    Ok(result.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0) as u32)
}
