use chrono::{Duration, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::client::WalletTransport;
use super::d1::{int, text, D1Exec};
use super::ops::{Grant, WalletOps};
use super::pack_refund;
use super::types::Kind;
use crate::error::AppError;
use crate::ledgers::Account;
use crate::storekit::REFUNDED_MESSAGE;

/// A stored RevenueCat purchase waits this long as `pending` before the cron finishes it.
const PENDING_AFTER_MINUTES: i64 = 5;

/// Pending RevenueCat purchases one cron run completes.
pub const SWEEP_LIMIT: i64 = 50;

/// One `credit_purchases` row, as the completion needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurchaseRow {
    pub id: String,
    pub app_id: String,
    pub user_id: String,
    pub pack_id: String,
    pub credits: i64,
    pub amount_usd_cents: i64,
    pub provider: String,
    pub payment_id: String,
    pub status: String,
}

impl PurchaseRow {
    fn parse(row: &Value) -> Option<Self> {
        Some(Self {
            id: text(row, "id")?.to_string(),
            app_id: text(row, "app_id")?.to_string(),
            user_id: text(row, "user_id")?.to_string(),
            pack_id: text(row, "pack_id")?.to_string(),
            credits: int(row, "credits")?,
            amount_usd_cents: int(row, "amount_usd_cents").unwrap_or(0),
            provider: text(row, "payment_provider")?.to_string(),
            payment_id: text(row, "payment_id").unwrap_or_default().to_string(),
            status: text(row, "status")?.to_string(),
        })
    }
}

const COLUMNS: &str = "id, app_id, user_id, pack_id, credits, amount_usd_cents, payment_provider, payment_id, status";

/// The idempotency key a purchase grants under: the store transaction id for RevenueCat, which the purchase ledger
/// and the fast-track share, and the purchase id for Stripe and crypto, whose payment ids are not minted by us.
pub fn grant_key(row: &PurchaseRow) -> String {
    match row.provider.as_str() {
        "revenuecat" => format!("tx:{}", row.payment_id),
        "stripe" => format!("stripe:{}", row.id),
        "nowpayments" => format!("crypto:{}", row.id),
        _ => format!("purchase:{}", row.id),
    }
}

/// The ledger source and the description a purchase is shown with.
fn source_and_description(row: &PurchaseRow) -> (&'static str, String) {
    match row.provider.as_str() {
        "revenuecat" => ("revenuecat", format!("App Store purchase: {} pack", row.pack_id)),
        "stripe" => ("stripe", format!("Card purchase: {} pack", row.pack_id)),
        "nowpayments" => ("crypto", format!("Crypto purchase: {} pack", row.pack_id)),
        _ => ("stripe", format!("Purchased {} pack", row.pack_id)),
    }
}

/// A purchase by id, in any status.
pub async fn read(db: &impl D1Exec, purchase_id: &str) -> Result<Option<PurchaseRow>, AppError> {
    let sql = format!("SELECT {COLUMNS} FROM credit_purchases WHERE id = ?1");
    Ok(db.rows(&sql, &[purchase_id.into()]).await?.first().and_then(PurchaseRow::parse))
}

/// The status of the RevenueCat purchase of a store transaction, if one was recorded.
pub async fn revenuecat_status(db: &impl D1Exec, app_id: &str, payment_id: &str) -> Result<Option<String>, AppError> {
    let sql = "SELECT status FROM credit_purchases WHERE app_id = ?1 AND payment_id = ?2 AND payment_provider = 'revenuecat'
        ORDER BY created_at LIMIT 1";
    Ok(db.rows(sql, &[app_id.into(), payment_id.into()]).await?.first().and_then(|r| text(r, "status")).map(str::to_string))
}

/// Records a RevenueCat purchase as `pending` unless the transaction already has a row or its refund was claimed, and
/// returns the id of the row there is. This is the one place a pack purchase row is born, and the statement is the
/// linearization point against a refund: D1 applies one statement at a time, and the insert only happens while no
/// `purchase_refunds` claim exists, so a refund that claims first leaves no row for a credit to create
/// (`AppError::Refunded`), and a refund that claims after finds the row this statement made. The webhook, the
/// fast-track and the ledger's grant race on one transaction; at most one row ever exists for it.
pub async fn record_pending(
    db: &impl D1Exec,
    app_id: &str,
    user_id: &str,
    pack_id: &str,
    credits: u32,
    amount_usd_cents: u32,
    payment_id: &str,
) -> Result<String, AppError> {
    let now = Utc::now().to_rfc3339();
    db.exec(
        "INSERT INTO credit_purchases (id, app_id, user_id, pack_id, credits, amount_usd_cents, payment_provider, payment_id, status, created_at)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, 'revenuecat', ?7, 'pending', ?8
         WHERE NOT EXISTS (SELECT 1 FROM credit_purchases WHERE app_id = ?2 AND payment_provider = 'revenuecat' AND payment_id = ?7)
           AND NOT EXISTS (SELECT 1 FROM purchase_refunds WHERE app_id = ?2 AND transaction_id = ?7)",
        &[
            Uuid::new_v4().to_string().into(),
            app_id.into(),
            user_id.into(),
            pack_id.into(),
            (credits as i64).into(),
            (amount_usd_cents as i64).into(),
            payment_id.into(),
            now.into(),
        ],
    )
    .await?;
    let rows = db
        .rows(
            "SELECT id FROM credit_purchases WHERE app_id = ?1 AND payment_provider = 'revenuecat' AND payment_id = ?2 ORDER BY created_at LIMIT 1",
            &[app_id.into(), payment_id.into()],
        )
        .await?;
    if let Some(id) = rows.first().and_then(|r| text(r, "id")) {
        return Ok(id.to_string());
    }
    match pack_refund::refunded_error(db, app_id, payment_id).await? {
        Some(refunded) => Err(refunded),
        None => Err(AppError::InternalError("purchase row vanished".to_string())),
    }
}

/// Credits a purchase to a wallet account, once per its key. The caller decides whether the purchase may still be
/// credited; this only performs the idempotent grant.
pub async fn grant_purchase<W: WalletTransport>(
    db: &impl D1Exec,
    wallets: &W,
    account: &Account,
    purchase: &PurchaseRow,
) -> Result<(), AppError> {
    let Account::Wallet { midgar_id, app_id, .. } = account else {
        return Err(AppError::InternalError("grant_purchase needs a wallet account".to_string()));
    };
    let (source, description) = source_and_description(purchase);
    let grant = Grant {
        key: grant_key(purchase),
        kind: Kind::Purchase,
        source,
        amount: purchase.credits.max(0) as u32,
        description: &description,
        reference: Some(&purchase.id),
    };
    WalletOps { wallets, db }.grant(midgar_id, app_id, &grant).await?;
    Ok(())
}

/// Credits a purchase to a wallet account and then marks it `completed`. The order is the point: the grant is
/// idempotent by its key, so a crash between the two leaves `pending`, which a retry or the cron finishes without a
/// lost or a doubled credit. The status is read again right before the grant, so a purchase that a concurrent caller
/// completed through the local balance while the account was being resolved is not credited a second time. Returns
/// true when this call flipped the status, false when it was already completed. A purchase a refund already reversed
/// is `AppError::Refunded`: the flip only ever leaves `pending`, so it can never overwrite `refunded`, and the grant
/// stays idempotent by its key, so a refund that finished this credit and a caller that grants afterwards never
/// double-grant.
pub async fn complete_in_wallet<W: WalletTransport>(
    db: &impl D1Exec,
    wallets: &W,
    account: &Account,
    purchase: &PurchaseRow,
) -> Result<bool, AppError> {
    let current = read(db, &purchase.id).await?.unwrap_or_else(|| purchase.clone());
    match current.status.as_str() {
        "completed" => return Ok(false),
        "pending" => {}
        "refunded" => return Err(AppError::Refunded(REFUNDED_MESSAGE.to_string())),
        _ => return Err(AppError::NotFound("Purchase not found".to_string())),
    }
    grant_purchase(db, wallets, account, &current).await?;
    let flipped = db
        .exec(
            "UPDATE credit_purchases SET status = 'completed', completed_at = ?2 WHERE id = ?1 AND status = 'pending'",
            &[purchase.id.as_str().into(), Utc::now().to_rfc3339().into()],
        )
        .await?;
    Ok(flipped > 0)
}

/// Puts a purchase back to `pending` after this call claimed it and then could not credit it, so a retry or the cron
/// finishes the credit instead of the purchase reading as paid out. Only a `completed` row is touched.
pub async fn release_claim(db: &impl D1Exec, purchase_id: &str) -> Result<(), AppError> {
    db.exec("UPDATE credit_purchases SET status = 'pending', completed_at = NULL WHERE id = ?1 AND status = 'completed'", &[purchase_id.into()])
        .await?;
    Ok(())
}

/// Ids of RevenueCat purchases that have been `pending` for more than five minutes: the request that recorded them
/// is gone and nobody finished the grant. Stripe and crypto rows wait for a payment and are never listed.
pub async fn stale_revenuecat(db: &impl D1Exec, limit: i64) -> Result<Vec<String>, AppError> {
    let cutoff = (Utc::now() - Duration::minutes(PENDING_AFTER_MINUTES)).to_rfc3339();
    let rows = db
        .rows(
            "SELECT id FROM credit_purchases WHERE status = 'pending' AND payment_provider = 'revenuecat' AND created_at < ?1
             ORDER BY created_at LIMIT ?2",
            &[cutoff.into(), limit.into()],
        )
        .await?;
    Ok(rows.iter().filter_map(|r| text(r, "id")).map(str::to_string).collect())
}
