use worker::{console_log, Request, Response, Result, RouteContext};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::attribution::{
    attribution_totals, exchange_token, keyword_report, normalize_window, record_install,
    store_net_share, token_fingerprint, validate_token_shape, ExchangeOutcome, InstallOutcome,
};
use crate::auth;
use crate::error::AppError;
use crate::rate_limit::enforce_write_rate_limit;

const RETRY_AFTER_PENDING_SECONDS: u32 = 5;
const RETRY_AFTER_UNAVAILABLE_SECONDS: u32 = 60;

#[derive(Debug, Deserialize)]
pub struct SubmitAttributionRequest {
    pub app_id: Option<String>,
    pub attribution_token: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SubmitAttributionResponse {
    pub status: &'static str,
    pub attributed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u32>,
}

/// POST /v1/attribution — exchanges an AdServices token with Apple and stores the
/// resulting campaign/ad group/keyword against the caller's identity.
///
/// Every exchange outcome answers HTTP 200 with a status the client can act on
/// (`attributed`, `organic`, `duplicate`, `pending`, `invalid`, `unavailable`), so
/// nothing about ad measurement can stall or fail app launch. Only a bad caller
/// (missing key, unparseable body, wrong tenant) sees a 4xx.
pub async fn submit(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;
    let db = env.d1("DB")?;

    let auth = match auth::authenticate(&req, &db).await {
        Ok(a) => a,
        Err(e) => return e.to_response(),
    };

    let body: SubmitAttributionRequest = match req.json().await {
        Ok(b) => b,
        Err(e) => return AppError::BadRequest(format!("Invalid request body: {}", e)).to_response(),
    };

    if let Some(claimed) = body.app_id.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        if claimed != auth.app_id {
            return AppError::BadRequest("app_id does not match the authenticated app".to_string())
                .to_response();
        }
    }

    let token = body
        .attribution_token
        .or(body.token)
        .map(|t| t.trim().to_string())
        .unwrap_or_default();

    if let Err(reason) = validate_token_shape(&token) {
        console_log!("attribution token rejected locally: {}", reason);
        return terminal("invalid");
    }

    let fingerprint = token_fingerprint(&token);

    if let Some(existing) = crate::attribution::find_install_by_token(&db, &auth.app_id, &fingerprint).await? {
        return duplicate(existing.attributed);
    }
    if let Some(existing) = crate::attribution::find_install_by_user(&db, &auth.app_id, &auth.user_id).await? {
        return duplicate(existing.attributed);
    }

    if enforce_write_rate_limit(&env, &auth.app_id, &auth.user_id, "attribution.submit")
        .await
        .is_err()
    {
        return unavailable();
    }

    match exchange_token(&token).await {
        ExchangeOutcome::Resolved(attribution) => {
            let outcome = record_install(&db, &auth.app_id, &auth.user_id, &fingerprint, &attribution).await?;
            let (status, stored) = match outcome {
                InstallOutcome::Recorded(stored) => {
                    if stored.attributed {
                        ("attributed", stored)
                    } else {
                        ("organic", stored)
                    }
                }
                InstallOutcome::AlreadyRecorded(stored) => ("duplicate", stored),
            };
            console_log!(
                "attribution recorded app={} status={} campaign={:?} keyword={:?}",
                auth.app_id,
                status,
                stored.campaign_id,
                stored.keyword_id
            );
            Response::from_json(&SubmitAttributionResponse {
                status,
                attributed: stored.attributed,
                retry_after_seconds: None,
            })
        }
        ExchangeOutcome::Retryable(reason) => {
            console_log!("attribution exchange pending: {}", reason);
            Response::from_json(&SubmitAttributionResponse {
                status: "pending",
                attributed: false,
                retry_after_seconds: Some(RETRY_AFTER_PENDING_SECONDS),
            })
        }
        ExchangeOutcome::Terminal(reason) => {
            console_log!("attribution exchange terminal: {}", reason);
            terminal("invalid")
        }
    }
}

/// GET /v1/attribution/keywords — per-keyword installs, payers and revenue for
/// the authenticated app. Admin only; this is the input to the per-keyword pause
/// rule and the per-campaign kill rule.
pub async fn keywords(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let env = ctx.env;
    let db = env.d1("DB")?;

    let auth = match auth::authenticate(&req, &db).await {
        Ok(a) => a,
        Err(e) => return e.to_response(),
    };
    if !auth.is_admin {
        return AppError::Forbidden("Admin access required".to_string()).to_response();
    }

    let url = req.url()?;
    let param = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_string())
    };

    let (since, until) = match normalize_window(param("since").as_deref(), param("until").as_deref()) {
        Ok(window) => window,
        Err(reason) => return AppError::BadRequest(reason.to_string()).to_response(),
    };

    let net_share = store_net_share(&db, &auth.app_id).await;
    let keywords = keyword_report(&db, &auth.app_id, &since, &until, net_share).await?;
    let totals = attribution_totals(&db, &auth.app_id, &since, &until, net_share).await?;

    Response::from_json(&json!({
        "app_id": auth.app_id,
        "since": since,
        "until": until,
        "keywords": keywords,
        "totals": totals,
    }))
}

fn duplicate(attributed: bool) -> Result<Response> {
    Response::from_json(&SubmitAttributionResponse {
        status: "duplicate",
        attributed,
        retry_after_seconds: None,
    })
}

fn terminal(status: &'static str) -> Result<Response> {
    Response::from_json(&SubmitAttributionResponse {
        status,
        attributed: false,
        retry_after_seconds: None,
    })
}

fn unavailable() -> Result<Response> {
    Response::from_json(&SubmitAttributionResponse {
        status: "unavailable",
        attributed: false,
        retry_after_seconds: Some(RETRY_AFTER_UNAVAILABLE_SECONDS),
    })
}
