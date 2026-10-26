use serde::Deserialize;
use serde_json::Value;
use worker::{Env, Fetcher, Headers, Method, RequestInit};

use crate::error::AppError;
use crate::wallet::d1::{text, D1Exec};

/// The secret that authorises the ledger's verify route, sent in `X-Midgar-Internal`.
const SECRET: &str = "LEDGER_VERIFY_SECRET";

/// The header the shared secret travels in, as identity and the ledger use it.
const INTERNAL_HEADER: &str = "X-Midgar-Internal";

/// The ledger route that verifies a StoreKit 2 signed transaction and stores nothing.
const VERIFY_URL: &str = "https://ledger.internal/internal/verify-transaction";

/// The ledger route that answers a Midgar ID's active entitlements in one environment.
const ENTITLEMENTS_URL: &str = "https://ledger.internal/internal/entitlements";

/// The App Store's name for a consumable in a verified transaction.
pub const CONSUMABLE: &str = "Consumable";

/// The ownership of a transaction the buyer made themselves, not one shared through Family Sharing.
const PURCHASED: &str = "PURCHASED";

/// The decoded fields of a signed transaction as the ledger's `POST /internal/verify-transaction` answers them.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LedgerTransaction {
    pub transaction_id: String,
    pub original_transaction_id: String,
    pub bundle_id: String,
    pub product_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub app_account_token: Option<String>,
    pub environment: String,
    pub purchase_date: i64,
    pub revocation_date: Option<i64>,
    pub in_app_ownership_type: Option<String>,
    pub quantity: i64,
    #[serde(default)]
    pub registry_slug: Option<String>,
}

/// One active entitlement as the ledger's entitlement routes answer it: the registry app and the registry name of
/// the entitlement (null for a product the registry does not name).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LedgerEntitlement {
    pub app: String,
    pub entitlement: Option<String>,
}

/// True when the tenant's own entitlement is among the active ones. An entitlement of another app never counts.
pub fn grants_entitlement(entitlements: &[LedgerEntitlement], tenant_slug: &str, required: &str) -> bool {
    entitlements.iter().any(|item| item.app == tenant_slug && item.entitlement.as_deref() == Some(required))
}

/// Why the ledger gave no transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyFailure {
    /// The ledger read the payload and it does not verify: a forged, altered or foreign signature.
    Rejected,
    /// The ledger could not be asked or did not answer usefully: a missing binding or secret, a timeout, a 5xx.
    Unavailable(String),
}

/// Turns the ledger's answer into a transaction. 200 is the decoded transaction, 400 is a payload that does not
/// verify, and anything else (a wrong secret, an unset secret, a rate limit, an outage) is the ledger being
/// unavailable, which must never read as a verdict on the purchase.
pub fn classify_answer(status: u16, body: &Value) -> Result<LedgerTransaction, VerifyFailure> {
    match status {
        200 => serde_json::from_value(body.clone()).map_err(|_| VerifyFailure::Unavailable("the ledger answered an unreadable transaction".to_string())),
        400 => Err(VerifyFailure::Rejected),
        other => Err(VerifyFailure::Unavailable(format!("the ledger answered {other}"))),
    }
}

/// Anything that can verify a StoreKit 2 signed transaction.
#[allow(async_fn_in_trait)]
pub trait TransactionVerifier {
    async fn verify(&self, signed_transaction: &str) -> Result<LedgerTransaction, VerifyFailure>;
}

/// The ledger behind the `LEDGER` service binding.
pub struct LedgerService {
    fetcher: Fetcher,
    secret: String,
}

impl LedgerService {
    /// Opens the binding and the shared secret; either missing means the ledger cannot be asked.
    pub fn open(env: &Env) -> Result<Self, VerifyFailure> {
        let secret = env
            .secret(SECRET)
            .map(|s| s.to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| VerifyFailure::Unavailable(format!("{SECRET} is not configured")))?;
        let fetcher = env.service("LEDGER").map_err(|_| VerifyFailure::Unavailable("the LEDGER binding is missing".to_string()))?;
        Ok(Self { fetcher, secret })
    }
}

impl TransactionVerifier for LedgerService {
    async fn verify(&self, signed_transaction: &str) -> Result<LedgerTransaction, VerifyFailure> {
        let unavailable = |e: worker::Error| VerifyFailure::Unavailable(format!("the ledger could not be reached: {e}"));
        let headers = Headers::new();
        headers.set(INTERNAL_HEADER, &self.secret).map_err(unavailable)?;
        headers.set("Content-Type", "application/json").map_err(unavailable)?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(serde_json::json!({ "signedTransaction": signed_transaction }).to_string().into()));
        let mut response = self.fetcher.fetch(VERIFY_URL, Some(init)).await.map_err(unavailable)?;
        let status = response.status_code();
        let body = response.json::<Value>().await.unwrap_or(Value::Null);
        classify_answer(status, &body)
    }
}

impl LedgerService {
    /// The Midgar ID's active entitlements in `environment` (`Production` or `Sandbox`), also counting purchases whose
    /// `appAccountToken` is one of `also_tokens` (ids mako vouches are the same caller's). Anything but a 200 with a
    /// readable list is the ledger being unavailable, never an empty answer.
    pub async fn entitlements(
        &self,
        midgar_id: &str,
        environment: &str,
        also_tokens: &[String],
    ) -> Result<Vec<LedgerEntitlement>, VerifyFailure> {
        let unavailable = |e: worker::Error| VerifyFailure::Unavailable(format!("the ledger could not be reached: {e}"));
        let headers = Headers::new();
        headers.set(INTERNAL_HEADER, &self.secret).map_err(unavailable)?;
        headers.set("Content-Type", "application/json").map_err(unavailable)?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(
                serde_json::json!({ "midgarId": midgar_id, "environment": environment, "appAccountTokens": also_tokens })
                    .to_string()
                    .into(),
            ));
        let mut response = self.fetcher.fetch(ENTITLEMENTS_URL, Some(init)).await.map_err(unavailable)?;
        let status = response.status_code();
        let body = response.json::<Value>().await.unwrap_or(Value::Null);
        entitlements_from_answer(status, &body)
    }
}

/// Reads the ledger's entitlement answer: 200 with an `entitlements` list, anything else unavailable.
pub fn entitlements_from_answer(status: u16, body: &Value) -> Result<Vec<LedgerEntitlement>, VerifyFailure> {
    if status != 200 {
        return Err(VerifyFailure::Unavailable(format!("the ledger answered {status}")));
    }
    body.get("entitlements")
        .cloned()
        .and_then(|list| serde_json::from_value(list).ok())
        .ok_or_else(|| VerifyFailure::Unavailable("the ledger answered unreadable entitlements".to_string()))
}

/// What a verified transaction has to match to be credited as a pack.
#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
    pub product_id: &'a str,
    pub midgar_id: &'a str,
    pub tenant_slug: &'a str,
    pub tenant_bundle: Option<&'a str>,
}

/// Why a verified transaction is not credited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    WrongApp,
    NotYours,
    WrongProduct,
    NotConsumable,
    Revoked,
    NotPurchased,
    Quantity,
}

impl Refusal {
    /// The error a refusal answers with. A transaction that belongs to another app or Midgar ID is 403; one that
    /// is no creditable pack purchase is 400 `bad_request`, final for the client.
    pub fn into_error(self) -> AppError {
        match self {
            Refusal::WrongApp => AppError::Forbidden("The transaction was not made in this app.".to_string()),
            Refusal::NotYours => AppError::Forbidden("The transaction was not made by this Midgar ID.".to_string()),
            Refusal::WrongProduct => AppError::BadRequest("The transaction is not the product being credited.".to_string()),
            Refusal::NotConsumable => AppError::BadRequest("The transaction is not a consumable purchase.".to_string()),
            Refusal::Revoked => AppError::BadRequest("The transaction was refunded or revoked.".to_string()),
            Refusal::NotPurchased => AppError::BadRequest("The transaction was not purchased by this Apple account.".to_string()),
            Refusal::Quantity => AppError::BadRequest("Only a purchase of one pack is credited.".to_string()),
        }
    }
}

/// True when the transaction's bundle id belongs to the tenant: the registry app the ledger found for it is the tenant's
/// slug, or its bundle id is the tenant's own `apple_app_bundle_id`.
pub fn made_in_tenant(transaction: &LedgerTransaction, tenant_slug: &str, tenant_bundle: Option<&str>) -> bool {
    let registry_match = transaction.registry_slug.as_deref() == Some(tenant_slug);
    let bundle_match = tenant_bundle.is_some_and(|bundle| bundle == transaction.bundle_id);
    registry_match || bundle_match
}

/// Decides whether a verified transaction may be credited: its bundle id belongs to the tenant (the registry app
/// the ledger found for it, or the tenant's own `apple_app_bundle_id`), its `appAccountToken` is the caller's
/// Midgar ID, it is the product the request names, a consumable, not revoked, bought by the Apple account itself,
/// for one pack. Sandbox purchases count like any other, so App Review and TestFlight buyers are credited.
pub fn check(transaction: &LedgerTransaction, expected: &Expected<'_>) -> Result<(), Refusal> {
    if !made_in_tenant(transaction, expected.tenant_slug, expected.tenant_bundle) {
        return Err(Refusal::WrongApp);
    }
    match transaction.app_account_token.as_deref() {
        Some(token) if token.eq_ignore_ascii_case(expected.midgar_id) => {}
        _ => return Err(Refusal::NotYours),
    }
    if transaction.product_id != expected.product_id {
        return Err(Refusal::WrongProduct);
    }
    if transaction.kind != CONSUMABLE {
        return Err(Refusal::NotConsumable);
    }
    if transaction.revocation_date.is_some() {
        return Err(Refusal::Revoked);
    }
    if transaction.in_app_ownership_type.as_deref() != Some(PURCHASED) {
        return Err(Refusal::NotPurchased);
    }
    if transaction.quantity != 1 {
        return Err(Refusal::Quantity);
    }
    Ok(())
}

/// Verifies a signed transaction through the ledger and checks it against what the request claims. A payload that
/// does not verify is `unverifiable_purchase`; a ledger that cannot be asked is `credit_pending`, because the
/// RevenueCat webhook credits the same transaction under the same key; a refusal is its own error.
pub async fn vet<V: TransactionVerifier>(verifier: &V, signed_transaction: &str, expected: &Expected<'_>) -> Result<LedgerTransaction, AppError> {
    let transaction = match verifier.verify(signed_transaction).await {
        Ok(transaction) => transaction,
        Err(VerifyFailure::Rejected) => return Err(AppError::UnverifiablePurchase("Purchase could not be verified.".to_string())),
        Err(VerifyFailure::Unavailable(reason)) => {
            crate::wallet::log::error(&format!("storekit verification unavailable: {reason}"));
            return Err(AppError::CreditPending(PENDING_MESSAGE.to_string()));
        }
    };
    check(&transaction, expected).map_err(Refusal::into_error)?;
    Ok(transaction)
}

/// The App Store bundle id configured for a tenant, if any.
pub async fn tenant_bundle(db: &impl D1Exec, app_id: &str) -> Result<Option<String>, AppError> {
    let rows = db.rows("SELECT apple_app_bundle_id FROM apps WHERE app_id = ?1", &[app_id.into()]).await?;
    Ok(rows.first().and_then(|row| text(row, "apple_app_bundle_id")).filter(|bundle| !bundle.is_empty()).map(str::to_string))
}

/// The tenant a product belongs to and the `rc_product_prefix` that claims it: the longest prefix wins, as in the
/// RevenueCat webhook. `None` when no tenant's prefix starts the product id.
pub async fn tenant_of_product(db: &impl D1Exec, product_id: &str) -> Result<Option<(String, String)>, AppError> {
    let rows = db
        .rows(
            "SELECT app_id, rc_product_prefix FROM apps
             WHERE rc_product_prefix IS NOT NULL AND rc_product_prefix != ''
               AND instr(?1, rc_product_prefix) = 1
             ORDER BY length(rc_product_prefix) DESC LIMIT 1",
            &[product_id.into()],
        )
        .await?;
    let Some(row) = rows.first() else { return Ok(None) };
    Ok(text(row, "app_id").zip(text(row, "rc_product_prefix")).map(|(app_id, prefix)| (app_id.to_string(), prefix.to_string())))
}

/// The pack id a product resolves to for a tenant: the product must start with the tenant's own `rc_product_prefix`
/// and no other tenant's longer prefix may claim it (the longest prefix wins, as in the RevenueCat webhook). `None`
/// when the product is no pack of this tenant.
pub async fn pack_of_product(db: &impl D1Exec, app_id: &str, product_id: &str) -> Result<Option<String>, AppError> {
    let Some((owner, prefix)) = tenant_of_product(db, product_id).await? else { return Ok(None) };
    if owner != app_id {
        return Ok(None);
    }
    Ok(product_id.strip_prefix(prefix.as_str()).filter(|pack| !pack.is_empty()).map(str::to_string))
}

/// What the credit decision reads from a tenant's `apps` row: the registry slug and App Store bundle id its
/// transactions are matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantFacts {
    pub slug: String,
    pub bundle: Option<String>,
}

/// The facts of a configured tenant, `None` for an `app_id` that is no `apps` row. A tenant without a registry slug
/// uses its `app_id`, as the Midgar access token does.
pub async fn tenant_facts(db: &impl D1Exec, app_id: &str) -> Result<Option<TenantFacts>, AppError> {
    let rows = db.rows("SELECT * FROM apps WHERE app_id = ?1", &[app_id.into()]).await?;
    Ok(rows.first().map(|row| TenantFacts {
        slug: text(row, "registry_slug").filter(|slug| !slug.is_empty()).unwrap_or(app_id).to_string(),
        bundle: text(row, "apple_app_bundle_id").filter(|bundle| !bundle.is_empty()).map(str::to_string),
    }))
}

/// True when the RevenueCat-keyed purchase row of a transaction exists and is no longer `pending`: it was credited
/// (by the webhook or an earlier call) or settled some other way, and is never credited again. A `pending` row is
/// finished by the next call, since the grant under its key is idempotent.
pub fn already_settled(status: Option<&str>) -> bool {
    matches!(status, Some(status) if status != "pending")
}

/// The text of a `refunded` answer. It must not contain the phrases a client without codes reads as a success ("already
/// been processed") or as a retry ("could not be verified"), so that such a client treats it as final.
pub const REFUNDED_MESSAGE: &str = "This purchase was refunded, so it is not credited.";

/// The text of every `credit_pending` answer of the validate route. It is the sentence the fast-track always
/// meant, now visible instead of masked behind a 500.
pub const PENDING_MESSAGE: &str = "Could not verify the purchase right now; it will be credited shortly.";

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "storekit_tests.rs"]
mod tests;
