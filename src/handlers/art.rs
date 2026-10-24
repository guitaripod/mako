use serde_json::{json, Value};
use worker::{D1Database, Env, Response, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::Utc;
use uuid::Uuid;
use crate::art::{
    art_charge, art_hold, compose_prompt, has_style_field, ledger_reference, parse_art_request, ArtRequest,
    ART_MODEL, ART_QUALITY, ART_SIZE,
};
use crate::auth::AuthedUser;
use crate::chat_alias::key_replay_is_free;
use crate::ledgers::Ledgers;
use crate::error::AppError;
use crate::handlers::chat_alias::{remember_request, replays_of_key, RememberedRequest};
use crate::handlers::images_v2::{
    credit_check_failure_response, provider_failure_response, record_failed_request, FailedRequest,
};
use crate::providers::{self, UnifiedImageRequest};
use crate::rate_limit::{check_and_acquire_lock, release_lock};
use crate::tenant::TenantPolicy;

struct StyleRow {
    fragment: String,
    suffix: String,
}

/// Takes the request when it is an art request: it names a `style` and the tenant
/// has styles configured. Anything else, including every request from a tenant
/// without styles, returns None and continues down the unchanged image path.
pub async fn try_handle(env: &Env, auth: &AuthedUser, body_text: &str, start_time: u64) -> Option<Result<Response>> {
    let body: Value = serde_json::from_str(body_text).ok()?;
    if !has_style_field(&body) {
        return None;
    }
    let db = env.d1("DB").ok()?;
    if !tenant_has_styles(&db, &auth.app_id).await {
        return None;
    }
    Some(handle(env, &db, auth, &body, start_time).await)
}

async fn tenant_has_styles(db: &D1Database, app_id: &str) -> bool {
    let row = match db.prepare("SELECT 1 AS hit FROM image_styles WHERE app_id = ?1 LIMIT 1").bind(&[app_id.into()]) {
        Ok(stmt) => stmt.first::<Value>(None).await.ok().flatten(),
        Err(_) => None,
    };
    row.is_some()
}

async fn handle(env: &Env, db: &D1Database, auth: &AuthedUser, body: &Value, start_time: u64) -> Result<Response> {
    let policy = match TenantPolicy::load(db, &auth.app_id).await {
        Ok(p) => p,
        Err(e) => return e.to_response(),
    };
    let request = match parse_art_request(body) {
        Ok(r) => r,
        Err(message) => return AppError::BadRequest(message).to_response(),
    };
    let style = match load_style(db, &auth.app_id, &request.style).await {
        Ok(Some(s)) => s,
        Ok(None) => return AppError::BadRequest("Unknown style".to_string()).to_response(),
        Err(e) => return e.to_response(),
    };

    if check_and_acquire_lock(&auth.app_id, &auth.user_id, db).await.is_err() {
        return Response::error("Another request is already in progress", 429);
    }
    let outcome = run(env, db, auth, &policy, &request, &style, start_time).await;
    let _ = release_lock(&auth.app_id, &auth.user_id, db).await;
    outcome
}

async fn load_style(db: &D1Database, app_id: &str, style_id: &str) -> std::result::Result<Option<StyleRow>, AppError> {
    let row = db
        .prepare("SELECT fragment, suffix FROM image_styles WHERE app_id = ?1 AND style_id = ?2")
        .bind(&[app_id.into(), style_id.into()])?
        .first::<Value>(None)
        .await?;
    Ok(row.and_then(|r| {
        Some(StyleRow {
            fragment: r.get("fragment")?.as_str()?.to_string(),
            suffix: r.get("suffix").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
        })
    }))
}

async fn run(
    env: &Env,
    db: &D1Database,
    auth: &AuthedUser,
    policy: &TenantPolicy,
    request: &ArtRequest,
    style: &StyleRow,
    start_time: u64,
) -> Result<Response> {
    let reference = ledger_reference(&request.idempotency_key);
    let replays = match replays_of_key(db, &auth.app_id, &auth.user_id, &reference).await {
        Ok(r) => r,
        Err(e) => return e.to_response(),
    };
    let settled = key_replay_is_free(replays);

    let prompt = compose_prompt(&style.fragment, &style.suffix, &request.scene, request.mood.as_deref());
    let stored_prompt = policy.prompt_for_storage(&prompt);

    let provider = match providers::get_provider(ART_MODEL, env, policy) {
        Ok(p) => p,
        Err(e) => return AppError::from(e).to_response(),
    };
    let unified = UnifiedImageRequest {
        prompt: prompt.clone(),
        model: ART_MODEL.to_string(),
        n: Some(request.n),
        size: Some(ART_SIZE.to_string()),
        quality: Some(ART_QUALITY.to_string()),
        background: None,
        moderation: None,
        output_compression: None,
        output_format: None,
        partial_images: None,
        user: None,
    };

    let unit = match policy.art_credits {
        Some(credits) => credits,
        None => provider.estimate_cost(&UnifiedImageRequest { n: Some(1), ..unified.clone() }).credits,
    };
    let premium = !settled
        && unit > 0
        && crate::handlers::credits::is_premium_user(env, db, auth).await;

    let hold = art_hold(unit, request.n, premium, settled);
    if hold > 0 {
        if let Err(e) = Ledgers::new(env, db).check_and_reserve_credits(&auth.account, hold).await {
            return credit_check_failure_response(e, env, db, &auth.account, "image.generate").await;
        }
    }

    let provider_response = match provider.generate_image(&unified).await {
        Ok(response) => response,
        Err(e) => {
            let error_msg = e.to_string();
            record_failed_request(db, FailedRequest {
                app_id: &auth.app_id,
                user_id: &auth.user_id,
                request_type: "art",
                provider: provider.get_name(),
                model: ART_MODEL,
                prompt: stored_prompt,
                size: ART_SIZE,
                quality: ART_QUALITY,
                image_count: request.n,
                input_images_count: None,
                response_time_ms: (worker::Date::now().as_millis() - start_time) as u32,
                error: &error_msg,
            }).await;
            return provider_failure_response(&error_msg, "scene");
        }
    };

    let produced = provider_response.images.len() as u32;
    if produced == 0 {
        return AppError::InternalError("Provider returned no images".to_string()).to_response();
    }

    let charge = art_charge(unit, produced, request.n, premium, settled);
    if charge > 0 {
        let description = format!("Art ({}) x{}", request.style, produced);
        let key = crate::chat_alias::charge_key(&reference, replays.is_some(), &Uuid::new_v4().to_string());
        if let Err(e) = Ledgers::new(env, db).deduct_credits_keyed(&auth.account, charge, &description, &reference, &key).await {
            return AppError::from(e).to_response();
        }
    }

    remember_request(db, &RememberedRequest {
        app_id: &auth.app_id,
        user_id: &auth.user_id,
        reference: &reference,
        alias: "art",
        kind: art_kind(premium, settled, charge),
        retry_of: None,
        is_free_repeat: settled,
    }).await;

    record_usage(db, auth, provider.get_name(), stored_prompt, produced, &provider_response, start_time).await;

    let data: Vec<Value> = provider_response
        .images
        .iter()
        .map(|image| json!({ "b64_json": BASE64.encode(&image.data) }))
        .collect();
    Response::from_json(&json!({
        "created": Utc::now().timestamp(),
        "data": data,
        "size": ART_SIZE,
        "quality": ART_QUALITY,
        "output_format": "png",
        "style": request.style,
        "credits_charged": charge,
    }))
}

fn art_kind(premium: bool, settled: bool, charge: u32) -> &'static str {
    if settled {
        "repeat"
    } else if premium {
        "premium"
    } else if charge > 0 {
        "paid"
    } else {
        "free"
    }
}

/// Best-effort usage row without the prompt text unless the tenant keeps prompts.
async fn record_usage(
    db: &D1Database,
    auth: &AuthedUser,
    provider_name: &str,
    stored_prompt: &str,
    produced: u32,
    response: &providers::ProviderResponse,
    start_time: u64,
) {
    let usage = response.usage.as_ref();
    let statement = db
        .prepare(
            "INSERT INTO usage_records (id, app_id, user_id, request_type, provider, model, prompt, image_size, image_quality,
             image_count, input_images_count, total_tokens, input_tokens, output_tokens, text_tokens,
             image_tokens, r2_keys, response_time_ms, simplified_cost, error, created_at)
             VALUES (?, ?, ?, 'art', ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, '[]', ?, ?, NULL, ?)",
        )
        .bind(&[
            Uuid::new_v4().to_string().into(),
            auth.app_id.clone().into(),
            auth.user_id.clone().into(),
            provider_name.into(),
            ART_MODEL.into(),
            stored_prompt.into(),
            ART_SIZE.into(),
            ART_QUALITY.into(),
            produced.into(),
            usage.map(|u| u.total_tokens).unwrap_or(0).into(),
            usage.map(|u| u.input_tokens).unwrap_or(0).into(),
            usage.map(|u| u.output_tokens).unwrap_or(0).into(),
            usage.map(|u| u.input_tokens_details.text_tokens).unwrap_or(0).into(),
            usage.map(|u| u.input_tokens_details.image_tokens).unwrap_or(0).into(),
            ((worker::Date::now().as_millis() - start_time) as u32).into(),
            (provider_name == "gemini").into(),
            Utc::now().to_rfc3339().into(),
        ]);
    match statement {
        Ok(stmt) => {
            if let Err(e) = stmt.run().await {
                worker::console_log!("art usage insert failed: {:?}", e);
            }
        }
        Err(e) => worker::console_log!("art usage prepare failed: {:?}", e),
    }
}
