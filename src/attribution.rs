use worker::{console_log, wasm_bindgen::JsValue, D1Database, Fetch, Headers, Method, Request, RequestInit, Result};
use sha2::{Digest, Sha256};
use chrono::Utc;

use crate::error::AppError;
use crate::models::{AdServicesAttribution, AttributionTotals, InstallAttribution, KeywordPerformance};

const ADSERVICES_ENDPOINT: &str = "https://api-adservices.apple.com/api/v1/";
const MAX_TOKEN_LEN: usize = 8192;
const MIN_TOKEN_LEN: usize = 16;
const WINDOW_MIN: &str = "1970-01-01T00:00:00+00:00";
const WINDOW_MAX: &str = "9999-12-31T23:59:59+00:00";

/// Result of exchanging an AdServices token with Apple.
#[derive(Debug, PartialEq, Eq)]
pub enum ExchangeOutcome {
    /// Apple answered with a definitive record (campaign data, or `attribution: false`).
    Resolved(AdServicesAttribution),
    /// The record has not propagated yet, or Apple was briefly unavailable. The
    /// client may submit the same token again shortly.
    Retryable(&'static str),
    /// Apple will never resolve this token (malformed, expired, already consumed).
    Terminal(&'static str),
}

/// Outcome of persisting an install against an identity.
#[derive(Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Recorded(InstallAttribution),
    AlreadyRecorded(InstallAttribution),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurchaseKind {
    CreditPack,
    Subscription,
    Unknown,
}

impl PurchaseKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PurchaseKind::CreditPack => "credit_pack",
            PurchaseKind::Subscription => "subscription",
            PurchaseKind::Unknown => "unknown",
        }
    }
}

/// Shape-checks a client-supplied AdServices token before it is sent anywhere.
/// The token is an opaque base64 blob; this rejects the obviously-not-a-token
/// cases locally so a bad client cannot make us hammer Apple.
pub fn validate_token_shape(token: &str) -> std::result::Result<(), &'static str> {
    let token = token.trim();
    if token.len() < MIN_TOKEN_LEN {
        return Err("token too short");
    }
    if token.len() > MAX_TOKEN_LEN {
        return Err("token too long");
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_'))
    {
        return Err("token is not base64");
    }
    Ok(())
}

/// SHA-256 hex fingerprint of the token. The raw token is never stored or logged;
/// the fingerprint exists solely so the same token submitted twice is recognised
/// as one install.
pub fn token_fingerprint(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.trim().as_bytes());
    hex::encode(hasher.finalize())
}

/// Maps Apple's HTTP status onto a retry decision. 404 means "not propagated
/// yet" and is expected for seconds after an install; 400 means the token is
/// malformed or already spent and will never resolve.
pub fn classify_exchange(status: u16, body: &str) -> ExchangeOutcome {
    match status {
        200 => match parse_attribution(body) {
            Ok(attribution) => ExchangeOutcome::Resolved(attribution),
            Err(reason) => ExchangeOutcome::Retryable(reason),
        },
        400 => ExchangeOutcome::Terminal("apple rejected the token"),
        404 => ExchangeOutcome::Retryable("attribution record not available yet"),
        429 => ExchangeOutcome::Retryable("apple rate limited the exchange"),
        s if (500..600).contains(&s) => ExchangeOutcome::Retryable("apple server error"),
        _ => ExchangeOutcome::Retryable("unexpected apple response"),
    }
}

/// Parses Apple's attribution payload. Only campaign-shaped fields are read;
/// anything else Apple adds is ignored.
pub fn parse_attribution(body: &str) -> std::result::Result<AdServicesAttribution, &'static str> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|_| "apple response was not json")?;
    let attributed = value
        .get("attribution")
        .and_then(|v| v.as_bool())
        .ok_or("apple response had no attribution flag")?;

    if !attributed || is_sample_payload(&value) {
        return Ok(AdServicesAttribution::default());
    }

    Ok(AdServicesAttribution {
        attributed: true,
        org_id: value.get("orgId").and_then(|v| v.as_i64()),
        campaign_id: value.get("campaignId").and_then(|v| v.as_i64()),
        ad_group_id: value.get("adGroupId").and_then(|v| v.as_i64()),
        keyword_id: value.get("keywordId").and_then(|v| v.as_i64()),
        ad_id: value.get("adId").and_then(|v| v.as_i64()),
        country_or_region: value
            .get("countryOrRegion")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        click_date: value.get("clickDate").and_then(|v| v.as_str()).map(|s| s.to_string()),
        conversion_type: value
            .get("conversionType")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

/// Campaign id in the fixed sample payload AdServices returns for development
/// and TestFlight builds, where no real ad tap can exist.
const SAMPLE_CAMPAIGN_ID: i64 = 1_234_567_890;

/// Whether Apple answered with its development-build sample payload rather
/// than a real ad tap. Recording it as paid would credit a campaign that never
/// ran with an install that only a developer's own build produced.
fn is_sample_payload(value: &serde_json::Value) -> bool {
    value.get("campaignId").and_then(|v| v.as_i64()) == Some(SAMPLE_CAMPAIGN_ID)
}

/// Exchanges the token with Apple. The token travels in the request body as
/// `text/plain` per the AdServices contract and is never logged.
pub async fn exchange_token(token: &str) -> ExchangeOutcome {
    let headers = Headers::new();
    if headers.set("Content-Type", "text/plain").is_err() {
        return ExchangeOutcome::Retryable("could not build exchange request");
    }

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(token.trim())));

    let request = match Request::new_with_init(ADSERVICES_ENDPOINT, &init) {
        Ok(r) => r,
        Err(_) => return ExchangeOutcome::Retryable("could not build exchange request"),
    };

    let mut response = match Fetch::Request(request).send().await {
        Ok(r) => r,
        Err(e) => {
            console_log!("adservices exchange transport error: {:?}", e);
            return ExchangeOutcome::Retryable("apple unreachable");
        }
    };

    let status = response.status_code();
    let body = response.text().await.unwrap_or_default();
    let outcome = classify_exchange(status, &body);
    console_log!("adservices exchange status={} outcome={}", status, outcome_label(&outcome));
    outcome
}

fn outcome_label(outcome: &ExchangeOutcome) -> &'static str {
    match outcome {
        ExchangeOutcome::Resolved(a) if a.attributed => "attributed",
        ExchangeOutcome::Resolved(_) => "organic",
        ExchangeOutcome::Retryable(_) => "retryable",
        ExchangeOutcome::Terminal(_) => "terminal",
    }
}

/// Persists one install against an existing identity. Idempotent twice over:
/// the primary key `(app_id, user_id)` keeps first-touch attribution for an
/// identity, and the unique `(app_id, token_hash)` index means the same token
/// can never be counted as two installs.
pub async fn record_install(
    db: &D1Database,
    app_id: &str,
    user_id: &str,
    token_hash: &str,
    attribution: &AdServicesAttribution,
) -> Result<InstallOutcome> {
    if let Some(existing) = find_install_by_token(db, app_id, token_hash).await? {
        return Ok(InstallOutcome::AlreadyRecorded(existing));
    }
    if let Some(existing) = find_install_by_user(db, app_id, user_id).await? {
        return Ok(InstallOutcome::AlreadyRecorded(existing));
    }

    let now = Utc::now().to_rfc3339();
    db.prepare(
        "INSERT OR IGNORE INTO install_attributions
            (app_id, user_id, token_hash, attributed, org_id, campaign_id, ad_group_id, keyword_id,
             ad_id, country_or_region, click_date, conversion_type, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )
    .bind(&[
        app_id.into(),
        user_id.into(),
        token_hash.into(),
        (if attribution.attributed { 1 } else { 0 }).into(),
        opt_num(attribution.org_id),
        opt_num(attribution.campaign_id),
        opt_num(attribution.ad_group_id),
        opt_num(attribution.keyword_id),
        opt_num(attribution.ad_id),
        opt_text(attribution.country_or_region.as_deref()),
        opt_text(attribution.click_date.as_deref()),
        opt_text(attribution.conversion_type.as_deref()),
        now.clone().into(),
    ])?
    .run()
    .await?;

    let stored = match find_install_by_user(db, app_id, user_id).await? {
        Some(stored) => Some(stored),
        None => find_install_by_token(db, app_id, token_hash).await?,
    };

    match stored {
        Some(stored) if stored.created_at == now => Ok(InstallOutcome::Recorded(stored)),
        Some(stored) => Ok(InstallOutcome::AlreadyRecorded(stored)),
        None => Err(AppError::InternalError("install attribution was not persisted".to_string()).into()),
    }
}

pub async fn find_install_by_token(
    db: &D1Database,
    app_id: &str,
    token_hash: &str,
) -> Result<Option<InstallAttribution>> {
    let row = db
        .prepare(
            "SELECT app_id, user_id, attributed, campaign_id, ad_group_id, keyword_id, country_or_region, created_at
             FROM install_attributions WHERE app_id = ?1 AND token_hash = ?2",
        )
        .bind(&[app_id.into(), token_hash.into()])?
        .first::<serde_json::Value>(None)
        .await?;
    Ok(row.as_ref().and_then(parse_install_row))
}

pub async fn find_install_by_user(
    db: &D1Database,
    app_id: &str,
    user_id: &str,
) -> Result<Option<InstallAttribution>> {
    let row = db
        .prepare(
            "SELECT app_id, user_id, attributed, campaign_id, ad_group_id, keyword_id, country_or_region, created_at
             FROM install_attributions WHERE app_id = ?1 AND user_id = ?2",
        )
        .bind(&[app_id.into(), user_id.into()])?
        .first::<serde_json::Value>(None)
        .await?;
    Ok(row.as_ref().and_then(parse_install_row))
}

/// Copies the buyer's campaign and keyword onto a validated purchase. A no-op
/// for identities with no attribution row, and idempotent on `purchase_id`, so
/// a replayed webhook cannot inflate a keyword's payer or revenue counts.
pub async fn tag_purchase(
    db: &D1Database,
    app_id: &str,
    user_id: &str,
    purchase_id: &str,
    kind: PurchaseKind,
    product_id: &str,
    amount_usd_cents: i64,
    net_usd_cents: Option<i64>,
) -> Result<bool> {
    let Some(install) = find_install_by_user(db, app_id, user_id).await? else {
        return Ok(false);
    };
    if !install.attributed {
        return Ok(false);
    }

    db.prepare(
        "INSERT OR IGNORE INTO purchase_attributions
            (purchase_id, app_id, user_id, kind, product_id, campaign_id, ad_group_id, keyword_id,
             country_or_region, amount_usd_cents, net_usd_cents, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .bind(&[
        purchase_id.into(),
        app_id.into(),
        user_id.into(),
        kind.as_str().into(),
        opt_text(if product_id.is_empty() { None } else { Some(product_id) }),
        opt_num(install.campaign_id),
        opt_num(install.ad_group_id),
        opt_num(install.keyword_id),
        opt_text(install.country_or_region.as_deref()),
        (amount_usd_cents as f64).into(),
        opt_num(net_usd_cents),
        Utc::now().to_rfc3339().into(),
    ])?
    .run()
    .await?;

    Ok(true)
}

/// Moves an install (and any purchases already tagged against it) from a
/// discarded anonymous identity onto the identity that survives a Sign in with
/// Apple merge.
///
/// Without this the keyword report silently understates conversion: the install
/// row stays on the abandoned `user_id` while every later purchase is recorded
/// against the surviving one, so the `(app_id, user_id)` join finds no payer and
/// a keyword that actually produced a paying customer reads as installs-with-zero-
/// payers — which is exactly the shape that trips the kill rule.
///
/// When the surviving identity already carries an install, its own first touch
/// wins and the orphan is dropped rather than left to inflate the install count
/// with a payer it can never gain.
pub async fn carry_attribution_across_merge(
    db: &D1Database,
    app_id: &str,
    from_user_id: &str,
    to_user_id: &str,
) -> Result<()> {
    if from_user_id == to_user_id {
        return Ok(());
    }
    if find_install_by_user(db, app_id, from_user_id).await?.is_none() {
        return Ok(());
    }

    if find_install_by_user(db, app_id, to_user_id).await?.is_some() {
        db.prepare("DELETE FROM install_attributions WHERE app_id = ?1 AND user_id = ?2")
            .bind(&[app_id.into(), from_user_id.into()])?
            .run()
            .await?;
    } else {
        db.prepare("UPDATE install_attributions SET user_id = ?3 WHERE app_id = ?1 AND user_id = ?2")
            .bind(&[app_id.into(), from_user_id.into(), to_user_id.into()])?
            .run()
            .await?;
    }

    db.prepare("UPDATE purchase_attributions SET user_id = ?3 WHERE app_id = ?1 AND user_id = ?2")
        .bind(&[app_id.into(), from_user_id.into(), to_user_id.into()])?
        .run()
        .await?;

    console_log!("carried ad attribution from {} to {}", from_user_id, to_user_id);
    Ok(())
}

/// The tenant's post-commission share of a store sale, when configured. Used to
/// net out purchases RevenueCat reported no `takehome_percentage` for; absent, the
/// report says net is unknown rather than guessing a commission.
pub async fn store_net_share(db: &D1Database, app_id: &str) -> Option<f64> {
    db.prepare("SELECT store_net_share FROM apps WHERE app_id = ?1")
        .bind(&[app_id.into()])
        .ok()?
        .first::<serde_json::Value>(None)
        .await
        .ok()??
        .get("store_net_share")
        .and_then(|v| v.as_f64())
}

/// Per-keyword spend-relevant counts for installs inside `[since, until)`.
/// Purchases are counted for the whole lifetime of that install cohort, so a
/// keyword's payers include buyers who converted after the window closed.
pub async fn keyword_report(
    db: &D1Database,
    app_id: &str,
    since: &str,
    until: &str,
    net_share: Option<f64>,
) -> Result<Vec<KeywordPerformance>> {
    let rows = db
        .prepare(
            "SELECT ia.campaign_id AS campaign_id,
                    ia.ad_group_id AS ad_group_id,
                    ia.keyword_id AS keyword_id,
                    MIN(ia.country_or_region) AS country_or_region,
                    COUNT(DISTINCT ia.user_id) AS installs,
                    COUNT(DISTINCT pa.user_id) AS payers,
                    COUNT(pa.purchase_id) AS purchases,
                    COALESCE(SUM(pa.amount_usd_cents), 0) AS revenue_usd_cents,
                    COALESCE(SUM(pa.net_usd_cents), 0) AS reported_net_usd_cents,
                    SUM(CASE WHEN pa.purchase_id IS NOT NULL AND pa.net_usd_cents IS NULL THEN 1 ELSE 0 END) AS purchases_missing_net,
                    COALESCE(SUM(CASE WHEN pa.net_usd_cents IS NULL THEN pa.amount_usd_cents ELSE 0 END), 0) AS revenue_missing_net_usd_cents,
                    MIN(ia.created_at) AS first_install_at,
                    MAX(ia.created_at) AS last_install_at
             FROM install_attributions ia
             LEFT JOIN purchase_attributions pa
                    ON pa.app_id = ia.app_id AND pa.user_id = ia.user_id
             WHERE ia.app_id = ?1 AND ia.attributed = 1
               AND ia.created_at >= ?2 AND ia.created_at < ?3
             GROUP BY ia.campaign_id, ia.ad_group_id, ia.keyword_id
             ORDER BY installs DESC, revenue_usd_cents DESC",
        )
        .bind(&[app_id.into(), since.into(), until.into()])?
        .all()
        .await?
        .results::<serde_json::Value>()?;

    Ok(rows.iter().map(|row| parse_keyword_row(row, net_share)).collect())
}

/// Window totals: install mix (attributed vs organic) plus the attributed
/// cohort's payers and revenue.
pub async fn attribution_totals(
    db: &D1Database,
    app_id: &str,
    since: &str,
    until: &str,
    net_share: Option<f64>,
) -> Result<AttributionTotals> {
    let installs = db
        .prepare(
            "SELECT COUNT(*) AS total_installs,
                    SUM(CASE WHEN attributed = 1 THEN 1 ELSE 0 END) AS attributed_installs,
                    SUM(CASE WHEN attributed = 0 THEN 1 ELSE 0 END) AS organic_installs
             FROM install_attributions
             WHERE app_id = ?1 AND created_at >= ?2 AND created_at < ?3",
        )
        .bind(&[app_id.into(), since.into(), until.into()])?
        .first::<serde_json::Value>(None)
        .await?
        .unwrap_or(serde_json::Value::Null);

    let payers = db
        .prepare(
            "SELECT COUNT(DISTINCT pa.user_id) AS payers,
                    COUNT(pa.purchase_id) AS purchases,
                    COALESCE(SUM(pa.amount_usd_cents), 0) AS revenue_usd_cents,
                    COALESCE(SUM(pa.net_usd_cents), 0) AS reported_net_usd_cents,
                    SUM(CASE WHEN pa.net_usd_cents IS NULL THEN 1 ELSE 0 END) AS purchases_missing_net,
                    COALESCE(SUM(CASE WHEN pa.net_usd_cents IS NULL THEN pa.amount_usd_cents ELSE 0 END), 0) AS revenue_missing_net_usd_cents
             FROM install_attributions ia
             JOIN purchase_attributions pa
                    ON pa.app_id = ia.app_id AND pa.user_id = ia.user_id
             WHERE ia.app_id = ?1 AND ia.attributed = 1
               AND ia.created_at >= ?2 AND ia.created_at < ?3",
        )
        .bind(&[app_id.into(), since.into(), until.into()])?
        .first::<serde_json::Value>(None)
        .await?
        .unwrap_or(serde_json::Value::Null);

    let payer_count = num(&payers, "payers");
    let revenue = num(&payers, "revenue_usd_cents");
    let reported_net = num(&payers, "reported_net_usd_cents");
    let revenue_missing_net = num(&payers, "revenue_missing_net_usd_cents");
    let net = effective_net_cents(reported_net, revenue_missing_net, net_share);

    Ok(AttributionTotals {
        total_installs: num(&installs, "total_installs"),
        attributed_installs: num(&installs, "attributed_installs"),
        organic_installs: num(&installs, "organic_installs"),
        payers: payer_count,
        purchases: num(&payers, "purchases"),
        revenue_usd_cents: revenue,
        reported_net_usd_cents: reported_net,
        purchases_missing_net: num(&payers, "purchases_missing_net"),
        revenue_missing_net_usd_cents: revenue_missing_net,
        net_revenue_usd_cents: net,
        net_per_payer_usd_cents: net.and_then(|n| per_payer_cents(n, payer_count)),
        revenue_per_payer_usd_cents: per_payer_cents(revenue, payer_count),
        store_net_share: net_share,
    })
}

/// A store price in USD as integer cents. RevenueCat omits the price on some
/// events; an unknown price contributes zero revenue rather than a guess.
pub fn usd_to_cents(price: Option<f64>) -> i64 {
    match price {
        Some(p) if p.is_finite() && p > 0.0 => (p * 100.0).round() as i64,
        _ => 0,
    }
}

/// Net (post-store-commission) cents, using RevenueCat's `takehome_percentage`.
/// `None` when RevenueCat did not send a share, so the report can say how much of
/// its net figure is unknown instead of inventing a commission.
pub fn net_cents(gross_cents: i64, takehome_percentage: Option<f64>) -> Option<i64> {
    let share = takehome_percentage?;
    if !share.is_finite() || share <= 0.0 || share > 1.0 {
        return None;
    }
    Some((gross_cents as f64 * share).round() as i64)
}

/// Total net revenue: RevenueCat-reported net, plus the tenant's configured share
/// of any purchase RevenueCat gave no commission for. `None` when a purchase has
/// no reported net and no `apps.store_net_share` is set — an unknown net must read
/// as unknown, never as zero, or a profitable keyword looks like a dead one.
pub fn effective_net_cents(
    reported_net_cents: i64,
    revenue_missing_net_cents: i64,
    store_net_share: Option<f64>,
) -> Option<i64> {
    if revenue_missing_net_cents == 0 {
        return Some(reported_net_cents);
    }
    let share = store_net_share?;
    if !share.is_finite() || share <= 0.0 || share > 1.0 {
        return None;
    }
    Some(reported_net_cents + (revenue_missing_net_cents as f64 * share).round() as i64)
}

/// Net revenue divided by payers — the denominator of the "cost per payer must
/// stay under 60% of net per payer" kill rule. `None` when nobody paid yet, so
/// a zero-payer keyword can never look profitable.
pub fn per_payer_cents(net_usd_cents: i64, payers: i64) -> Option<i64> {
    if payers <= 0 {
        return None;
    }
    Some(net_usd_cents / payers)
}

/// Clamps an optional caller-supplied window to comparable RFC3339 bounds.
/// Bounds are compared lexicographically against stored RFC3339 timestamps, so a
/// bare date works as-is; `until` is exclusive.
pub fn normalize_window(
    since: Option<&str>,
    until: Option<&str>,
) -> std::result::Result<(String, String), &'static str> {
    let since = normalize_bound(since, WINDOW_MIN)?;
    let until = normalize_bound(until, WINDOW_MAX)?;
    if since >= until {
        return Err("since must be before until");
    }
    Ok((since, until))
}

fn normalize_bound(value: Option<&str>, default: &str) -> std::result::Result<String, &'static str> {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(default.to_string());
    };
    if raw.len() < 4 || raw.len() > 40 {
        return Err("window bound must be an ISO-8601 date or timestamp");
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b':' | b'T' | b'Z' | b'+' | b'.' | b' '))
    {
        return Err("window bound must be an ISO-8601 date or timestamp");
    }
    if !raw.as_bytes()[..4].iter().all(|b| b.is_ascii_digit()) {
        return Err("window bound must be an ISO-8601 date or timestamp");
    }
    Ok(raw.to_string())
}

fn parse_install_row(row: &serde_json::Value) -> Option<InstallAttribution> {
    Some(InstallAttribution {
        app_id: row.get("app_id")?.as_str()?.to_string(),
        user_id: row.get("user_id")?.as_str()?.to_string(),
        attributed: row.get("attributed").and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        campaign_id: row.get("campaign_id").and_then(|v| v.as_i64()),
        ad_group_id: row.get("ad_group_id").and_then(|v| v.as_i64()),
        keyword_id: row.get("keyword_id").and_then(|v| v.as_i64()),
        country_or_region: row
            .get("country_or_region")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        created_at: row
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

fn parse_keyword_row(row: &serde_json::Value, net_share: Option<f64>) -> KeywordPerformance {
    let payers = num(row, "payers");
    let revenue_usd_cents = num(row, "revenue_usd_cents");
    let reported_net_usd_cents = num(row, "reported_net_usd_cents");
    let revenue_missing_net_usd_cents = num(row, "revenue_missing_net_usd_cents");
    let net_revenue_usd_cents =
        effective_net_cents(reported_net_usd_cents, revenue_missing_net_usd_cents, net_share);
    KeywordPerformance {
        campaign_id: row.get("campaign_id").and_then(|v| v.as_i64()),
        ad_group_id: row.get("ad_group_id").and_then(|v| v.as_i64()),
        keyword_id: row.get("keyword_id").and_then(|v| v.as_i64()),
        country_or_region: row
            .get("country_or_region")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        installs: num(row, "installs"),
        payers,
        purchases: num(row, "purchases"),
        revenue_usd_cents,
        reported_net_usd_cents,
        purchases_missing_net: num(row, "purchases_missing_net"),
        revenue_missing_net_usd_cents,
        net_revenue_usd_cents,
        net_per_payer_usd_cents: net_revenue_usd_cents.and_then(|n| per_payer_cents(n, payers)),
        revenue_per_payer_usd_cents: per_payer_cents(revenue_usd_cents, payers),
        first_install_at: row
            .get("first_install_at")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        last_install_at: row
            .get("last_install_at")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

fn num(row: &serde_json::Value, key: &str) -> i64 {
    row.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
}

fn opt_num(value: Option<i64>) -> JsValue {
    match value {
        Some(n) => (n as f64).into(),
        None => JsValue::NULL,
    }
}

fn opt_text(value: Option<&str>) -> JsValue {
    match value {
        Some(s) => s.into(),
        None => JsValue::NULL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATTRIBUTED_BODY: &str = r#"{
        "attribution": true,
        "orgId": 40669820,
        "campaignId": 542370539,
        "conversionType": "Download",
        "clickDate": "2026-07-28T17:17Z",
        "adGroupId": 542317095,
        "countryOrRegion": "US",
        "keywordId": 87675432,
        "adId": 542317136
    }"#;

    #[test]
    fn parses_a_keyword_attribution() {
        let parsed = parse_attribution(ATTRIBUTED_BODY).unwrap();
        assert!(parsed.attributed);
        assert_eq!(parsed.campaign_id, Some(542370539));
        assert_eq!(parsed.ad_group_id, Some(542317095));
        assert_eq!(parsed.keyword_id, Some(87675432));
        assert_eq!(parsed.country_or_region.as_deref(), Some("US"));
        assert_eq!(parsed.conversion_type.as_deref(), Some("Download"));
    }

    #[test]
    fn development_sample_payload_is_not_a_paid_install() {
        let body = r#"{"attribution": true, "orgId": 1234567890, "campaignId": 1234567890,
            "conversionType": "Download", "clickDate": "2026-09-24T21:29Z",
            "adGroupId": 1234567890, "countryOrRegion": "US", "keywordId": 12323222, "adId": 1234567890}"#;
        let parsed = parse_attribution(body).unwrap();
        assert!(!parsed.attributed);
        assert_eq!(parsed.campaign_id, None);
    }

    #[test]
    fn parses_organic_install_without_campaign_data() {
        let parsed = parse_attribution(r#"{"attribution": false}"#).unwrap();
        assert!(!parsed.attributed);
        assert_eq!(parsed.campaign_id, None);
        assert_eq!(parsed.keyword_id, None);
        assert_eq!(parsed.country_or_region, None);
    }

    #[test]
    fn attribution_without_keyword_is_still_attributed() {
        let body = r#"{"attribution": true, "campaignId": 1, "adGroupId": 2, "countryOrRegion": "DE"}"#;
        let parsed = parse_attribution(body).unwrap();
        assert!(parsed.attributed);
        assert_eq!(parsed.keyword_id, None);
    }

    #[test]
    fn rejects_payloads_without_an_attribution_flag() {
        assert!(parse_attribution(r#"{"campaignId": 1}"#).is_err());
        assert!(parse_attribution("not json").is_err());
    }

    #[test]
    fn treats_404_as_retryable_and_400_as_terminal() {
        assert_eq!(
            classify_exchange(404, ""),
            ExchangeOutcome::Retryable("attribution record not available yet")
        );
        assert_eq!(
            classify_exchange(400, ""),
            ExchangeOutcome::Terminal("apple rejected the token")
        );
    }

    #[test]
    fn treats_apple_outages_as_retryable() {
        for status in [429u16, 500, 502, 503, 504] {
            assert!(matches!(classify_exchange(status, ""), ExchangeOutcome::Retryable(_)));
        }
    }

    #[test]
    fn classifies_a_successful_exchange() {
        match classify_exchange(200, ATTRIBUTED_BODY) {
            ExchangeOutcome::Resolved(a) => assert_eq!(a.keyword_id, Some(87675432)),
            other => panic!("expected resolved, got {:?}", other),
        }
        match classify_exchange(200, r#"{"attribution": false}"#) {
            ExchangeOutcome::Resolved(a) => assert!(!a.attributed),
            other => panic!("expected resolved, got {:?}", other),
        }
    }

    #[test]
    fn a_200_with_garbage_body_is_retryable_not_terminal() {
        assert!(matches!(classify_exchange(200, "<html>"), ExchangeOutcome::Retryable(_)));
    }

    #[test]
    fn fingerprint_is_stable_and_hides_the_token() {
        let token = "QVRUUklCVVRJT05UT0tFTg==";
        let first = token_fingerprint(token);
        assert_eq!(first, token_fingerprint(token));
        assert_eq!(first, token_fingerprint("  QVRUUklCVVRJT05UT0tFTg==  "));
        assert_eq!(first.len(), 64);
        assert!(!first.contains("QVRU"));
        assert_ne!(first, token_fingerprint("QVRUUklCVVRJT05UT0tFTh=="));
    }

    #[test]
    fn validates_token_shape() {
        assert!(validate_token_shape("QVRUUklCVVRJT05UT0tFTg==").is_ok());
        assert!(validate_token_shape("abc-_123abc-_123").is_ok());
        assert!(validate_token_shape("").is_err());
        assert!(validate_token_shape("short").is_err());
        assert!(validate_token_shape(&"A".repeat(MAX_TOKEN_LEN + 1)).is_err());
        assert!(validate_token_shape("has spaces in it here").is_err());
        assert!(validate_token_shape("{\"json\":\"nope\"}...").is_err());
    }

    #[test]
    fn converts_store_prices_to_cents() {
        assert_eq!(usd_to_cents(Some(4.99)), 499);
        assert_eq!(usd_to_cents(Some(49.99)), 4999);
        assert_eq!(usd_to_cents(Some(0.0)), 0);
        assert_eq!(usd_to_cents(None), 0);
        assert_eq!(usd_to_cents(Some(-1.0)), 0);
        assert_eq!(usd_to_cents(Some(f64::NAN)), 0);
    }

    #[test]
    fn nets_out_the_store_commission_only_when_known() {
        assert_eq!(net_cents(4999, Some(0.85)), Some(4249));
        assert_eq!(net_cents(499, Some(0.7)), Some(349));
        assert_eq!(net_cents(4999, None), None);
        assert_eq!(net_cents(4999, Some(0.0)), None);
        assert_eq!(net_cents(4999, Some(1.5)), None);
    }

    #[test]
    fn per_payer_guards_against_zero_payers() {
        assert_eq!(per_payer_cents(0, 0), None);
        assert_eq!(per_payer_cents(4999, 0), None);
        assert_eq!(per_payer_cents(4999, 1), Some(4999));
        assert_eq!(per_payer_cents(10000, 3), Some(3333));
    }

    #[test]
    fn kill_rule_arithmetic_uses_net_per_payer() {
        let net_per_payer = per_payer_cents(6000, 3).unwrap();
        let ceiling = net_per_payer * 60 / 100;
        assert_eq!(net_per_payer, 2000);
        assert_eq!(ceiling, 1200);
        assert!(1100 <= ceiling);
        assert!(1300 > ceiling);
    }

    #[test]
    fn normalizes_an_open_window() {
        let (since, until) = normalize_window(None, None).unwrap();
        assert_eq!(since, WINDOW_MIN);
        assert_eq!(until, WINDOW_MAX);
    }

    #[test]
    fn accepts_dates_and_timestamps() {
        let (since, until) = normalize_window(Some("2026-07-01"), Some("2026-08-01")).unwrap();
        assert_eq!(since, "2026-07-01");
        assert_eq!(until, "2026-08-01");
        assert!(normalize_window(Some("2026-07-01T12:00:00+00:00"), None).is_ok());
    }

    #[test]
    fn a_bare_date_bound_compares_correctly_against_stored_timestamps() {
        let (since, until) = normalize_window(Some("2026-07-01"), Some("2026-08-01")).unwrap();
        let inside = "2026-07-01T00:00:00.001+00:00".to_string();
        let last_moment = "2026-07-31T23:59:59.999+00:00".to_string();
        let outside = "2026-08-01T00:00:00.000+00:00".to_string();
        assert!(inside >= since && inside < until);
        assert!(last_moment >= since && last_moment < until);
        assert!(!(outside < until));
    }

    #[test]
    fn rejects_nonsense_and_inverted_windows() {
        assert!(normalize_window(Some("yesterday"), None).is_err());
        assert!(normalize_window(Some("2026-07-01; DROP TABLE users"), None).is_err());
        assert!(normalize_window(Some("2026-08-01"), Some("2026-07-01")).is_err());
        assert!(normalize_window(Some("2026-07-01"), Some("2026-07-01")).is_err());
    }

    #[test]
    fn purchase_kinds_have_stable_storage_labels() {
        assert_eq!(PurchaseKind::CreditPack.as_str(), "credit_pack");
        assert_eq!(PurchaseKind::Subscription.as_str(), "subscription");
        assert_eq!(PurchaseKind::Unknown.as_str(), "unknown");
    }

    fn rollup_row() -> serde_json::Value {
        serde_json::json!({
            "campaign_id": 542370539,
            "ad_group_id": 542317095,
            "keyword_id": 87675432,
            "country_or_region": "US",
            "installs": 40,
            "payers": 2,
            "purchases": 3,
            "revenue_usd_cents": 1497,
            "reported_net_usd_cents": 424,
            "purchases_missing_net": 2,
            "revenue_missing_net_usd_cents": 998,
            "first_install_at": "2026-07-01T00:00:00+00:00",
            "last_install_at": "2026-07-20T00:00:00+00:00"
        })
    }

    #[test]
    fn reads_a_keyword_rollup_row() {
        let parsed = parse_keyword_row(&rollup_row(), Some(0.85));
        assert_eq!(parsed.installs, 40);
        assert_eq!(parsed.payers, 2);
        assert_eq!(parsed.purchases, 3);
        assert_eq!(parsed.revenue_usd_cents, 1497);
        assert_eq!(parsed.reported_net_usd_cents, 424);
        assert_eq!(parsed.purchases_missing_net, 2);
        assert_eq!(parsed.net_revenue_usd_cents, Some(424 + 848));
        assert_eq!(parsed.net_per_payer_usd_cents, Some(636));
        assert_eq!(parsed.revenue_per_payer_usd_cents, Some(748));
    }

    #[test]
    fn an_unknown_store_commission_reports_unknown_net_never_zero() {
        let parsed = parse_keyword_row(&rollup_row(), None);
        assert_eq!(parsed.revenue_usd_cents, 1497);
        assert_eq!(parsed.net_revenue_usd_cents, None);
        assert_eq!(parsed.net_per_payer_usd_cents, None);
        assert_eq!(parsed.revenue_per_payer_usd_cents, Some(748));
    }

    #[test]
    fn fully_reported_net_needs_no_configured_share() {
        let mut row = rollup_row();
        row["purchases_missing_net"] = serde_json::json!(0);
        row["revenue_missing_net_usd_cents"] = serde_json::json!(0);
        let parsed = parse_keyword_row(&row, None);
        assert_eq!(parsed.net_revenue_usd_cents, Some(424));
        assert_eq!(parsed.net_per_payer_usd_cents, Some(212));
    }

    #[test]
    fn a_keyword_with_taps_but_no_payers_reports_no_net_per_payer() {
        let row = serde_json::json!({
            "campaign_id": 1,
            "keyword_id": 9,
            "installs": 30,
            "payers": 0,
            "purchases": 0,
            "revenue_usd_cents": 0,
            "reported_net_usd_cents": 0,
            "purchases_missing_net": 0,
            "revenue_missing_net_usd_cents": 0
        });
        let parsed = parse_keyword_row(&row, Some(0.85));
        assert_eq!(parsed.net_revenue_usd_cents, Some(0));
        assert_eq!(parsed.net_per_payer_usd_cents, None);
        assert_eq!(parsed.revenue_per_payer_usd_cents, None);
        assert_eq!(parsed.ad_group_id, None);
        assert_eq!(parsed.country_or_region, None);
    }

    #[test]
    fn effective_net_falls_back_to_the_configured_share() {
        assert_eq!(effective_net_cents(0, 0, None), Some(0));
        assert_eq!(effective_net_cents(424, 0, None), Some(424));
        assert_eq!(effective_net_cents(0, 1000, Some(0.85)), Some(850));
        assert_eq!(effective_net_cents(424, 998, Some(0.7)), Some(424 + 699));
        assert_eq!(effective_net_cents(424, 998, None), None);
        assert_eq!(effective_net_cents(424, 998, Some(0.0)), None);
        assert_eq!(effective_net_cents(424, 998, Some(1.4)), None);
    }

    #[test]
    fn reads_an_install_row() {
        let row = serde_json::json!({
            "app_id": "dreameater",
            "user_id": "user-1",
            "attributed": 1,
            "campaign_id": 542370539,
            "ad_group_id": 542317095,
            "keyword_id": 87675432,
            "country_or_region": "US",
            "created_at": "2026-07-28T10:00:00+00:00"
        });
        let parsed = parse_install_row(&row).unwrap();
        assert!(parsed.attributed);
        assert_eq!(parsed.keyword_id, Some(87675432));
        assert_eq!(parsed.app_id, "dreameater");
    }

    #[test]
    fn reads_an_organic_install_row() {
        let row = serde_json::json!({
            "app_id": "payday",
            "user_id": "user-2",
            "attributed": 0,
            "campaign_id": null,
            "ad_group_id": null,
            "keyword_id": null,
            "country_or_region": null,
            "created_at": "2026-07-28T10:00:00+00:00"
        });
        let parsed = parse_install_row(&row).unwrap();
        assert!(!parsed.attributed);
        assert_eq!(parsed.campaign_id, None);
    }
}
