use worker::{D1Database, Env, Response, Result};

use crate::storekit::VerifyFailure;

use crate::auth::AuthedUser;
use crate::credits::CreditPack;
use crate::error::AppError;
use crate::handlers::credits::{ValidatePurchaseRequest, ValidatePurchaseResponse};
use crate::handlers::storekit::ALREADY_PROCESSED_MESSAGE;
use crate::ledgers::Ledgers;
use crate::playstore::{self, Acknowledgements, Expected, FetchTransport, PlayApi, ProductPurchase, ServiceAccount, SubtleSigner, NOT_CONFIGURED_MESSAGE};
use crate::storekit::{already_settled, pack_of_product, Refusal, PENDING_MESSAGE, REFUNDED_MESSAGE};
use crate::wallet::pack_credit::{credit_pack, Credit};
use crate::wallet::{log, pack_refund, purchase};

/// A pending answer for any failure that a retry of the same request can fix.
fn pending(reason: impl std::fmt::Debug) -> AppError {
    log::error(&format!("google play credit pending: {reason:?}"));
    AppError::CreditPending(PENDING_MESSAGE.to_string())
}

fn not_configured(reason: &str) -> AppError {
    log::error(&format!("google play not configured: {reason}"));
    AppError::NotConfigured(NOT_CONFIGURED_MESSAGE.to_string())
}

/// The service account in `GOOGLE_PLAY_SERVICE_ACCOUNT`, `Err` with the reason when it is unset or not a key file.
fn service_account(env: &Env) -> std::result::Result<ServiceAccount, &'static str> {
    let json = env.secret(playstore::SECRET).map(|secret| secret.to_string()).map_err(|_| "GOOGLE_PLAY_SERVICE_ACCOUNT is not set")?;
    ServiceAccount::parse(&json).ok_or("GOOGLE_PLAY_SERVICE_ACCOUNT is not a service account key file")
}

fn now_seconds() -> u64 {
    worker::Date::now().as_millis() / 1000
}

/// `POST /v1/credits/purchase/validate` with `platform: "android"`: Google Play's Developer API confirms the purchase
/// token (`purchases.products.get`) as the service account in `GOOGLE_PLAY_SERVICE_ACCOUNT`, mako checks it is this
/// tenant's paid pack bought by the caller, and credits it once under `tx:gp:<orderId>` through the same purchase row,
/// refund tombstone and wallet grant as an App Store pack. A tenant without `play_package_name` or a server without the
/// secret answers 503 `not_configured` and grants nothing; the app keeps the purchase unconsumed, and Google refunds
/// it after three days if it is never credited. Refunds made in Play are not reversed yet: that needs Real-time
/// developer notifications (or the Voided Purchases API) feeding `pack_refund`.
pub async fn validate_play(env: &Env, db: &D1Database, auth: &AuthedUser, request: &ValidatePurchaseRequest, pack: &CreditPack) -> Result<Response> {
    match credit_play(env, db, auth, request, pack).await {
        Ok(response) => Ok(response),
        Err(error) => match crate::error::wallet_outage_response(error) {
            Ok(response) => Ok(response),
            Err(error) => AppError::from(error).to_response(),
        },
    }
}

async fn credit_play(env: &Env, db: &D1Database, auth: &AuthedUser, request: &ValidatePurchaseRequest, pack: &CreditPack) -> Result<Response> {
    let app_id = auth.app_id.as_str();
    let Some(package) = playstore::package_of(db, app_id).await? else {
        return not_configured(&format!("{app_id} has no play_package_name")).to_response();
    };
    let account = match service_account(env) {
        Ok(account) => account,
        Err(reason) => return not_configured(reason).to_response(),
    };
    if pack_of_product(db, app_id, &request.product_id).await?.as_deref() != Some(pack.id.as_str()) {
        return Refusal::WrongProduct.into_error().to_response();
    }
    let api = PlayApi { account: &account, transport: &FetchTransport, signer: &SubtleSigner, tokens: playstore::isolate_tokens(), now: now_seconds() };
    let expected = Expected { product_id: &request.product_id, user_id: &auth.user_id, order_id: request.play_order_id.as_deref() };
    let bought = match playstore::vet(&api, &package, &request.purchase_token, &expected).await {
        Ok(bought) => bought,
        Err(error) => return error.to_response(),
    };
    let payment_id = playstore::payment_id(&bought, &request.purchase_token);

    match pack_refund::refunded_error(db, app_id, &payment_id).await {
        Ok(Some(refunded)) => return refunded.to_response(),
        Ok(None) => {}
        Err(error) => return pending(error).to_response(),
    }
    match purchase::store_status(db, app_id, &payment_id).await {
        Ok(status) if already_settled(status.as_deref()) => {
            acknowledge(&api, db, app_id, &package, request, &bought, &payment_id).await;
            return AppError::AlreadyProcessed(ALREADY_PROCESSED_MESSAGE.to_string()).to_response();
        }
        Ok(_) => {}
        Err(error) => return pending(error).to_response(),
    }
    let ledgers = Ledgers::new(env, db);
    let credits = (pack.credits + pack.bonus_credits) as u32;
    let charged = bought.charged_cents(pack.price_usd_cents as u32);
    let token = (!bought.acknowledged()).then_some(request.purchase_token.as_str());
    let credited = credit_pack(db, &ledgers, app_id, auth.account.user_id(), &pack.id, credits, charged, &payment_id, token).await;
    let purchase_id = match credited {
        Ok(Credit::Credited { purchase_id }) => purchase_id,
        Ok(Credit::Duplicate) => {
            acknowledge(&api, db, app_id, &package, request, &bought, &payment_id).await;
            return AppError::AlreadyProcessed(ALREADY_PROCESSED_MESSAGE.to_string()).to_response();
        }
        Ok(Credit::Refunded) | Err(AppError::Refunded(_)) => return AppError::Refunded(REFUNDED_MESSAGE.to_string()).to_response(),
        Err(error) => return pending(error).to_response(),
    };
    acknowledge(&api, db, app_id, &package, request, &bought, &payment_id).await;
    let new_balance = ledgers.get_user_balance(&auth.account).await?;
    Response::from_json(&ValidatePurchaseResponse { success: true, purchase_id, credits_added: credits, new_balance })
}

/// Acknowledges a credited purchase the app has not consumed yet, so Google cannot refund it after three days while the
/// app is closed. Once Google has it (or will never take it) the row's token is dropped; while Google cannot be asked the
/// token stays on the row and the cron acknowledges it (`run_cron`).
#[allow(clippy::too_many_arguments)]
async fn acknowledge(
    api: &PlayApi<'_, FetchTransport, SubtleSigner>,
    db: &D1Database,
    app_id: &str,
    package: &str,
    request: &ValidatePurchaseRequest,
    bought: &ProductPurchase,
    payment_id: &str,
) {
    if bought.acknowledged() {
        return;
    }
    let kept = match api.acknowledge(package, &request.product_id, &request.purchase_token).await {
        Ok(()) | Err(VerifyFailure::Rejected) => purchase::forget_store_token(db, app_id, payment_id).await,
        Err(failure) => {
            log::error(&format!("google play acknowledgement postponed to the cron: {failure:?}"));
            purchase::remember_store_token(db, app_id, payment_id, &request.purchase_token).await
        }
    };
    if let Err(error) = kept {
        log::error(&format!("google play token bookkeeping failed for {payment_id}: {error:?}"));
    }
}

/// The scheduled Google Play work: for every tenant with a Play package, acknowledges the credited purchases that still
/// wait for it (`playstore::acknowledge_credited`). Does nothing without `GOOGLE_PLAY_SERVICE_ACCOUNT`.
pub async fn run_cron(env: &Env, db: &D1Database) {
    let tenants = match playstore::play_tenants(db).await {
        Ok(tenants) if !tenants.is_empty() => tenants,
        Ok(_) => return,
        Err(error) => {
            log::error(&format!("google play cron could not list tenants: {error:?}"));
            return;
        }
    };
    let Ok(account) = service_account(env) else { return };
    let api = PlayApi { account: &account, transport: &FetchTransport, signer: &SubtleSigner, tokens: playstore::isolate_tokens(), now: now_seconds() };
    for tenant in &tenants {
        match playstore::acknowledge_credited(&api, db, tenant).await {
            Ok(report) if report != Acknowledgements::default() => log::info(&format!("google play cron {}: {report:?}", tenant.app_id)),
            Ok(_) => {}
            Err(error) => log::error(&format!("google play acknowledgements failed for {}: {error:?}", tenant.app_id)),
        }
    }
}
