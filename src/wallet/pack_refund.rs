use chrono::Utc;

use super::d1::{int, text, D1Exec};
use super::ops::Reversal;
use super::types::Kind;
use crate::error::AppError;
use crate::storekit::REFUNDED_MESSAGE;

/// The credit purchase of a store transaction that a refund concerns: `pending` while its credit is not finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditedPurchase {
    pub id: String,
    pub user_id: String,
    pub credits: u32,
    pub status: String,
}

impl CreditedPurchase {
    /// True while the purchase waits for its credit to finish.
    pub fn is_pending(&self) -> bool {
        self.status == "pending"
    }

    /// True once a reversal finished: the purchase reads `refunded`.
    pub fn is_refunded(&self) -> bool {
        self.status == "refunded"
    }
}

/// The purchase row the RevenueCat webhook, the client's validate call and the ledger's grant share for a store
/// transaction, when it was started (`pending`), credited (`completed`) or already reversed (`refunded`). A transaction
/// that no credit path ever saw has no such row, which is what keeps a refund of anything else from touching a balance.
pub async fn refundable_purchase(db: &impl D1Exec, app_id: &str, transaction_id: &str) -> Result<Option<CreditedPurchase>, AppError> {
    let rows = db
        .rows(
            "SELECT id, user_id, credits, status FROM credit_purchases
             WHERE app_id = ?1 AND payment_provider = 'revenuecat' AND payment_id = ?2 AND status IN ('pending', 'completed', 'refunded')
             ORDER BY created_at LIMIT 1",
            &[app_id.into(), transaction_id.into()],
        )
        .await?;
    Ok(rows.first().map(|row| CreditedPurchase {
        id: text(row, "id").unwrap_or_default().to_string(),
        user_id: text(row, "user_id").unwrap_or_default().to_string(),
        credits: int(row, "credits").unwrap_or(0).max(0) as u32,
        status: text(row, "status").unwrap_or_default().to_string(),
    }))
}

/// True when a refund of the store transaction was claimed, which makes every credit path refuse it for good.
pub async fn is_claimed(db: &impl D1Exec, app_id: &str, transaction_id: &str) -> Result<bool, AppError> {
    let rows = db
        .rows("SELECT 1 AS found FROM purchase_refunds WHERE app_id = ?1 AND transaction_id = ?2", &[app_id.into(), transaction_id.into()])
        .await?;
    Ok(!rows.is_empty())
}

/// The `refunded` error a validate call answers for a transaction whose refund was claimed, `None` when it was not.
pub async fn refunded_error(db: &impl D1Exec, app_id: &str, transaction_id: &str) -> Result<Option<AppError>, AppError> {
    Ok(is_claimed(db, app_id, transaction_id).await?.then(|| AppError::Refunded(REFUNDED_MESSAGE.to_string())))
}

/// Records the refund of a transaction no credit path has seen yet, so a purchase event or a grant that arrives later is
/// refused (`is_claimed`). The claim names no user and no credits, because nothing was credited.
pub async fn record_uncredited(db: &impl D1Exec, app_id: &str, transaction_id: &str) -> Result<(), AppError> {
    claim(db, app_id, transaction_id, "", 0).await?;
    Ok(())
}

/// Claims the refund of a store transaction in `purchase_refunds`, which is shared by every path that reverses a pack.
/// True for the call that wrote the claim, false when one already existed.
pub async fn claim(db: &impl D1Exec, app_id: &str, transaction_id: &str, user_id: &str, credits: u32) -> Result<bool, AppError> {
    let changed = db
        .exec(
            "INSERT OR IGNORE INTO purchase_refunds (app_id, transaction_id, user_id, credits, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            &[app_id.into(), transaction_id.into(), user_id.into(), (credits as i64).into(), Utc::now().to_rfc3339().into()],
        )
        .await?;
    Ok(changed > 0)
}

/// Releases a claim whose reversal failed, so the retry of whichever path sees the refund first can finish the job.
pub async fn release(db: &impl D1Exec, app_id: &str, transaction_id: &str) -> Result<(), AppError> {
    db.exec("DELETE FROM purchase_refunds WHERE app_id = ?1 AND transaction_id = ?2", &[app_id.into(), transaction_id.into()]).await?;
    Ok(())
}

/// Marks a purchase `refunded` once its credits were taken back.
pub async fn mark_refunded(db: &impl D1Exec, purchase_id: &str) -> Result<(), AppError> {
    db.exec("UPDATE credit_purchases SET status = 'refunded' WHERE id = ?1", &[purchase_id.into()]).await?;
    Ok(())
}

/// The wallet idempotency key of a transaction's reversal, the same whichever path takes the credits back.
pub fn reversal_key(transaction_id: &str) -> String {
    format!("reverse:{transaction_id}")
}

/// The ledger description a reversed pack is shown with.
pub fn description(pack_id: &str) -> String {
    format!("App Store refund reversed: {pack_id} pack")
}

/// The clamped debit that takes `credits` back for a refunded store transaction, tagged with where the refund came from.
pub fn reversal<'a>(transaction_id: &str, source: &'static str, credits: u32, description: &'a str, reference: &'a str) -> Reversal<'a> {
    Reversal { key: reversal_key(transaction_id), kind: Kind::Reversal, source, amount: credits, description, reference: Some(reference) }
}
