use worker::{D1Database, Request, Response, RouteContext, Result};
use crate::models::{ImageGenerationRequest, ImageEditRequest, ImageResponse, ImageData, UsageRecord, ErrorResponse, ErrorDetail};
use crate::error::AppError;
use crate::auth;
use crate::storage::store_image_from_bytes;
use crate::credits::{check_and_reserve_credits, deduct_credits, get_flat_capability_cost, get_user_balance, record_paywall_event};
use crate::rate_limit::{check_and_acquire_lock, release_lock};
use crate::providers::{self, UnifiedImageRequest, UnifiedEditRequest};
use crate::{log_debug, log_error};
use serde_json::json;
use uuid::Uuid;
use chrono::Utc;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

pub async fn handle_generation(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let start_time = worker::Date::now().as_millis();
    let env = ctx.env;

    let auth = {
        let db = env.d1("DB")?;
        match auth::authenticate(&req, &db).await {
            Ok(a) => a,
            Err(e) => return e.to_response(),
        }
    };
    let user_id = auth.user_id.clone();
    let app_id = auth.app_id.clone();

    if let Err(e) = crate::rate_limit::enforce_write_rate_limit(&env, &app_id, &user_id, "image.generate").await {
        return e.to_response();
    }

    let generation_req: ImageGenerationRequest = match req.json().await {
        Ok(req) => req,
        Err(e) => return AppError::BadRequest(format!("Invalid request body: {}", e)).to_response(),
    };

    if generation_req.stream {
        return AppError::BadRequest("Streaming is not supported yet".to_string()).to_response();
    }
    
    let db = env.d1("DB")?;

    if let Err(_) = check_and_acquire_lock(&app_id, &user_id, &db).await {
        return Ok(Response::error("Another request is already in progress", 429)?);
    }

    let provider = match providers::get_provider(&generation_req.model, &env) {
        Ok(p) => p,
        Err(e) => {
            let _ = release_lock(&app_id, &user_id, &db).await;
            return Err(e);
        }
    };

    let unified_request = UnifiedImageRequest {
        prompt: generation_req.prompt.clone(),
        model: generation_req.model.clone(),
        n: Some(generation_req.n),
        size: Some(generation_req.size.clone()),
        quality: Some(generation_req.quality.clone()),
        background: Some(generation_req.background.clone()),
        moderation: generation_req.moderation.clone(),
        output_compression: generation_req.output_compression,
        output_format: Some(generation_req.output_format.clone()),
        partial_images: Some(generation_req.partial_images),
        user: generation_req.user.clone(),
        api_key: generation_req.openai_api_key.clone(),
    };

    let mut cost_estimate = provider.estimate_cost(&unified_request);
    if let Some(flat) = get_flat_capability_cost(&app_id, "image.generate", &db).await {
        cost_estimate.credits = flat;
    }

    if let Err(e) = check_and_reserve_credits(&app_id, &user_id, cost_estimate.credits, &db).await {
        let _ = release_lock(&app_id, &user_id, &db).await;
        return credit_check_failure_response(e, &db, &app_id, &user_id, "image.generate").await;
    }

    log_debug!("Sending request to provider", json!({
        "provider": provider.get_name(),
        "model": &generation_req.model,
        "prompt_length": generation_req.prompt.len(),
        "n": generation_req.n,
    }));

    let provider_response = match provider.generate_image(&unified_request).await {
        Ok(resp) => resp,
        Err(e) => {
            let _ = release_lock(&app_id, &user_id, &db).await;

            let error_msg = e.to_string();
            record_failed_request(&db, FailedRequest {
                app_id: &app_id,
                user_id: &user_id,
                request_type: "generation",
                provider: provider.get_name(),
                model: &generation_req.model,
                prompt: &generation_req.prompt,
                size: &generation_req.size,
                quality: &generation_req.quality,
                image_count: generation_req.n,
                input_images_count: None,
                response_time_ms: (worker::Date::now().as_millis() - start_time) as u32,
                error: &error_msg,
            }).await;
            return provider_failure_response(&error_msg, "prompt");
        }
    };

    let mut image_data_list = Vec::new();
    let mut r2_keys = Vec::new();
    let mut images_stored = 0;
    
    for (i, image_bytes) in provider_response.images.iter().enumerate() {
        let _base64_string = BASE64.encode(&image_bytes.data);
        
        match store_image_from_bytes(
            &env,
            &user_id,
            &image_bytes.data,
            &generation_req.prompt,
            &generation_req.model,
            &generation_req.size,
            Some(&generation_req.quality),
            provider.get_name(),
        ).await {
            Ok(stored_image) => {
                let db = env.d1("DB")?;

                let per_image_credits = cost_estimate.credits / generation_req.n as u32;
                let cost_cents = (cost_estimate.credits as f32 / 3.0) as i32;
                let is_public: i32 = if generation_req.is_public.unwrap_or(false) { 1 } else { 0 };

                let stmt = db.prepare(
                    "INSERT INTO stored_images (id, app_id, user_id, r2_key, prompt, provider, model, size, quality, created_at, expires_at, cost_cents, credits_charged, is_public)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                );

                let _ = stmt
                    .bind(&[
                        stored_image.id.clone().into(),
                        app_id.clone().into(),
                        stored_image.user_id.clone().into(),
                        stored_image.r2_key.clone().into(),
                        stored_image.prompt.clone().into(),
                        provider.get_name().into(),
                        stored_image.model.clone().into(),
                        stored_image.size.clone().into(),
                        generation_req.quality.clone().into(),
                        stored_image.created_at.to_rfc3339().into(),
                        stored_image.expires_at.to_rfc3339().into(),
                        cost_cents.into(),
                        per_image_credits.into(),
                        is_public.into(),
                    ])?
                    .run()
                    .await?;
                
                r2_keys.push(stored_image.r2_key.clone());
                
                let revised_prompt = provider_response.revised_prompts
                    .get(i)
                    .and_then(|p| p.clone());
                
                image_data_list.push(ImageData {
                    b64_json: None,
                    url: Some(stored_image.url.clone()),
                    revised_prompt,
                });
                
                images_stored += 1;
            }
            Err(e) => {
                log_error!("Failed to store image in R2", json!({
                    "error": e.to_string(),
                    "user_id": &user_id,
                    "prompt": &generation_req.prompt
                }));
            }
        }
    }
    
    if images_stored > 0 {
        let actual_credits_to_charge = (cost_estimate.credits * images_stored) / generation_req.n as u32;
        let description = format!("Generated {} image(s) using {}", images_stored, generation_req.model);
        
        if let Err(e) = deduct_credits(
            &app_id,
            &user_id,
            actual_credits_to_charge,
            &description,
            &r2_keys.join(","),
            &db
        ).await {
            log_error!("Failed to deduct credits", json!({
                "error": e.to_string(),
                "user_id": &user_id,
                "credits": actual_credits_to_charge,
                "images_stored": images_stored
            }));
        }
    }

    let response_time_ms = (worker::Date::now().as_millis() - start_time) as u32;
    let usage = provider_response.usage.as_ref();

    let _usage_record = UsageRecord {
        id: Uuid::new_v4().to_string(),
        user_id: user_id.clone(),
        request_type: "generation".to_string(),
        model: generation_req.model.clone(),
        prompt: generation_req.prompt.clone(),
        image_size: generation_req.size.clone(),
        image_quality: generation_req.quality.clone(),
        image_count: generation_req.n,
        input_images_count: None,
        total_tokens: usage.map(|u| u.total_tokens).unwrap_or(0),
        input_tokens: usage.map(|u| u.input_tokens).unwrap_or(0),
        output_tokens: usage.map(|u| u.output_tokens).unwrap_or(0),
        text_tokens: usage.map(|u| u.input_tokens_details.text_tokens).unwrap_or(0),
        image_tokens: usage.map(|u| u.input_tokens_details.image_tokens).unwrap_or(0),
        r2_keys,
        response_time_ms,
        error: None,
        created_at: Utc::now(),
    };

    let db = env.d1("DB")?;
    let stmt = db.prepare(
        "INSERT INTO usage_records (id, app_id, user_id, request_type, provider, model, prompt, image_size, image_quality,
         image_count, input_images_count, total_tokens, input_tokens, output_tokens, text_tokens,
         image_tokens, r2_keys, response_time_ms, simplified_cost, error, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    );

    let simplified_cost = provider.get_name() == "gemini";

    let _ = stmt
        .bind(&[
            _usage_record.id.into(),
            app_id.clone().into(),
            _usage_record.user_id.into(),
            _usage_record.request_type.into(),
            provider.get_name().into(),
            _usage_record.model.into(),
            _usage_record.prompt.into(),
            _usage_record.image_size.into(),
            _usage_record.image_quality.into(),
            _usage_record.image_count.into(),
            _usage_record.input_images_count.map(|n| n.into()).unwrap_or(worker::wasm_bindgen::JsValue::NULL),
            _usage_record.total_tokens.into(),
            _usage_record.input_tokens.into(),
            _usage_record.output_tokens.into(),
            _usage_record.text_tokens.into(),
            _usage_record.image_tokens.into(),
            serde_json::to_string(&_usage_record.r2_keys).unwrap().into(),
            _usage_record.response_time_ms.into(),
            simplified_cost.into(),
            worker::wasm_bindgen::JsValue::NULL,
            _usage_record.created_at.to_rfc3339().into(),
        ])?
        .run()
        .await?;

    let _ = release_lock(&app_id, &user_id, &db).await;

    let response = ImageResponse {
        created: Utc::now().timestamp() as u64,
        data: image_data_list,
        background: if provider.get_name() == "openai" { Some(generation_req.background) } else { None },
        output_format: Some(generation_req.output_format),
        size: Some(generation_req.size),
        quality: if provider.get_name() == "openai" { Some(generation_req.quality) } else { None },
        usage: provider_response.usage,
    };
    
    Response::from_json(&response)
}

pub async fn handle_edit(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let start_time = worker::Date::now().as_millis();
    let env = ctx.env;

    let auth = {
        let db = env.d1("DB")?;
        match auth::authenticate(&req, &db).await {
            Ok(a) => a,
            Err(e) => return e.to_response(),
        }
    };
    let user_id = auth.user_id.clone();
    let app_id = auth.app_id.clone();

    if let Err(e) = crate::rate_limit::enforce_write_rate_limit(&env, &app_id, &user_id, "image.edit").await {
        return e.to_response();
    }

    let edit_req: ImageEditRequest = match req.json().await {
        Ok(req) => req,
        Err(e) => return AppError::BadRequest(format!("Invalid request body: {}", e)).to_response(),
    };

    if edit_req.stream {
        return AppError::BadRequest("Streaming is not supported yet".to_string()).to_response();
    }
    
    let db = env.d1("DB")?;

    if let Err(_) = check_and_acquire_lock(&app_id, &user_id, &db).await {
        return Ok(Response::error("Another request is already in progress", 429)?);
    }

    let provider = match providers::get_provider(&edit_req.model, &env) {
        Ok(p) => p,
        Err(e) => {
            let _ = release_lock(&app_id, &user_id, &db).await;
            return Err(e);
        }
    };

    if !provider.get_supported_features().supports_edit {
        let _ = release_lock(&app_id, &user_id, &db).await;
        return AppError::BadRequest(format!("Model {} does not support image editing", edit_req.model)).to_response();
    }

    let unified_request = UnifiedEditRequest {
        image: edit_req.image.clone(),
        prompt: edit_req.prompt.clone(),
        mask: edit_req.mask.clone(),
        model: edit_req.model.clone(),
        n: Some(edit_req.n),
        size: Some(edit_req.size.clone()),
        quality: Some(edit_req.quality.clone()),
        background: Some(edit_req.background.clone()),
        input_fidelity: Some(edit_req.input_fidelity.clone()),
        output_compression: edit_req.output_compression,
        output_format: Some(edit_req.output_format.clone()),
        partial_images: Some(edit_req.partial_images),
        user: edit_req.user.clone(),
        api_key: edit_req.openai_api_key.clone(),
    };

    let cost_estimate = provider.estimate_edit_cost(&unified_request);

    if let Err(e) = check_and_reserve_credits(&app_id, &user_id, cost_estimate.credits, &db).await {
        let _ = release_lock(&app_id, &user_id, &db).await;
        return credit_check_failure_response(e, &db, &app_id, &user_id, "image.edit").await;
    }

    log_debug!("Sending edit request to provider", json!({
        "provider": provider.get_name(),
        "model": &edit_req.model,
        "prompt_length": edit_req.prompt.len(),
        "n": edit_req.n,
    }));

    let provider_response = match provider.edit_image(&unified_request).await {
        Ok(resp) => resp,
        Err(e) => {
            let _ = release_lock(&app_id, &user_id, &db).await;

            let error_msg = e.to_string();
            record_failed_request(&db, FailedRequest {
                app_id: &app_id,
                user_id: &user_id,
                request_type: "edit",
                provider: provider.get_name(),
                model: &edit_req.model,
                prompt: &edit_req.prompt,
                size: &edit_req.size,
                quality: &edit_req.quality,
                image_count: edit_req.n,
                input_images_count: Some(edit_req.image.len() as u8),
                response_time_ms: (worker::Date::now().as_millis() - start_time) as u32,
                error: &error_msg,
            }).await;
            return provider_failure_response(&error_msg, "image");
        }
    };

    let mut image_data_list = Vec::new();
    let mut r2_keys = Vec::new();
    let mut images_stored = 0;
    
    for (i, image_bytes) in provider_response.images.iter().enumerate() {
        match store_image_from_bytes(
            &env,
            &user_id,
            &image_bytes.data,
            &edit_req.prompt,
            &edit_req.model,
            &edit_req.size,
            Some(&edit_req.quality),
            provider.get_name(),
        ).await {
            Ok(stored_image) => {
                let db = env.d1("DB")?;

                let per_image_credits = cost_estimate.credits / edit_req.n as u32;
                let cost_cents = (cost_estimate.credits as f32 / 3.0) as i32;
                let is_public: i32 = if edit_req.is_public.unwrap_or(false) { 1 } else { 0 };

                let stmt = db.prepare(
                    "INSERT INTO stored_images (id, app_id, user_id, r2_key, prompt, provider, model, size, quality, created_at, expires_at, cost_cents, credits_charged, is_public)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                );

                let _ = stmt
                    .bind(&[
                        stored_image.id.clone().into(),
                        app_id.clone().into(),
                        stored_image.user_id.clone().into(),
                        stored_image.r2_key.clone().into(),
                        stored_image.prompt.clone().into(),
                        provider.get_name().into(),
                        stored_image.model.clone().into(),
                        stored_image.size.clone().into(),
                        edit_req.quality.clone().into(),
                        stored_image.created_at.to_rfc3339().into(),
                        stored_image.expires_at.to_rfc3339().into(),
                        cost_cents.into(),
                        per_image_credits.into(),
                        is_public.into(),
                    ])?
                    .run()
                    .await?;
                
                r2_keys.push(stored_image.r2_key.clone());
                
                let revised_prompt = provider_response.revised_prompts
                    .get(i)
                    .and_then(|p| p.clone());
                
                image_data_list.push(ImageData {
                    b64_json: None,
                    url: Some(stored_image.url.clone()),
                    revised_prompt,
                });
                
                images_stored += 1;
            }
            Err(e) => {
                log_error!("Failed to store image in R2", json!({
                    "error": e.to_string(),
                    "user_id": &user_id,
                    "prompt": &edit_req.prompt
                }));
            }
        }
    }
    
    if images_stored > 0 {
        let actual_credits_to_charge = (cost_estimate.credits * images_stored) / edit_req.n as u32;
        let description = format!("Edited {} image(s) using {}", images_stored, edit_req.model);
        
        if let Err(e) = deduct_credits(
            &app_id,
            &user_id,
            actual_credits_to_charge,
            &description,
            &r2_keys.join(","),
            &db
        ).await {
            log_error!("Failed to deduct credits", json!({
                "error": e.to_string(),
                "user_id": &user_id,
                "credits": actual_credits_to_charge,
                "images_stored": images_stored
            }));
        }
    }

    let response_time_ms = (worker::Date::now().as_millis() - start_time) as u32;
    let usage = provider_response.usage.as_ref();

    let _usage_record = UsageRecord {
        id: Uuid::new_v4().to_string(),
        user_id: user_id.clone(),
        request_type: "edit".to_string(),
        model: edit_req.model.clone(),
        prompt: edit_req.prompt.clone(),
        image_size: edit_req.size.clone(),
        image_quality: edit_req.quality.clone(),
        image_count: edit_req.n,
        input_images_count: Some(edit_req.image.len() as u8),
        total_tokens: usage.map(|u| u.total_tokens).unwrap_or(0),
        input_tokens: usage.map(|u| u.input_tokens).unwrap_or(0),
        output_tokens: usage.map(|u| u.output_tokens).unwrap_or(0),
        text_tokens: usage.map(|u| u.input_tokens_details.text_tokens).unwrap_or(0),
        image_tokens: usage.map(|u| u.input_tokens_details.image_tokens).unwrap_or(0),
        r2_keys,
        response_time_ms,
        error: None,
        created_at: Utc::now(),
    };

    let db = env.d1("DB")?;
    let stmt = db.prepare(
        "INSERT INTO usage_records (id, app_id, user_id, request_type, provider, model, prompt, image_size, image_quality,
         image_count, input_images_count, total_tokens, input_tokens, output_tokens, text_tokens,
         image_tokens, r2_keys, response_time_ms, simplified_cost, error, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    );

    let simplified_cost = provider.get_name() == "gemini";

    let _ = stmt
        .bind(&[
            _usage_record.id.into(),
            app_id.clone().into(),
            _usage_record.user_id.into(),
            _usage_record.request_type.into(),
            provider.get_name().into(),
            _usage_record.model.into(),
            _usage_record.prompt.into(),
            _usage_record.image_size.into(),
            _usage_record.image_quality.into(),
            _usage_record.image_count.into(),
            _usage_record.input_images_count.map(|n| n.into()).unwrap_or(worker::wasm_bindgen::JsValue::NULL),
            _usage_record.total_tokens.into(),
            _usage_record.input_tokens.into(),
            _usage_record.output_tokens.into(),
            _usage_record.text_tokens.into(),
            _usage_record.image_tokens.into(),
            serde_json::to_string(&_usage_record.r2_keys).unwrap().into(),
            _usage_record.response_time_ms.into(),
            simplified_cost.into(),
            worker::wasm_bindgen::JsValue::NULL,
            _usage_record.created_at.to_rfc3339().into(),
        ])?
        .run()
        .await?;

    let _ = release_lock(&app_id, &user_id, &db).await;

    let response = ImageResponse {
        created: Utc::now().timestamp() as u64,
        data: image_data_list,
        background: if provider.get_name() == "openai" { Some(edit_req.background) } else { None },
        output_format: Some(edit_req.output_format),
        size: Some(edit_req.size),
        quality: if provider.get_name() == "openai" { Some(edit_req.quality) } else { None },
        usage: provider_response.usage,
    };
    
    Response::from_json(&response)
}

/// A 402 from the credit check is a paywall hit; it is logged before answering so the
/// funnel shows who ran out, not just who paid. Any other failure answers as before.
async fn credit_check_failure_response(
    error: worker::Error,
    db: &D1Database,
    app_id: &str,
    user_id: &str,
    capability: &str,
) -> Result<Response> {
    let app_error = AppError::from(error);
    if matches!(app_error, AppError::PaymentRequired(_)) {
        let balance = get_user_balance(app_id, user_id, db).await.unwrap_or(0);
        record_paywall_event(db, app_id, user_id, capability, balance).await;
    }
    app_error.to_response()
}

/// An image request that produced no image, logged so the funnel can tell a user who
/// never tried from one who tried and was refused.
struct FailedRequest<'a> {
    app_id: &'a str,
    user_id: &'a str,
    request_type: &'a str,
    provider: &'a str,
    model: &'a str,
    prompt: &'a str,
    size: &'a str,
    quality: &'a str,
    image_count: u8,
    input_images_count: Option<u8>,
    response_time_ms: u32,
    error: &'a str,
}

/// Best-effort: a logging failure never changes the response the caller already gets.
async fn record_failed_request(db: &D1Database, failed: FailedRequest<'_>) {
    let error: String = failed.error.chars().take(500).collect();
    let stmt = db
        .prepare(
            "INSERT INTO usage_records (id, app_id, user_id, request_type, provider, model, prompt, image_size, image_quality,
             image_count, input_images_count, total_tokens, input_tokens, output_tokens, text_tokens,
             image_tokens, r2_keys, response_time_ms, simplified_cost, error, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, 0, 0, 0, '[]', ?, ?, ?, ?)",
        )
        .bind(&[
            Uuid::new_v4().to_string().into(),
            failed.app_id.into(),
            failed.user_id.into(),
            failed.request_type.into(),
            failed.provider.into(),
            failed.model.into(),
            failed.prompt.into(),
            failed.size.into(),
            failed.quality.into(),
            failed.image_count.into(),
            failed.input_images_count.map(|n| n.into()).unwrap_or(worker::wasm_bindgen::JsValue::NULL),
            failed.response_time_ms.into(),
            (failed.provider == "gemini").into(),
            error.into(),
            Utc::now().to_rfc3339().into(),
        ]);
    match stmt {
        Ok(s) => {
            if let Err(e) = s.run().await {
                worker::console_log!("record_failed_request: insert failed: {:?}", e);
            }
        }
        Err(e) => worker::console_log!("record_failed_request: prepare failed: {:?}", e),
    }
}

/// The response for a provider call that produced nothing. A refusal is the request
/// being declined, not a server fault: moderation answers 400, and a Gemini reply
/// without an image (usually its safety filter on a photo of a person) answers 422
/// `no_image`, so clients can explain it instead of reporting a server error.
fn provider_failure_response(error_msg: &str, retry_subject: &str) -> Result<Response> {
    let (status, error_type, code, message) =
        if error_msg.contains("content_policy_violation") || error_msg.contains("moderation") {
            (
                400,
                "moderation_error",
                "moderation_blocked",
                format!("Our AI backend is being a bit too cautious with this image. Nothing wrong on your end - just the underlying service being overly protective. Try a different {} and you should be good to go!", retry_subject),
            )
        } else if error_msg.contains("No images returned by Gemini") {
            (
                422,
                "generation_error",
                "no_image",
                "The model declined to make this image. Try rewording the request - photos of people sometimes trip its safety filter.".to_string(),
            )
        } else {
            return AppError::InternalError(error_msg.to_string()).to_response();
        };

    Response::from_json(&ErrorResponse {
        error: ErrorDetail {
            message,
            error_type: error_type.to_string(),
            param: None,
            code: Some(code.to_string()),
        },
    })
    .map(|r| r.with_status(status))
}
