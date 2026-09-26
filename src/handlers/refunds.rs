use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use worker::{Request, Response, Result, RouteContext};

use crate::auth;
use crate::credits::add_credits;
use crate::error::AppError;
use crate::rate_limit::enforce_write_rate_limit;

/// Tenants whose clients may hand back a charge. Psywave charges before it
/// knows whether the suggested songs exist on Apple Music in the listener's
/// country; when almost none do, the playlist is unusable through no fault of
/// the listener.
const REFUNDABLE_APPS: &[&str] = &["psywave"];
const REFUND_WINDOW_MINUTES: i64 = 15;
const MAX_REFUNDS_PER_DAY: i64 = 2;

#[derive(Debug, Deserialize)]
struct RefundRequest {
    reference: String,
}

/// POST /v1/credits/refund — returns the credits of one recent chat charge whose
/// result the app could not use.
///
/// The caller can only name its own `spend` ledger rows; each charge is refundable
/// once (the `credit_refunds` primary key is the claim), only within
/// `REFUND_WINDOW_MINUTES` of being made, and at most `MAX_REFUNDS_PER_DAY` times
/// per wallet — so a modified client can recover at most two generations a day.
pub async fn refund_charge(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    match refund_charge_inner(req, ctx).await {
        Ok(response) => Ok(response),
        Err(e) => e.to_response(),
    }
}

async fn refund_charge_inner(mut req: Request, ctx: RouteContext<()>) -> std::result::Result<Response, AppError> {
    let db = ctx.env.d1("DB")?;
    let auth = auth::authenticate(&req, &db).await?;
    if !REFUNDABLE_APPS.contains(&auth.app_id.as_str()) {
        return Err(AppError::Forbidden("Refunds are not available for this app".to_string()));
    }
    enforce_write_rate_limit(&ctx.env, &auth.app_id, &auth.user_id, "credits.refund").await?;

    let body: RefundRequest = req
        .json()
        .await
        .map_err(|_| AppError::BadRequest("Invalid request body".to_string()))?;
    let reference = body.reference.trim().to_string();
    if !is_valid_reference(&reference) {
        return Err(AppError::BadRequest("Invalid charge reference".to_string()));
    }

    let spend = db
        .prepare(
            "SELECT amount, created_at FROM credit_transactions
             WHERE app_id = ? AND user_id = ? AND type = 'spend' AND reference_id = ?
             LIMIT 1",
        )
        .bind(&[auth.app_id.clone().into(), auth.user_id.clone().into(), reference.clone().into()])?
        .first::<serde_json::Value>(None)
        .await?
        .ok_or_else(|| AppError::NotFound("No such charge".to_string()))?;

    let amount = spend.get("amount").and_then(|a| a.as_i64()).unwrap_or(0).abs();
    let charged_at = spend
        .get("created_at")
        .and_then(|c| c.as_str())
        .and_then(|c| DateTime::parse_from_rfc3339(c).ok())
        .map(|c| c.with_timezone(&Utc));
    let now = Utc::now();
    if amount == 0 {
        return Err(AppError::Conflict("Nothing to refund".to_string()));
    }
    if !is_within_window(charged_at, now) {
        return Err(AppError::Conflict("This charge is too old to refund".to_string()));
    }

    let since = (now - Duration::hours(24)).to_rfc3339();
    let recent = db
        .prepare("SELECT COUNT(*) AS n FROM credit_refunds WHERE app_id = ? AND user_id = ? AND created_at >= ?")
        .bind(&[auth.app_id.clone().into(), auth.user_id.clone().into(), since.into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("n").and_then(|n| n.as_i64()))
        .unwrap_or(0);
    if recent >= MAX_REFUNDS_PER_DAY {
        return Err(AppError::Conflict("Daily refund limit reached".to_string()));
    }

    let claim = db
        .prepare(
            "INSERT OR IGNORE INTO credit_refunds (reference_id, app_id, user_id, amount, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&[
            reference.clone().into(),
            auth.app_id.clone().into(),
            auth.user_id.clone().into(),
            (amount as i32).into(),
            now.to_rfc3339().into(),
        ])?
        .run()
        .await?;
    let claimed = claim.meta()?.and_then(|m| m.changes).unwrap_or(0) > 0;
    if !claimed {
        return Err(AppError::Conflict("This charge was already refunded".to_string()));
    }

    let refund_reference = format!("refund:{}", reference);
    let balance = match add_credits(
        &auth.app_id,
        &auth.user_id,
        amount as u32,
        "refund",
        "Refund: result unavailable",
        Some(&refund_reference),
        &db,
    )
    .await
    {
        Ok(balance) => balance,
        Err(e) => {
            release_claim(&reference, &db).await;
            return Err(AppError::from(e));
        }
    };

    Ok(Response::from_json(&json!({ "refunded": amount, "balance": balance }))?)
}

/// Frees a claim whose credit never landed, so the charge stays refundable.
async fn release_claim(reference: &str, db: &worker::D1Database) {
    let statement = db.prepare("DELETE FROM credit_refunds WHERE reference_id = ?");
    if let Ok(bound) = statement.bind(&[reference.into()]) {
        let _ = bound.run().await;
    }
}

fn is_valid_reference(reference: &str) -> bool {
    reference.len() <= 64
        && reference
            .strip_prefix("chat:")
            .map(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
            .unwrap_or(false)
}

fn is_within_window(charged_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match charged_at {
        Some(at) => at <= now && now - at <= Duration::minutes(REFUND_WINDOW_MINUTES),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_chat_references() {
        assert!(is_valid_reference("chat:6f1c2d0e-8b1a-4c1e-9f6e-0a1b2c3d4e5f"));
        assert!(!is_valid_reference("image:6f1c2d0e"));
        assert!(!is_valid_reference("chat:"));
        assert!(!is_valid_reference("chat:x' OR 1=1 --"));
        assert!(!is_valid_reference(&format!("chat:{}", "a".repeat(80))));
    }

    #[test]
    fn window_is_fifteen_minutes_and_never_in_the_future() {
        let now = Utc::now();
        assert!(is_within_window(Some(now - Duration::minutes(14)), now));
        assert!(!is_within_window(Some(now - Duration::minutes(16)), now));
        assert!(!is_within_window(Some(now + Duration::minutes(1)), now));
        assert!(!is_within_window(None, now));
    }
}
