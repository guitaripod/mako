use worker::{D1Database, Env, Request, Response, Result};

use crate::auth::AuthedUser;
use crate::credits::CreditPack;
use crate::error::AppError;
use crate::handlers::credits::{ValidateRevenueCatPurchaseRequest, ValidateRevenueCatPurchaseResponse};
use crate::ledgers::{Account, Ledgers};
use crate::midgar_token::MidgarCaller;
use crate::storekit::{already_settled, pack_of_product, tenant_bundle, vet, Expected, LedgerService, PENDING_MESSAGE, REFUNDED_MESSAGE};
use crate::wallet::pack_credit::{credit_pack, Credit};
use crate::wallet::{log, pack_refund, purchase};

/// The text of an `already_processed` answer, as the RevenueCat path has always worded it.
const ALREADY_PROCESSED_MESSAGE: &str = "Purchase has already been processed";

/// A pending answer for any failure that a retry of the same request can fix.
fn pending(reason: impl std::fmt::Debug) -> AppError {
    log::error(&format!("storekit credit pending: {reason:?}"));
    AppError::CreditPending(PENDING_MESSAGE.to_string())
}

/// The Midgar ID a signed purchase must have been made by: the verified `X-Midgar-Token` or the native JWT. With
/// `WALLET_LINKING` off the authentication does not read the token, so it is read here.
async fn caller_of(req: &Request, env: &Env, auth: &AuthedUser) -> Option<MidgarCaller> {
    match &auth.midgar {
        Some(caller) => Some(caller.clone()),
        None => crate::midgar_token::caller_from_request(req, env, &auth.app_id).await,
    }
}

/// `POST /v1/credits/purchase/revenuecat/validate` with a `signed_transaction`: the ledger verifies the StoreKit 2
/// transaction, mako checks it belongs to this tenant, product and caller, and credits the pack once under the store
/// transaction id, the key the RevenueCat webhook uses, so the two dedupe. A grant lands in a wallet only with
/// `WALLET_LINKING` on; off, it credits the local balance through the same purchase row.
pub async fn validate_signed(
    req: &Request,
    env: &Env,
    db: &D1Database,
    auth: &AuthedUser,
    request: &ValidateRevenueCatPurchaseRequest,
    pack: &CreditPack,
) -> Result<Response> {
    match credit_signed(req, env, db, auth, request, pack).await {
        Ok(response) => Ok(response),
        Err(error) => match crate::error::wallet_outage_response(error) {
            Ok(response) => Ok(response),
            Err(error) => AppError::from(error).to_response(),
        },
    }
}

async fn credit_signed(
    req: &Request,
    env: &Env,
    db: &D1Database,
    auth: &AuthedUser,
    request: &ValidateRevenueCatPurchaseRequest,
    pack: &CreditPack,
) -> Result<Response> {
    let signed = request.signed_transaction.as_deref().unwrap_or_default();
    let Some(caller) = caller_of(req, env, auth).await else {
        return AppError::Unauthorized("A Midgar ID session is required to credit a signed purchase.".to_string()).to_response();
    };
    let app_id = auth.app_id.as_str();
    let policy = crate::tenant::TenantPolicy::load(db, app_id).await?;
    let slug = crate::tenant::registry_slug(app_id);
    let bundle = tenant_bundle(db, app_id).await?;
    let production = crate::purchases::is_production(env.var("ENVIRONMENT").ok().map(|v| v.to_string()).as_deref());
    let expected = Expected {
        product_id: &request.product_id,
        midgar_id: &caller.midgar_id,
        tenant_slug: &slug,
        tenant_bundle: bundle.as_deref(),
        strict: policy.strict_purchases,
        production,
    };
    let verifier = match LedgerService::open(env) {
        Ok(verifier) => verifier,
        Err(reason) => return pending(reason).to_response(),
    };
    let transaction = match vet(&verifier, signed, &expected).await {
        Ok(transaction) => transaction,
        Err(error) => return error.to_response(),
    };
    if pack_of_product(db, app_id, &transaction.product_id).await?.as_deref() != Some(pack.id.as_str()) {
        return crate::storekit::Refusal::WrongProduct.into_error().to_response();
    }

    match pack_refund::refunded_error(db, app_id, &transaction.transaction_id).await {
        Ok(Some(refunded)) => return refunded.to_response(),
        Ok(None) => {}
        Err(error) => return pending(error).to_response(),
    }
    match purchase::revenuecat_status(db, app_id, &transaction.transaction_id).await {
        Ok(status) if already_settled(status.as_deref()) => return AppError::AlreadyProcessed(ALREADY_PROCESSED_MESSAGE.to_string()).to_response(),
        Ok(_) => {}
        Err(error) => return pending(error).to_response(),
    }
    let ledgers = Ledgers::new(env, db);
    let account = match target_of(&ledgers, auth, &caller).await {
        Ok(account) => account,
        Err(error) => return pending(error).to_response(),
    };
    let credits = (pack.credits + pack.bonus_credits) as u32;
    let credited = credit_pack(
        db,
        &ledgers,
        app_id,
        account.user_id(),
        &pack.id,
        credits,
        pack.price_usd_cents as u32,
        &transaction.transaction_id,
    )
    .await;
    let purchase_id = match credited {
        Ok(Credit::Credited { purchase_id }) => purchase_id,
        Ok(Credit::Duplicate) => return AppError::AlreadyProcessed(ALREADY_PROCESSED_MESSAGE.to_string()).to_response(),
        Ok(Credit::Refunded) | Err(AppError::Refunded(_)) => return AppError::Refunded(REFUNDED_MESSAGE.to_string()).to_response(),
        Err(error) => return pending(error).to_response(),
    };
    let new_balance = ledgers.get_user_balance(&account).await?;
    Response::from_json(&ValidateRevenueCatPurchaseResponse { success: true, purchase_id, credits_added: credits, new_balance })
}

/// Where the credits go: with linking on, the account the RevenueCat webhook would resolve the Midgar ID to (so the
/// two paths credit one wallet); off, the caller's own local account.
async fn target_of(ledgers: &Ledgers<'_>, auth: &AuthedUser, caller: &MidgarCaller) -> Result<Account> {
    if ledgers.linking() {
        return ledgers.credit_target(&auth.app_id, &caller.midgar_id).await;
    }
    Ok(auth.account.clone())
}
