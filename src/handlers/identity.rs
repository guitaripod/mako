use worker::{Request, Response, RouteContext, Result, console_log, D1Database, Fetch, Method, Headers, RequestInit, RateLimiter};
use crate::error::AppError;
use crate::auth::{resolve_app_id, authenticate};
use crate::credits::{initialize_user_credits, add_credits, get_user_balance};
use crate::handlers::oauth_native::validate_apple_identity_token;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use chrono::Utc;

#[cfg(not(target_os = "windows"))]
use jwt_simple::prelude::*;

const ANON_PROVIDER: &str = "anonymous";
const DEVICECHECK_HOST_PROD: &str = "https://api.devicecheck.apple.com";
const DEVICECHECK_HOST_DEV: &str = "https://api.development.devicecheck.apple.com";

#[derive(Debug, Serialize)]
pub struct IdentityResponse {
    pub api_key: String,
    pub user_id: String,
}

#[derive(Debug, Deserialize)]
pub struct AnonymousRegisterRequest {
    /// Apple DeviceCheck token (DCDevice.generateToken) — proves a genuine device.
    pub device_token: Option<String>,
    /// Stable per-app device id persisted in the Keychain — the per-app idempotency key.
    pub client_device_id: Option<String>,
    /// Non-production simulator/dev fallback only.
    pub device_id: Option<String>,
    pub unverified: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct LinkRequest {
    pub identity_token: String,
}

pub async fn anonymous_register(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    match anonymous_register_inner(req, ctx).await {
        Ok(response) => Ok(response),
        Err(e) => e.to_response(),
    }
}

/// Per-IP rate gate for anonymous registration, backed by the native Workers
/// Rate Limiting binding `ANON_REGISTER_LIMITER`. Fails OPEN: a missing binding,
/// missing `CF-Connecting-IP`, or any `.limit()` error allows the request (logged,
/// never silent) so an infra/config fault cannot hard-block legitimate users.
/// Only an explicit `success == false` rejects with HTTP 429.
async fn enforce_anon_register_rate_limit(
    req: &Request,
    ctx: &RouteContext<()>,
) -> std::result::Result<(), AppError> {
    let limiter = match ctx.env.get_binding::<RateLimiter>("ANON_REGISTER_LIMITER") {
        Ok(limiter) => limiter,
        Err(e) => {
            console_log!("anon rate-limit binding unavailable (failing open): {:?}", e);
            return Ok(());
        }
    };

    let ip = match req.headers().get("CF-Connecting-IP") {
        Ok(Some(ip)) if !ip.is_empty() => ip,
        _ => return Ok(()),
    };

    match limiter.limit(format!("anon_register:{}", ip)).await {
        Ok(outcome) if !outcome.success => Err(AppError::RateLimitExceeded),
        Ok(_) => Ok(()),
        Err(e) => {
            console_log!("anon rate-limit check errored (failing open): {:?}", e);
            Ok(())
        }
    }
}

async fn anonymous_register_inner(
    mut req: Request,
    ctx: RouteContext<()>,
) -> std::result::Result<Response, AppError> {
    enforce_anon_register_rate_limit(&req, &ctx).await?;

    let app_id = resolve_app_id(&req);
    let body: AnonymousRegisterRequest = req
        .json()
        .await
        .map_err(|_| AppError::BadRequest("Invalid request body".to_string()))?;

    let db = ctx.env.d1("DB")?;

    if let Some(device_token) = body.device_token.as_ref().filter(|t| !t.is_empty()) {
        let client_device_id = body
            .client_device_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::BadRequest("Missing client_device_id".to_string()))?;
        return register_with_devicecheck(&ctx, &db, &app_id, device_token, client_device_id).await;
    }

    if body.unverified == Some(true) {
        let device_id = verify_unverified(&ctx, body.device_id.as_deref())?;
        let (user_id, api_key) = create_or_reuse_anonymous(&db, &app_id, &device_id).await?;
        return Response::from_json(&IdentityResponse { api_key, user_id }).map_err(AppError::from);
    }

    Err(AppError::BadRequest("Missing device_token or unverified device_id".to_string()))
}

#[cfg(target_os = "windows")]
async fn register_with_devicecheck(
    _ctx: &RouteContext<()>,
    _db: &D1Database,
    _app_id: &str,
    _device_token: &str,
    _client_device_id: &str,
) -> std::result::Result<Response, AppError> {
    Err(AppError::InternalError("DeviceCheck is not supported on Windows servers".to_string()))
}

#[cfg(not(target_os = "windows"))]
async fn register_with_devicecheck(
    ctx: &RouteContext<()>,
    db: &D1Database,
    app_id: &str,
    device_token: &str,
    client_device_id: &str,
) -> std::result::Result<Response, AppError> {
    let cfg = DeviceCheckConfig::load(ctx)?;
    let jwt = cfg.sign_jwt()?;

    // Gate: prove this request comes from a genuine Apple device (blocks
    // emulator/script farming). Apple performs the verification server-side.
    cfg.validate_device(&jwt, device_token).await?;

    // Per-app, per-device idempotent grant. The unique index
    // (app_id, provider, provider_id) makes first-insert-wins atomic, so a real
    // device gets the free trial exactly once per app regardless of concurrency.
    let (user_id, api_key) = create_or_reuse_anonymous(db, app_id, client_device_id).await?;
    Response::from_json(&IdentityResponse { api_key, user_id }).map_err(AppError::from)
}

#[cfg(not(target_os = "windows"))]
struct DeviceCheckConfig {
    bases: Vec<&'static str>,
    team_id: String,
    key_id: String,
    private_key: String,
}

#[cfg(not(target_os = "windows"))]
impl DeviceCheckConfig {
    fn load(ctx: &RouteContext<()>) -> std::result::Result<Self, AppError> {
        let team_id = ctx
            .env
            .var("APPLE_TEAM_ID")
            .map(|v| v.to_string())
            .map_err(|_| AppError::InternalError("APPLE_TEAM_ID not configured".to_string()))?;

        let key_id = ctx
            .env
            .var("DEVICECHECK_KEY_ID")
            .map(|v| v.to_string())
            .or_else(|_| ctx.env.secret("DEVICECHECK_KEY_ID").map(|v| v.to_string()))
            .map_err(|_| AppError::InternalError("DEVICECHECK_KEY_ID not configured".to_string()))?;

        let private_key = ctx
            .env
            .secret("DEVICECHECK_PRIVATE_KEY")
            .map(|v| v.to_string())
            .map_err(|_| AppError::InternalError("DEVICECHECK_PRIVATE_KEY not configured".to_string()))?;

        let bases = Self::ordered_bases(ctx);

        Ok(Self { bases, team_id, key_id, private_key })
    }

    /// Both DeviceCheck hosts to try, ordered by likely match. A DCDevice token
    /// validates against exactly one Apple environment — development for
    /// Xcode/TestFlight-internal builds, production for App Store builds — and the
    /// server cannot tell which from the token. So we ALWAYS try both hosts;
    /// `ENVIRONMENT` only chooses which to try first to save a round-trip in the
    /// common case. Correctness is order-independent. This removes the need to
    /// flip a `DEVICECHECK_ENV` flag when shipping.
    fn ordered_bases(ctx: &RouteContext<()>) -> Vec<&'static str> {
        let environment = ctx
            .env
            .var("ENVIRONMENT")
            .map(|v| v.to_string())
            .unwrap_or_else(|_| "development".to_string());

        if environment == "production" {
            vec![DEVICECHECK_HOST_PROD, DEVICECHECK_HOST_DEV]
        } else {
            vec![DEVICECHECK_HOST_DEV, DEVICECHECK_HOST_PROD]
        }
    }

    fn sign_jwt(&self) -> std::result::Result<String, AppError> {
        let claims = Claims::create(Duration::from_mins(10)).with_issuer(&self.team_id);

        let key_pair = ES256KeyPair::from_pem(&self.private_key)
            .map_err(|e| AppError::InternalError(format!("Failed to parse DeviceCheck key: {}", e)))?
            .with_key_id(&self.key_id);

        key_pair
            .sign(claims)
            .map_err(|e| AppError::InternalError(format!("Failed to sign DeviceCheck JWT: {}", e)))
    }

    fn request_body(&self, device_token: &str) -> Value {
        json!({
            "device_token": device_token,
            "transaction_id": Uuid::new_v4().to_string(),
            "timestamp": Utc::now().timestamp_millis(),
        })
    }

    async fn post(&self, base: &str, jwt: &str, path: &str, body: &Value) -> std::result::Result<Response, AppError> {
        let url = format!("{}{}", base, path);
        let headers = Headers::new();
        headers.set("Authorization", &format!("Bearer {}", jwt))?;
        headers.set("Content-Type", "application/json")?;

        let body_str = body.to_string();
        let request = Request::new_with_init(
            &url,
            RequestInit::new()
                .with_method(Method::Post)
                .with_headers(headers)
                .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body_str))),
        )?;

        Fetch::Request(request)
            .send()
            .await
            .map_err(AppError::from)
    }

    /// Confirm the DeviceCheck token is genuine by trying BOTH Apple environments.
    /// A token is valid for exactly one environment, so the wrong host returns a
    /// non-2xx while the right host returns 2xx. Success = any host returns 2xx.
    /// A forged/expired token gets a non-2xx from BOTH hosts => 400 (reject, no
    /// grant). If Apple is never reached on any host (transport error or 429
    /// throttle), the device is NOT at fault => 500 so the client retries, rather
    /// than a permanent 400 deny of a legitimate paying device.
    async fn validate_device(&self, jwt: &str, device_token: &str) -> std::result::Result<(), AppError> {
        let mut saw_http_response = false;
        let mut last_status = 0u16;

        for base in &self.bases {
            let body = self.request_body(device_token);
            let mut resp = match self.post(base, jwt, "/v1/validate_device_token", &body).await {
                Ok(resp) => resp,
                Err(e) => {
                    console_log!("DeviceCheck transport error on {}: {:?}", base, e);
                    continue;
                }
            };

            let status = resp.status_code();
            if (200..300).contains(&status) {
                return Ok(());
            }
            if status == 429 {
                console_log!("DeviceCheck throttled (429) on {}", base);
                continue;
            }

            saw_http_response = true;
            last_status = status;
            let detail = resp.text().await.unwrap_or_default();
            console_log!("DeviceCheck non-2xx on {}: {}", base, crate::redact::provider_error_summary(status, &detail));
        }

        if !saw_http_response {
            return Err(AppError::InternalError("device validation unavailable".to_string()));
        }
        Err(AppError::BadRequest(format!("device validation failed (last status {})", last_status)))
    }
}

fn verify_unverified(ctx: &RouteContext<()>, device_id: Option<&str>) -> std::result::Result<String, AppError> {
    let environment = ctx
        .env
        .var("ENVIRONMENT")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "production".to_string());
    if environment == "production" {
        return Err(AppError::BadRequest("unverified registration not allowed".to_string()));
    }
    let device_id = device_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing device_id".to_string()))?;
    Ok(device_id.to_string())
}

async fn fetch_apple_app_identity(db: &D1Database, app_id: &str) -> std::result::Result<(String, String), AppError> {
    let row = db
        .prepare("SELECT apple_team_id, apple_app_bundle_id FROM apps WHERE app_id = ?")
        .bind(&[app_id.into()])?
        .first::<Value>(None)
        .await?
        .ok_or_else(|| AppError::BadRequest("Unknown app".to_string()))?;

    let team_id = row
        .get("apple_team_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("App missing apple_team_id".to_string()))?
        .to_string();
    let bundle_id = row
        .get("apple_app_bundle_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("App missing apple_app_bundle_id".to_string()))?
        .to_string();

    Ok((team_id, bundle_id))
}

/// Create the anonymous user for `device_key`, or return the existing one.
/// Idempotency is enforced atomically by the unique index
/// (app_id, provider, provider_id) via ON CONFLICT DO NOTHING: the free trial is
/// granted only on the row that is actually inserted, so concurrent or repeated
/// calls for the same device never double-grant.
async fn create_or_reuse_anonymous(
    db: &D1Database,
    app_id: &str,
    device_key: &str,
) -> std::result::Result<(String, String), AppError> {
    let new_user_id = Uuid::new_v4().to_string();
    let new_api_key = format!("pixie_{}", Uuid::new_v4().to_string().replace("-", ""));
    let now = Utc::now().to_rfc3339();

    let result = db
        .prepare(
            "INSERT INTO users (id, app_id, email, provider, provider_id, name, api_key, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(app_id, provider, provider_id) DO NOTHING",
        )
        .bind(&[
            new_user_id.clone().into(),
            app_id.into(),
            worker::wasm_bindgen::JsValue::NULL,
            ANON_PROVIDER.into(),
            device_key.into(),
            "Anonymous".into(),
            new_api_key.clone().into(),
            now.clone().into(),
            now.into(),
        ])?
        .run()
        .await?;

    let inserted = result.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0) > 0;

    if inserted {
        initialize_user_credits(app_id, &new_user_id, db).await?;
        console_log!("Created anonymous user: {}", new_user_id);
        return Ok((new_user_id, new_api_key));
    }

    let existing = db
        .prepare("SELECT id, api_key FROM users WHERE app_id = ? AND provider = ? AND provider_id = ?")
        .bind(&[app_id.into(), ANON_PROVIDER.into(), device_key.into()])?
        .first::<Value>(None)
        .await?
        .ok_or_else(|| AppError::InternalError("Anonymous user lookup failed".to_string()))?;

    let user_id = existing.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let api_key = existing.get("api_key").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if user_id.is_empty() || api_key.is_empty() {
        return Err(AppError::InternalError("Anonymous user lookup failed".to_string()));
    }
    console_log!("Reusing anonymous user: {}", user_id);
    Ok((user_id, api_key))
}

pub async fn link(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    match link_inner(req, ctx).await {
        Ok(response) => Ok(response),
        Err(e) => e.to_response(),
    }
}

async fn link_inner(
    mut req: Request,
    ctx: RouteContext<()>,
) -> std::result::Result<Response, AppError> {
    let app_id = resolve_app_id(&req);
    let db = ctx.env.d1("DB")?;

    let anon = authenticate(&req, &db).await?;

    let body: LinkRequest = req
        .json()
        .await
        .map_err(|_| AppError::BadRequest("Invalid request body".to_string()))?;

    let (_, bundle_id) = fetch_apple_app_identity(&db, &app_id).await?;
    let claims = validate_apple_identity_token(&body.identity_token, &bundle_id).await?;

    let existing_apple = db
        .prepare("SELECT id, api_key FROM users WHERE app_id = ? AND provider = ? AND provider_id = ?")
        .bind(&[app_id.clone().into(), "apple".into(), claims.sub.clone().into()])?
        .first::<Value>(None)
        .await?;

    if let Some(apple_user) = existing_apple {
        let existing_id = apple_user.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let existing_api_key = apple_user.get("api_key").and_then(|v| v.as_str()).unwrap_or("").to_string();

        if existing_id == anon.user_id {
            return Response::from_json(&IdentityResponse {
                api_key: existing_api_key,
                user_id: existing_id,
            })
            .map_err(AppError::from);
        }

        let anon_balance = drain_wallet(&db, &app_id, &anon.user_id).await?;
        if anon_balance > 0 {
            add_credits(
                &app_id,
                &existing_id,
                anon_balance,
                "admin_adjustment",
                "merge from anonymous",
                Some(&anon.user_id),
                &db,
            )
            .await?;
        }

        crate::attribution::carry_attribution_across_merge(&db, &app_id, &anon.user_id, &existing_id).await?;

        console_log!("Merged anonymous {} into apple user {}", anon.user_id, existing_id);
        return Response::from_json(&IdentityResponse {
            api_key: existing_api_key,
            user_id: existing_id,
        })
        .map_err(AppError::from);
    }

    relink_anonymous_to_apple(&db, &app_id, &anon.user_id, &claims).await?;

    let api_key = db
        .prepare("SELECT api_key FROM users WHERE app_id = ? AND id = ?")
        .bind(&[app_id.clone().into(), anon.user_id.clone().into()])?
        .first::<Value>(None)
        .await?
        .and_then(|v| v.get("api_key").and_then(|k| k.as_str()).map(|s| s.to_string()))
        .ok_or_else(|| AppError::InternalError("User vanished during link".to_string()))?;

    console_log!("Relinked anonymous {} to apple", anon.user_id);
    Response::from_json(&IdentityResponse {
        api_key,
        user_id: anon.user_id,
    })
    .map_err(AppError::from)
}

async fn relink_anonymous_to_apple(
    db: &D1Database,
    app_id: &str,
    user_id: &str,
    claims: &crate::handlers::oauth_native::AppleIdTokenClaims,
) -> std::result::Result<(), AppError> {
    let now = Utc::now().to_rfc3339();
    let email = claims
        .email
        .clone()
        .unwrap_or_else(|| format!("{}@privaterelay.appleid.com", claims.sub.chars().take(8).collect::<String>()));

    let result = db
        .prepare("UPDATE users SET provider = ?, provider_id = ?, email = ?, name = ?, updated_at = ? WHERE app_id = ? AND id = ? AND provider = ?")
        .bind(&[
            "apple".into(),
            claims.sub.clone().into(),
            email.into(),
            "Apple User".into(),
            now.into(),
            app_id.into(),
            user_id.into(),
            ANON_PROVIDER.into(),
        ])?
        .run()
        .await?;

    let relinked = result.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0) > 0;
    if !relinked {
        return Err(AppError::Conflict(
            "This account is already linked to a different Apple ID.".to_string(),
        ));
    }

    Ok(())
}

/// Every table with a `user_id` column that account deletion empties with
/// `DELETE ... WHERE user_id = ?`, in the order it runs. `stored_images` is not
/// listed because `purge_user_images` removes it together with its R2 objects and
/// reports; the `users` row goes last. Children come first because
/// `usage_records`, `device_auth_flows`, `credit_transactions` and
/// `credit_purchases` hold a foreign key to `users(id)`, which D1 enforces: deleting
/// the user while any of them still has a row makes the whole deletion fail. The
/// test `every_user_table_is_deleted_with_the_account` derives the tables with a
/// `user_id` column from the migrations and fails when one is missing here.
const USER_DATA_TABLES: &[&str] = &[
    "usage_records",
    "paywall_events",
    "device_auth_flows",
    "credit_transactions",
    "credit_purchases",
    "credit_refunds",
    "user_credits",
    "user_locks",
    "realtime_sessions",
    "shared_playlists",
    "install_attributions",
    "purchase_attributions",
    "weekly_usage",
    "chat_requests",
];

/// Delete the authenticated user's account (App Store 5.1.1(v)): removes every
/// table in `USER_DATA_TABLES` (prompts and usage history, paywall events, credit
/// records, ad attribution, weekly counters), all of the user's generated images
/// (R2 objects + rows + reports) so nothing of theirs lingers in the public gallery
/// feed, and finally the user row. Scoped to the api-key's own user_id, so a
/// caller can only ever delete their own account.
pub async fn delete_identity(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let db = ctx.env.d1("DB")?;
    let auth = match authenticate(&req, &db).await {
        Ok(a) => a,
        Err(e) => return e.to_response(),
    };
    let uid = auth.user_id;

    purge_user_images(&ctx.env, &db, &uid).await?;

    for table in USER_DATA_TABLES {
        db.prepare(&format!("DELETE FROM {} WHERE user_id = ?", table))
            .bind(&[uid.clone().into()])?
            .run()
            .await?;
    }
    db.prepare("DELETE FROM users WHERE id = ?")
        .bind(&[uid.into()])?
        .run()
        .await?;
    Response::from_json(&json!({ "deleted": true }))
}

/// Removes every image owned by `uid`: deletes each R2 object (best-effort, a
/// failed object delete never blocks account deletion), the reports the user
/// filed, and the stored_images rows.
async fn purge_user_images(env: &worker::Env, db: &D1Database, uid: &str) -> std::result::Result<(), AppError> {
    let rows = db
        .prepare("SELECT r2_key FROM stored_images WHERE user_id = ?")
        .bind(&[uid.into()])?
        .all()
        .await?;

    if let (Ok(bucket), Ok(results)) = (env.bucket("IMAGES"), rows.results::<Value>()) {
        for row in results {
            if let Some(r2_key) = row.get("r2_key").and_then(|v| v.as_str()) {
                if let Err(e) = bucket.delete(r2_key).await {
                    console_log!("Failed to delete R2 object {} during account deletion: {:?}", r2_key, e);
                }
            }
        }
    }

    db.prepare("DELETE FROM image_reports WHERE reporter_user_id = ?")
        .bind(&[uid.into()])?
        .run()
        .await?;
    db.prepare("DELETE FROM image_reports WHERE image_id IN (SELECT id FROM stored_images WHERE user_id = ?)")
        .bind(&[uid.into()])?
        .run()
        .await?;
    db.prepare("DELETE FROM stored_images WHERE user_id = ?")
        .bind(&[uid.into()])?
        .run()
        .await?;

    Ok(())
}

/// Returns the authenticated user's identity. Used by sibling workers (e.g.
/// payday-worker) to validate a mako api-key and obtain the stable user id
/// without sharing mako's database.
pub async fn identity_me(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let db = ctx.env.d1("DB")?;
    let auth = match authenticate(&req, &db).await {
        Ok(a) => a,
        Err(e) => return e.to_response(),
    };
    Response::from_json(&json!({ "user_id": auth.user_id, "app_id": auth.app_id }))
}

/// Charges a flat-cost capability (e.g. `peppol.send`) against the authenticated
/// user's balance — for non-AI metered actions a sibling worker performs.
/// Returns 402 if the balance is insufficient.
pub async fn charge_capability(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let db = ctx.env.d1("DB")?;
    let auth = match authenticate(&req, &db).await {
        Ok(a) => a,
        Err(e) => return e.to_response(),
    };
    let body: Value = match req.json().await {
        Ok(b) => b,
        Err(e) => return AppError::BadRequest(format!("Invalid request body: {}", e)).to_response(),
    };
    let capability = body.get("capability").and_then(|v| v.as_str()).unwrap_or("");
    if capability.is_empty() {
        return AppError::BadRequest("capability is required".to_string()).to_response();
    }
    let cost = crate::credits::get_flat_capability_cost(&auth.app_id, capability, &db)
        .await
        .unwrap_or(0);
    if cost == 0 {
        return AppError::BadRequest(format!("Unknown or free capability: {}", capability)).to_response();
    }
    let reference = body.get("reference").and_then(|v| v.as_str()).unwrap_or(capability);
    match crate::credits::deduct_credits(&auth.app_id, &auth.user_id, cost, capability, reference, &db).await {
        Ok(balance) => Response::from_json(&json!({ "charged": cost, "balance": balance })),
        Err(_) => AppError::PaymentRequired(format!("Insufficient credits for {}", capability)).to_response(),
    }
}

/// Atomically zeroes `user_id`'s wallet and returns exactly what was drained.
/// Retries on contention (a refund or a settle landing on the anonymous
/// wallet mid-merge) so the amount later credited to the linked account is
/// always exactly what left this one — never a stale snapshot that a
/// concurrent mutation is silently overwritten by. Bounded like
/// `deduct_credits_clamped`'s CAS loop, so pathological contention fails loud
/// instead of retrying forever.
async fn drain_wallet(db: &D1Database, app_id: &str, user_id: &str) -> std::result::Result<u32, AppError> {
    for _ in 0..8 {
        let current = get_user_balance(app_id, user_id, db).await?;
        if current <= 0 {
            return Ok(0);
        }
        let now = Utc::now().to_rfc3339();
        let claimed = db
            .prepare(
                "UPDATE user_credits SET balance = 0, updated_at = ?
                 WHERE app_id = ? AND user_id = ? AND balance = ?
                 RETURNING balance",
            )
            .bind(&[now.into(), app_id.into(), user_id.into(), current.into()])?
            .first::<Value>(None)
            .await?;
        if claimed.is_some() {
            return Ok(current as u32);
        }
    }
    Err(AppError::InternalError("drain_wallet: too much contention".to_string()))
}

#[cfg(test)]
mod tests {
    use super::USER_DATA_TABLES;
    use std::collections::{BTreeMap, BTreeSet};

    const IMAGE_TABLE: &str = "stored_images";

    fn migration_sql() -> String {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        names.sort();
        names
            .iter()
            .map(|n| std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations").join(n)).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn strip_comments(sql: &str) -> String {
        sql.lines()
            .map(|l| l.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn top_level_items(body: &str) -> Vec<String> {
        let mut items = Vec::new();
        let mut depth = 0;
        let mut current = String::new();
        for c in body.chars() {
            match c {
                '(' => { depth += 1; current.push(c); }
                ')' => { depth -= 1; current.push(c); }
                ',' if depth == 0 => { items.push(std::mem::take(&mut current)); }
                _ => current.push(c),
            }
        }
        items.push(current);
        items
    }

    fn name_after(words: &[&str], skip: &[&str]) -> String {
        words
            .iter()
            .skip_while(|w| skip.contains(&w.to_ascii_uppercase().as_str()))
            .next()
            .unwrap()
            .trim_matches(|c| c == '"' || c == '`')
            .to_string()
    }

    struct Schema {
        columns: BTreeMap<String, BTreeSet<String>>,
        references_users: BTreeSet<String>,
    }

    fn replay_migrations() -> Schema {
        let sql = strip_comments(&migration_sql());
        let mut columns: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut references_users = BTreeSet::new();
        for statement in sql.split(';') {
            let text = statement.trim();
            let words: Vec<&str> = text.split_whitespace().collect();
            let upper = text.to_ascii_uppercase();
            if upper.starts_with("CREATE TABLE") {
                let rest: Vec<&str> = words[2..].to_vec();
                let name = name_after(&rest, &["IF", "NOT", "EXISTS"]);
                let name = name.split('(').next().unwrap().to_string();
                let open = text.find('(').unwrap();
                let close = text.rfind(')').unwrap();
                let mut cols = BTreeSet::new();
                for item in top_level_items(&text[open + 1..close]) {
                    let first = item.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
                    if matches!(first.as_str(), "PRIMARY" | "FOREIGN" | "UNIQUE" | "CHECK" | "CONSTRAINT" | "") {
                        continue;
                    }
                    cols.insert(item.split_whitespace().next().unwrap().to_ascii_lowercase());
                }
                if upper.contains("REFERENCES USERS") {
                    references_users.insert(name.clone());
                }
                columns.insert(name, cols);
            } else if upper.starts_with("DROP TABLE") {
                let rest: Vec<&str> = words[2..].to_vec();
                let name = name_after(&rest, &["IF", "EXISTS"]);
                columns.remove(&name);
                references_users.remove(&name);
            } else if upper.starts_with("ALTER TABLE") {
                let table = words[2].to_string();
                if upper.contains(" RENAME TO ") {
                    let target = words[words.len() - 1].to_string();
                    if let Some(cols) = columns.remove(&table) {
                        columns.insert(target.clone(), cols);
                    }
                    if references_users.remove(&table) {
                        references_users.insert(target);
                    }
                } else if upper.contains(" ADD COLUMN ") {
                    columns.entry(table).or_default().insert(words[5].to_ascii_lowercase());
                }
            }
        }
        Schema { columns, references_users }
    }

    fn covered_tables() -> BTreeSet<String> {
        USER_DATA_TABLES.iter().map(|t| t.to_string()).chain([IMAGE_TABLE.to_string()]).collect()
    }

    #[test]
    fn every_user_table_is_deleted_with_the_account() {
        let schema = replay_migrations();
        let covered = covered_tables();
        let missing: Vec<_> = schema
            .columns
            .iter()
            .filter(|(_, cols)| cols.contains("user_id"))
            .map(|(name, _)| name.clone())
            .filter(|name| !covered.contains(name))
            .collect();
        assert!(missing.is_empty(), "tables with a user_id column that account deletion skips: {:?}", missing);
    }

    #[test]
    fn every_listed_table_exists_and_has_a_user_id_column() {
        let schema = replay_migrations();
        for table in USER_DATA_TABLES {
            let cols = schema.columns.get(*table).unwrap_or_else(|| panic!("{} is not created by any migration", table));
            assert!(cols.contains("user_id"), "{} has no user_id column", table);
        }
    }

    #[test]
    fn tables_with_a_foreign_key_to_users_are_deleted_before_the_user() {
        let schema = replay_migrations();
        let covered = covered_tables();
        for table in &schema.references_users {
            assert!(covered.contains(table), "{} references users(id) and would block the user delete", table);
        }
        assert!(!USER_DATA_TABLES.contains(&"users"));
    }

    #[test]
    fn the_migration_replay_understands_the_rebuilds() {
        let schema = replay_migrations();
        assert!(schema.columns["user_credits"].contains("user_id"));
        assert!(schema.columns["user_credits"].contains("app_id"));
        assert!(schema.columns["users"].contains("api_key"));
        assert!(!schema.columns.contains_key("user_credits_new"));
        assert!(schema.columns["usage_records"].contains("capability"));
    }
}
