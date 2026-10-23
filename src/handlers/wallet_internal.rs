use serde::Deserialize;
use serde_json::{json, Value};
use worker::{Env, Request, Response, Result, RouteContext};

use crate::error::AppError;
use crate::internal;
use crate::storekit::LedgerTransaction;
use crate::wallet::client::DoWallets;
use crate::wallet::ledger_grant::{self, Outcome};
use crate::wallet::lifecycle::{self, Erasure, Merged};
use crate::wallet::target::is_midgar_id;
use crate::wallet::{link, log, resolve};

/// The wrangler variable that opens `/internal/wallet/grant`. Anything but `on` hides the route.
pub const LEDGER_GRANTS: &str = "LEDGER_GRANTS";

/// True when `LEDGER_GRANTS` is `on`.
fn ledger_grants_on(env: &Env) -> bool {
    resolve::flag_is_on(env.var(LEDGER_GRANTS).ok().map(|v| v.to_string()).as_deref())
}

/// The answer to a forwarded transaction, as the ledger reads it: every 2xx means the transaction needs no retry.
fn answer(outcome: &Outcome) -> Value {
    match outcome {
        Outcome::Credited { purchase_id, credits, .. } => json!({ "ok": true, "status": "credited", "purchase_id": purchase_id, "credits": credits }),
        Outcome::Duplicate { purchase_id } => json!({ "ok": true, "status": "duplicate", "purchase_id": purchase_id }),
        Outcome::Refused(reason) => json!({ "ok": true, "status": "refused", "reason": reason.as_str() }),
    }
}

/// `POST /internal/wallet/grant`: the ledger forwards a verified consumable credit-pack transaction. Needs
/// `WALLET_INTERNAL_SECRET` in `X-Midgar-Internal` and `LEDGER_GRANTS = "on"`; either missing is 404. With
/// `WALLET_LINKING` off nothing is credited, because no wallet exists to credit. A wallet that cannot answer is a 503
/// the ledger retries.
pub async fn grant(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    match grant_inner(&mut req, &ctx.env).await {
        Ok(body) => Response::from_json(&body),
        Err(error) => error.to_response(),
    }
}

async fn grant_inner(req: &mut Request, env: &Env) -> std::result::Result<Value, AppError> {
    internal::require(req, env, internal::GRANT_SECRET)?;
    if !ledger_grants_on(env) {
        return Err(AppError::NotFound("No such route.".to_string()));
    }
    let transaction: LedgerTransaction =
        req.json().await.map_err(|_| AppError::BadRequest("The body is not a verified transaction.".to_string()))?;
    if !link::enabled(env) {
        return Ok(answer(&Outcome::Refused(ledger_grant::Reason::LinkingOff)));
    }
    let db = env.d1("DB")?;
    let wallets = DoWallets(env.durable_object("WALLET").map_err(|_| AppError::WalletUnavailable)?);
    let production = crate::purchases::is_production(env.var("ENVIRONMENT").ok().map(|v| v.to_string()).as_deref());
    let outcome = ledger_grant::credit(&db, &wallets, production, &transaction).await?;
    if let Outcome::Credited { app_id, user_id, purchase_id, pack_id, amount_usd_cents, .. } = &outcome {
        crate::credits::tag_purchase_attribution(&db, app_id, user_id, purchase_id, pack_id, *amount_usd_cents).await;
    }
    log::info(&format!("ledger_grant tx={} {}", transaction.transaction_id, answer(&outcome)["status"]));
    Ok(answer(&outcome))
}

/// The body of `POST /internal/wallet/erase_all`.
#[derive(Deserialize)]
struct EraseAllRequest {
    id: String,
}

/// A Midgar ID in the one spelling mako stores, or `None` for anything that cannot be one. Such an event is answered
/// with a 200 that says it was ignored: refusing it would make identity retry it for ever.
fn midgar_id_of(raw: &str) -> Option<String> {
    Some(raw.to_lowercase()).filter(|id| is_midgar_id(id))
}

/// The wallet object namespace, whatever `WALLET_LINKING` says: an event about a deleted or merged ID must reach the
/// wallet even after linking was switched off again, because the money stays in the object.
fn wallets_of(env: &Env) -> std::result::Result<DoWallets, AppError> {
    env.durable_object("WALLET").map(DoWallets).map_err(|_| AppError::WalletUnavailable)
}

/// `POST /internal/wallet/erase_all`: identity tells mako that a Midgar ID was deleted. Needs
/// `WALLET_LIFECYCLE_SECRET` in `X-Midgar-Internal` (404 when unset, 401 when wrong). Erases every tenant partition of
/// the ID's wallet, which then refuses writes, and removes its `wallet_links` and `wallet_summary` rows. Idempotent. A
/// wallet that cannot answer is a 503 identity retries with backoff until it does.
pub async fn erase_all(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    match erase_all_inner(&mut req, &ctx.env).await {
        Ok(body) => Response::from_json(&body),
        Err(error) => error.to_response(),
    }
}

async fn erase_all_inner(req: &mut Request, env: &Env) -> std::result::Result<Value, AppError> {
    internal::require(req, env, internal::LIFECYCLE_SECRET)?;
    let body: EraseAllRequest = req.json().await.map_err(|_| AppError::BadRequest("The body must be {\"id\": \"<midgar id>\"}.".to_string()))?;
    let Some(id) = midgar_id_of(&body.id) else { return Ok(json!({ "ok": true, "status": "ignored" })) };
    let db = env.d1("DB")?;
    Ok(match lifecycle::erase_all(&db, &wallets_of(env)?, &id).await? {
        Erasure::Erased { entries, links } => json!({ "ok": true, "status": "erased", "entries": entries, "links": links }),
        Erasure::NoWallet => json!({ "ok": true, "status": "no_wallet" }),
    })
}

/// The body of `POST /internal/wallet/merge`.
#[derive(Deserialize)]
struct MergeRequest {
    from: String,
    to: String,
}

/// `POST /internal/wallet/merge`: identity tells mako that Midgar ID `from` was merged into `to`. Needs
/// `WALLET_LIFECYCLE_SECRET` in `X-Midgar-Internal` (404 when unset, 401 when wrong). Seals the wallet of `from`, grants
/// its credits to the survivor per tenant under `merge:<from>:<tenant>`, and points the `wallet_links` of `from` at the
/// survivor. Idempotent; a survivor that was deleted means `from`'s wallet is erased instead. A wallet that cannot
/// answer is a 503 identity retries with backoff until it does.
pub async fn merge(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    match merge_inner(&mut req, &ctx.env).await {
        Ok(body) => Response::from_json(&body),
        Err(error) => error.to_response(),
    }
}

async fn merge_inner(req: &mut Request, env: &Env) -> std::result::Result<Value, AppError> {
    internal::require(req, env, internal::LIFECYCLE_SECRET)?;
    let body: MergeRequest =
        req.json().await.map_err(|_| AppError::BadRequest("The body must be {\"from\": \"<midgar id>\", \"to\": \"<midgar id>\"}.".to_string()))?;
    let (Some(from), Some(to)) = (midgar_id_of(&body.from), midgar_id_of(&body.to)) else {
        return Ok(json!({ "ok": true, "status": "ignored" }));
    };
    let db = env.d1("DB")?;
    Ok(match lifecycle::merge(&db, &wallets_of(env)?, &from, &to).await? {
        Merged::Moved { tenants, credits, .. } => json!({ "ok": true, "status": "merged", "tenants": tenants, "credits": credits }),
        Merged::NoWallet => json!({ "ok": true, "status": "no_wallet" }),
        Merged::Discarded => json!({ "ok": true, "status": "discarded" }),
        Merged::Same => json!({ "ok": true, "status": "same" }),
    })
}
