use chrono::{Duration, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::client::WalletTransport;
use super::d1::{int, text, D1Exec, Stmt};
use super::ops::{Grant, Reversal, WalletOps};
use super::pack_refund;
use super::sql::Val;
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

/// The condition every statement of a local batch carries: the purchase is in the status the batch moves it out of. The
/// batch is one D1 transaction, and its last statement is the flip that changes this status, so every earlier statement
/// sees the status the batch started with and the whole batch takes effect for exactly one of any number of concurrent
/// callers.
const PURCHASE_IS: &str = "EXISTS (SELECT 1 FROM credit_purchases WHERE id = ?1 AND status = ?2)";

/// Credits a purchase to the D1 balance of its user and marks it `completed` as one batch: the balance increment, the
/// ledger row and the flip all run in one D1 transaction, and the increment and the ledger row are conditional on the
/// purchase still being `pending` in that transaction, so a crash can neither lose the credit after the flip nor credit
/// without the flip, and two callers credit once. The status is not read through `changes()` or a marker: the flip is
/// the last statement and the others test the same `pending` it tests, which holds for every statement or none because
/// D1 runs a batch without another statement in between. Returns true when this call flipped the status, false when it
/// was already completed, and `AppError::Refunded` for a purchase a refund already reversed.
pub async fn complete_locally(db: &impl D1Exec, purchase_id: &str) -> Result<bool, AppError> {
    let row = read(db, purchase_id).await?.ok_or_else(|| AppError::NotFound("Purchase not found".to_string()))?;
    match row.status.as_str() {
        "completed" => return Ok(false),
        "pending" => {}
        "refunded" => return Err(AppError::Refunded(REFUNDED_MESSAGE.to_string())),
        _ => return Err(AppError::NotFound("Purchase not found".to_string())),
    }
    let (_, description) = source_and_description(&row);
    let now = Utc::now().to_rfc3339();
    let credits = row.credits.max(0);
    let batch = [
        Stmt::new(
            &format!(
                "UPDATE user_credits SET balance = balance + ?3, lifetime_purchased = lifetime_purchased + ?3, updated_at = ?4
                 WHERE app_id = ?5 AND user_id = ?6 AND {PURCHASE_IS}"
            ),
            vec![row.id.as_str().into(), "pending".into(), credits.into(), now.as_str().into(), row.app_id.as_str().into(), row.user_id.as_str().into()],
        ),
        Stmt::new(
            &format!(
                "INSERT INTO credit_transactions (id, app_id, user_id, type, amount, balance_after, description, reference_id, created_at)
                 SELECT ?3, app_id, user_id, 'purchase', ?4, balance, ?5, ?1, ?6 FROM user_credits
                 WHERE app_id = ?7 AND user_id = ?8 AND {PURCHASE_IS}"
            ),
            vec![
                row.id.as_str().into(),
                "pending".into(),
                Uuid::new_v4().to_string().into(),
                credits.into(),
                description.as_str().into(),
                now.as_str().into(),
                row.app_id.as_str().into(),
                row.user_id.as_str().into(),
            ],
        ),
        Stmt::new(
            "UPDATE credit_purchases SET status = 'completed', completed_at = ?3
             WHERE id = ?1 AND status = ?2 AND EXISTS (SELECT 1 FROM user_credits WHERE app_id = ?4 AND user_id = ?5)",
            vec![row.id.as_str().into(), "pending".into(), now.as_str().into(), row.app_id.as_str().into(), row.user_id.as_str().into()],
        ),
    ];
    let changes = db.batch_changes(&batch).await?;
    if changes.last().copied().unwrap_or(0) > 0 {
        return Ok(true);
    }
    match read(db, purchase_id).await?.map(|r| r.status).as_deref() {
        Some("completed") => Ok(false),
        Some("refunded") => Err(AppError::Refunded(REFUNDED_MESSAGE.to_string())),
        Some("pending") => Err(AppError::InternalError(format!("no user_credits row for {}/{}", row.app_id, row.user_id))),
        _ => Err(AppError::NotFound("Purchase not found".to_string())),
    }
}

/// Takes back at most `amount` credits of a `completed` purchase from the D1 balance of its user and marks the purchase
/// `refunded`, as one batch: the clamped deduction, its ledger row and the flip run in one D1 transaction, and the
/// deduction and the ledger row are conditional on the purchase being `completed` in that transaction. The clamp is
/// `MIN(balance, amount)` inside the statement, so no read-then-write loop is needed and a concurrent spend cannot
/// slip in between. Returns what was taken, or `None` when the purchase was not `completed` (still `pending`, or
/// already `refunded`) and nothing changed.
pub async fn reverse_locally(
    db: &impl D1Exec,
    app_id: &str,
    user_id: &str,
    purchase_id: &str,
    reversal: &Reversal<'_>,
) -> Result<Option<u32>, AppError> {
    let now = Utc::now().to_rfc3339();
    let amount = reversal.amount as i64;
    let batch = [
        Stmt::new(
            &format!(
                "INSERT INTO credit_transactions (id, app_id, user_id, type, amount, balance_after, description, reference_id, created_at)
                 SELECT ?3, app_id, user_id, 'admin_adjustment', -MIN(balance, ?4), balance - MIN(balance, ?4), ?5, ?6, ?7 FROM user_credits
                 WHERE app_id = ?8 AND user_id = ?9 AND balance > 0 AND ?4 > 0 AND {PURCHASE_IS}"
            ),
            vec![
                purchase_id.into(),
                "completed".into(),
                Uuid::new_v4().to_string().into(),
                amount.into(),
                reversal.description.into(),
                reversal.reference.map(Val::from).unwrap_or(Val::Null),
                now.as_str().into(),
                app_id.into(),
                user_id.into(),
            ],
        ),
        Stmt::new(
            &format!(
                "UPDATE user_credits SET balance = balance - MIN(balance, ?3), lifetime_spent = lifetime_spent + MIN(balance, ?3), updated_at = ?4
                 WHERE app_id = ?5 AND user_id = ?6 AND balance > 0 AND ?3 > 0 AND {PURCHASE_IS}"
            ),
            vec![purchase_id.into(), "completed".into(), amount.into(), now.as_str().into(), app_id.into(), user_id.into()],
        ),
        Stmt::new(
            "UPDATE credit_purchases SET status = 'refunded' WHERE id = ?1 AND status = ?2",
            vec![purchase_id.into(), "completed".into()],
        ),
    ];
    let changes = db.batch_changes(&batch).await?;
    if changes.last().copied().unwrap_or(0) == 0 {
        return Ok(None);
    }
    let taken = match reversal.reference {
        Some(reference) => db
            .rows(
                "SELECT -amount AS taken FROM credit_transactions
                 WHERE app_id = ?1 AND user_id = ?2 AND reference_id = ?3 AND type = 'admin_adjustment' ORDER BY created_at DESC LIMIT 1",
                &[app_id.into(), user_id.into(), reference.into()],
            )
            .await?
            .first()
            .and_then(|r| int(r, "taken"))
            .unwrap_or(0),
        None => 0,
    };
    Ok(Some(taken.max(0) as u32))
}

/// Records a RevenueCat purchase and completes it into the D1 balance, the local-balance counterpart of the wallet
/// path: `record_pending` (which refuses a refunded transaction) and `complete_locally`. Returns the purchase id and
/// whether this call completed it.
pub async fn record_and_complete_locally(
    db: &impl D1Exec,
    app_id: &str,
    user_id: &str,
    pack_id: &str,
    credits: u32,
    amount_usd_cents: u32,
    payment_id: &str,
) -> Result<(String, bool), AppError> {
    let id = record_pending(db, app_id, user_id, pack_id, credits, amount_usd_cents, payment_id).await?;
    let completed = complete_locally(db, &id).await?;
    Ok((id, completed))
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
