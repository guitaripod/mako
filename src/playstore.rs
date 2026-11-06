use std::cell::RefCell;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::AppError;
use crate::storekit::{VerifyFailure, PENDING_MESSAGE, REFUNDED_MESSAGE};
use crate::wallet::d1::{text, D1Exec};
use crate::wallet::purchase::PLAY_PAYMENT_PREFIX;

mod google;

pub use google::{FetchTransport, SubtleSigner};

/// The secret holding the JSON key file of the Google Cloud service account that reads Play purchases.
pub const SECRET: &str = "GOOGLE_PLAY_SERVICE_ACCOUNT";

/// The OAuth scope of the Google Play Developer API.
const SCOPE: &str = "https://www.googleapis.com/auth/androidpublisher";

/// Where a service account trades its signed assertion for an access token when its key file names no `token_uri`.
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// The applications resource of the Google Play Developer API.
const APPLICATIONS_URL: &str = "https://androidpublisher.googleapis.com/androidpublisher/v3/applications";

/// The OAuth grant type of a signed service account assertion.
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// Lifetime asked for an assertion; Google accepts at most an hour.
const ASSERTION_SECONDS: u64 = 3600;

/// Lifetime assumed for an access token whose answer names none.
const DEFAULT_TOKEN_SECONDS: u64 = 3600;

/// A cached access token is renewed this long before Google would expire it.
const REFRESH_MARGIN_SECONDS: u64 = 300;

/// Longest purchase token sent to Google; Play's are a few hundred bytes.
const MAX_TOKEN_BYTES: usize = 4096;

/// `purchaseState` of a completed purchase.
const PURCHASED: i64 = 0;

/// `purchaseState` of a purchase the buyer has not paid for yet (a cash or bank payment in progress).
const PENDING: i64 = 2;

/// `acknowledgementState` of an acknowledged purchase.
const ACKNOWLEDGED: i64 = 1;

/// The text of a `not_configured` answer.
pub const NOT_CONFIGURED_MESSAGE: &str = "Google Play purchases cannot be verified for this app yet. Nothing was credited; keep the purchase and try again later.";

/// The parts of a Google Cloud service account key file that sign in to the Play Developer API.
#[derive(Clone, PartialEq, Eq)]
pub struct ServiceAccount {
    pub client_email: String,
    pub key_id: Option<String>,
    pub token_uri: String,
    /// The private key as PKCS #8 DER, decoded from the key file's PEM.
    pub private_key: Vec<u8>,
}

#[derive(Deserialize)]
struct KeyFile {
    client_email: String,
    private_key: String,
    #[serde(default)]
    private_key_id: Option<String>,
    #[serde(default)]
    token_uri: Option<String>,
}

impl ServiceAccount {
    /// Reads a service account key file as Google Cloud downloads it. `None` for anything else, including a key that is
    /// not a PKCS #8 PEM.
    pub fn parse(json: &str) -> Option<Self> {
        let file: KeyFile = serde_json::from_str(json).ok()?;
        let client_email = file.client_email.trim().to_string();
        if client_email.is_empty() {
            return None;
        }
        Some(Self {
            client_email,
            key_id: file.private_key_id.filter(|id| !id.trim().is_empty()),
            token_uri: file.token_uri.filter(|uri| !uri.trim().is_empty()).unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string()),
            private_key: pem_body(&file.private_key)?,
        })
    }
}

/// Decodes the DER inside the `PRIVATE KEY` PEM block a Google key file carries.
fn pem_body(pem: &str) -> Option<Vec<u8>> {
    let body = pem.trim().strip_prefix("-----BEGIN PRIVATE KEY-----")?.strip_suffix("-----END PRIVATE KEY-----")?;
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD.decode(compact).ok().filter(|der| !der.is_empty())
}

/// The fields of a Play `ProductPurchase` the credit decision reads.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ProductPurchase {
    pub purchase_state: Option<i64>,
    pub acknowledgement_state: Option<i64>,
    pub order_id: Option<String>,
    pub product_id: Option<String>,
    pub quantity: Option<i64>,
    pub refundable_quantity: Option<i64>,
    pub obfuscated_external_account_id: Option<String>,
    pub purchase_type: Option<i64>,
}

impl ProductPurchase {
    /// True once the purchase was acknowledged, by the app's consume or by `PlayApi::acknowledge`.
    pub fn acknowledged(&self) -> bool {
        self.acknowledgement_state == Some(ACKNOWLEDGED)
    }

    /// True when the buyer paid through the standard billing flow. Google sets `purchaseType` only for the others: a
    /// license tester's purchase (0), a redeemed promo code (1) and a rewarded ad (2), none of which charged anyone.
    pub fn paid(&self) -> bool {
        self.purchase_type.is_none()
    }

    /// What the purchase is recorded as having cost: the pack's price when it was paid, nothing otherwise, so a test or
    /// promo purchase is credited like an App Store sandbox one but never counted as revenue or attributed to an ad.
    pub fn charged_cents(&self, pack_price_usd_cents: u32) -> u32 {
        if self.paid() {
            pack_price_usd_cents
        } else {
            0
        }
    }
}

/// The access token of one isolate and the service account it was issued to.
#[derive(Debug, Default)]
pub struct TokenCache {
    entry: Option<CachedToken>,
}

#[derive(Debug, Clone)]
struct CachedToken {
    account: String,
    token: String,
    expires_at: u64,
}

impl TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached token of this account while it has more than the refresh margin left.
    fn valid(&self, account: &str, now: u64) -> Option<String> {
        self.entry
            .as_ref()
            .filter(|entry| entry.account == account && now + REFRESH_MARGIN_SECONDS < entry.expires_at)
            .map(|entry| entry.token.clone())
    }

    fn store(&mut self, account: &str, token: &str, expires_at: u64) {
        self.entry = Some(CachedToken { account: account.to_string(), token: token.to_string(), expires_at });
    }

    fn forget(&mut self) {
        self.entry = None;
    }
}

thread_local! {
    static TOKENS: &'static RefCell<TokenCache> = Box::leak(Box::new(RefCell::new(TokenCache::new())));
}

/// The access token cache of this isolate.
pub fn isolate_tokens() -> &'static RefCell<TokenCache> {
    TOKENS.with(|cache| *cache)
}

/// The HTTP method of a call to Google.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Get,
    Post,
}

/// One request to Google, as the verification sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub verb: Verb,
    pub url: String,
    pub bearer: Option<String>,
    pub content_type: Option<&'static str>,
    pub body: Option<String>,
}

impl Call {
    fn get(url: String) -> Self {
        Self { verb: Verb::Get, url, bearer: None, content_type: None, body: None }
    }

    fn post(url: String, content_type: &'static str, body: String) -> Self {
        Self { verb: Verb::Post, url, bearer: None, content_type: Some(content_type), body: Some(body) }
    }

    fn with_bearer(&self, token: &str) -> Self {
        Self { bearer: Some(token.to_string()), ..self.clone() }
    }
}

/// Google's answer: the status and the JSON body, `Null` when the body is not JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub status: u16,
    pub body: Value,
}

/// Anything that can send a call to Google; the real one fetches, tests answer from a script.
#[allow(async_fn_in_trait)]
pub trait Transport {
    async fn send(&self, call: &Call) -> Result<Answer, String>;
}

/// Anything that can sign RS256 with a PKCS #8 RSA key; the real one is the runtime's `SubtleCrypto`.
#[allow(async_fn_in_trait)]
pub trait Signer {
    async fn sign(&self, pkcs8: &[u8], input: &[u8]) -> Result<Vec<u8>, String>;
}

/// The Google Play Developer API as one service account sees it at one moment.
pub struct PlayApi<'a, T: Transport, S: Signer> {
    pub account: &'a ServiceAccount,
    pub transport: &'a T,
    pub signer: &'a S,
    pub tokens: &'a RefCell<TokenCache>,
    pub now: u64,
}

fn unavailable(reason: impl Into<String>) -> VerifyFailure {
    VerifyFailure::Unavailable(reason.into())
}

impl<T: Transport, S: Signer> PlayApi<'_, T, S> {
    /// `purchases.products.get`. A token Google says is no purchase of the product (400, 404, 410) is `Rejected`;
    /// everything else that is not a readable purchase (403 before the service account has Play Console access, 429,
    /// 5xx, no answer) is `Unavailable`, which must never read as a verdict on the purchase.
    pub async fn product_purchase(&self, package: &str, product_id: &str, token: &str) -> Result<ProductPurchase, VerifyFailure> {
        let answer = self.authorized(&Call::get(purchase_url(package, product_id, token))).await?;
        match answer.status {
            200 => serde_json::from_value(answer.body).map_err(|_| unavailable("Google answered an unreadable purchase")),
            400 | 404 | 410 => Err(VerifyFailure::Rejected),
            other => Err(unavailable(format!("Google answered {other} for the purchase{}", google_reason(&answer.body)))),
        }
    }

    /// `purchases.products.acknowledge`, which keeps Google from refunding a credited purchase the app has not consumed
    /// yet (it refunds an unacknowledged one after three days).
    pub async fn acknowledge(&self, package: &str, product_id: &str, token: &str) -> Result<(), VerifyFailure> {
        let url = format!("{}:acknowledge", purchase_url(package, product_id, token));
        let answer = self.authorized(&Call::post(url, "application/json", "{}".to_string())).await?;
        match answer.status {
            200 | 204 => Ok(()),
            other => Err(unavailable(format!("Google answered {other} to the acknowledgement{}", google_reason(&answer.body)))),
        }
    }

    /// Sends a call with the cached access token, and once more with a new one when Google refuses the cached one.
    async fn authorized(&self, call: &Call) -> Result<Answer, VerifyFailure> {
        let token = self.access_token().await?;
        let answer = self.send(&call.with_bearer(&token)).await?;
        if answer.status != 401 {
            return Ok(answer);
        }
        self.tokens.borrow_mut().forget();
        let token = self.access_token().await?;
        self.send(&call.with_bearer(&token)).await
    }

    async fn send(&self, call: &Call) -> Result<Answer, VerifyFailure> {
        self.transport.send(call).await.map_err(|e| unavailable(format!("Google could not be reached: {e}")))
    }

    /// The isolate's access token for this account, or a new one from a freshly signed assertion.
    async fn access_token(&self) -> Result<String, VerifyFailure> {
        let cached = self.tokens.borrow().valid(&self.account.client_email, self.now);
        if let Some(token) = cached {
            return Ok(token);
        }
        let assertion = self.assertion().await?;
        let form = format!("grant_type={}&assertion={}", encode(JWT_BEARER), encode(&assertion));
        let answer = self.send(&Call::post(self.account.token_uri.clone(), "application/x-www-form-urlencoded", form)).await?;
        let (token, lifetime) = read_token(&answer)?;
        self.tokens.borrow_mut().store(&self.account.client_email, &token, self.now + lifetime);
        Ok(token)
    }

    /// The service account's signed JWT assertion for the Play Developer API scope.
    pub async fn assertion(&self) -> Result<String, VerifyFailure> {
        let input = signing_input(self.account, self.now);
        let signature = self
            .signer
            .sign(&self.account.private_key, input.as_bytes())
            .await
            .map_err(|e| unavailable(format!("the service account key could not sign: {e}")))?;
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

/// The header and claims of an assertion, encoded and joined as the signature covers them.
fn signing_input(account: &ServiceAccount, now: u64) -> String {
    let mut header = json!({ "alg": "RS256", "typ": "JWT" });
    if let Some(key_id) = &account.key_id {
        header["kid"] = json!(key_id);
    }
    let claims = json!({
        "iss": account.client_email,
        "scope": SCOPE,
        "aud": account.token_uri,
        "iat": now,
        "exp": now + ASSERTION_SECONDS,
    });
    format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()))
}

/// The access token and its lifetime from the token endpoint's answer.
fn read_token(answer: &Answer) -> Result<(String, u64), VerifyFailure> {
    let token = answer.body.get("access_token").and_then(Value::as_str).filter(|token| !token.is_empty());
    match (answer.status, token) {
        (200, Some(token)) => {
            let lifetime = answer.body.get("expires_in").and_then(Value::as_u64).unwrap_or(DEFAULT_TOKEN_SECONDS);
            Ok((token.to_string(), lifetime))
        }
        (status, _) => Err(unavailable(format!("Google refused the service account sign-in ({status}){}", google_reason(&answer.body)))),
    }
}

/// Google's own words for a failure, for the log: the OAuth `error` or the API's `error.status`. Neither carries a secret.
fn google_reason(body: &Value) -> String {
    let reason = body
        .get("error")
        .and_then(|error| error.as_str().or_else(|| error.get("status").and_then(Value::as_str)))
        .unwrap_or_default();
    if reason.is_empty() {
        String::new()
    } else {
        format!(": {reason}")
    }
}

fn purchase_url(package: &str, product_id: &str, token: &str) -> String {
    format!("{APPLICATIONS_URL}/{}/purchases/products/{}/tokens/{}", encode(package), encode(product_id), encode(token))
}

/// Percent-encodes everything but RFC 3986's unreserved characters, for a path segment or a form value.
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// True for something shaped like a Play purchase token: letters, digits, `.`, `_` and `-`, with at least one letter or
/// digit, so it can never be a path of its own (`..`).
pub fn plausible_token(token: &str) -> bool {
    token.len() <= MAX_TOKEN_BYTES
        && token.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && token.bytes().any(|b| b.is_ascii_alphanumeric())
}

/// What a verified Play purchase has to match to be credited as a pack.
#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
    pub product_id: &'a str,
    /// The caller's mako user id, which the app passes to Play as the obfuscated account id.
    pub user_id: &'a str,
    /// The order id the app saw, if it sent one.
    pub order_id: Option<&'a str>,
}

/// Why a verified Play purchase is not credited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Pending,
    Voided,
    WrongProduct,
    WrongOrder,
    NotYours,
    Quantity,
}

impl Refusal {
    /// The error a refusal answers with. A purchase still being paid is `credit_pending`; a canceled or refunded one is
    /// `refunded`, final; one made by another account is 403; anything else that is no creditable pack is 400.
    pub fn into_error(self) -> AppError {
        match self {
            Refusal::Pending => AppError::CreditPending(PENDING_MESSAGE.to_string()),
            Refusal::Voided => AppError::Refunded(REFUNDED_MESSAGE.to_string()),
            Refusal::WrongProduct => AppError::BadRequest("The purchase is not the product being credited.".to_string()),
            Refusal::WrongOrder => AppError::BadRequest("The order id does not match the purchase.".to_string()),
            Refusal::NotYours => AppError::Forbidden("The purchase was not made by this account.".to_string()),
            Refusal::Quantity => AppError::BadRequest("Only a purchase of one pack is credited.".to_string()),
        }
    }
}

/// Decides whether a purchase Google answered may be credited: it is paid (`purchaseState` 0) and not refunded, it is
/// the product the request names, one pack, made by the caller when the app named the account, and its order is the
/// one the app saw when it sent one. Purchases by license testers count like any other, as App Store sandbox purchases
/// do, so Play review and internal testers are credited.
pub fn check(purchase: &ProductPurchase, expected: &Expected<'_>) -> Result<(), Refusal> {
    match purchase.purchase_state {
        Some(PURCHASED) => {}
        Some(PENDING) => return Err(Refusal::Pending),
        _ => return Err(Refusal::Voided),
    }
    if purchase.refundable_quantity == Some(0) {
        return Err(Refusal::Voided);
    }
    if purchase.product_id.as_deref().is_some_and(|product| product != expected.product_id) {
        return Err(Refusal::WrongProduct);
    }
    if purchase.quantity.unwrap_or(1) != 1 {
        return Err(Refusal::Quantity);
    }
    if let Some(account) = purchase.obfuscated_external_account_id.as_deref().filter(|account| !account.is_empty()) {
        if !account.eq_ignore_ascii_case(expected.user_id) {
            return Err(Refusal::NotYours);
        }
    }
    let claimed = expected.order_id.filter(|order| !order.is_empty());
    if let (Some(claimed), Some(order)) = (claimed, purchase.order_id.as_deref()) {
        if claimed != order {
            return Err(Refusal::WrongOrder);
        }
    }
    Ok(())
}

/// The payment id a Play purchase is recorded and granted under (`tx:` plus this): `gp:<orderId>`, or for a purchase
/// without an order (a redeemed promo code) `gp:token:<SHA-256 of its token>`, so the token itself is never stored.
pub fn payment_id(purchase: &ProductPurchase, token: &str) -> String {
    match purchase.order_id.as_deref().map(str::trim).filter(|order| !order.is_empty()) {
        Some(order) => format!("{PLAY_PAYMENT_PREFIX}{order}"),
        None => {
            let digest = Sha256::digest(token.as_bytes());
            let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("{PLAY_PAYMENT_PREFIX}token:{hex}")
        }
    }
}

/// Asks Google about the purchase and checks it against the request. A token that cannot be a purchase or that Google
/// does not know is `unverifiable_purchase`; Google that cannot be asked is `credit_pending`, and the app asks again;
/// a refusal is its own error.
pub async fn vet<T: Transport, S: Signer>(
    api: &PlayApi<'_, T, S>,
    package: &str,
    token: &str,
    expected: &Expected<'_>,
) -> Result<ProductPurchase, AppError> {
    if !plausible_token(token) {
        return Err(AppError::UnverifiablePurchase("Purchase could not be verified.".to_string()));
    }
    let purchase = match api.product_purchase(package, expected.product_id, token).await {
        Ok(purchase) => purchase,
        Err(VerifyFailure::Rejected) => return Err(AppError::UnverifiablePurchase("Purchase could not be verified.".to_string())),
        Err(VerifyFailure::Unavailable(reason)) => {
            crate::wallet::log::error(&format!("google play verification unavailable: {reason}"));
            return Err(AppError::CreditPending(PENDING_MESSAGE.to_string()));
        }
    };
    check(&purchase, expected).map_err(Refusal::into_error)?;
    Ok(purchase)
}

/// The Play package name configured for a tenant (`apps.play_package_name`), if any. A database without the column
/// reads as none, so the code may deploy before its migration.
pub async fn package_of(db: &impl D1Exec, app_id: &str) -> Result<Option<String>, AppError> {
    let rows = db.rows("SELECT * FROM apps WHERE app_id = ?1", &[app_id.into()]).await?;
    Ok(rows.first().and_then(|row| text(row, "play_package_name")).filter(|package| !package.is_empty()).map(str::to_string))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "playstore_tests.rs"]
mod tests;
