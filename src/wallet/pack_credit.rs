use super::d1::D1Exec;
use super::{pack_refund, purchase};
use crate::error::AppError;
use crate::storekit::already_settled;

/// What the validate call does with a purchase row once it exists: finish it into whichever account its user has now.
/// `Ledgers` is the production implementation; the host tests run the same code over SQLite and the wallet fakes.
#[allow(async_fn_in_trait)]
pub trait PurchaseLedger {
    /// Completes a purchase into whichever account its user has now.
    async fn complete(&self, purchase_id: &str) -> Result<(), AppError>;
}
/// What a credit of a pack purchase came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credit {
    /// This call created or found the purchase row and completed it.
    Credited { purchase_id: String },
    /// The transaction was settled before, in any final status: nothing is credited again.
    Duplicate,
    /// The refund of the transaction was claimed, before the row or while it was being completed: nothing stays credited
    /// that a refund will not reverse.
    Refunded,
}

/// Credits the pack of a store transaction, once per transaction: the validate call lands here and the ledger's grant in
/// `ledger_grant`. A claimed refund is refused and a row that is no longer `pending` is a duplicate; both checks are only
/// shortcuts, because the row is created unless the refund was claimed in the same statement (`purchase::record_pending`,
/// the linearization point) and completed through the ledger, whose wallet grant is idempotent by the transaction key and
/// whose flip never overwrites `refunded`. A `store_token` (a Google Play purchase token that still needs mako's
/// acknowledgement) is kept on the row before the credit is completed, so a row the cron sweep finishes later can still
/// be acknowledged; failing to keep it is only logged.
#[allow(clippy::too_many_arguments)]
pub async fn credit_pack<D: D1Exec, L: PurchaseLedger>(
    db: &D,
    ledger: &L,
    app_id: &str,
    user_id: &str,
    pack_id: &str,
    credits: u32,
    amount_usd_cents: u32,
    transaction_id: &str,
    store_token: Option<&str>,
) -> Result<Credit, AppError> {
    if pack_refund::is_claimed(db, app_id, transaction_id).await? {
        return Ok(Credit::Refunded);
    }
    let status = purchase::store_status(db, app_id, transaction_id).await?;
    if already_settled(status.as_deref()) {
        return Ok(Credit::Duplicate);
    }
    let purchase_id = match purchase::record_pending(db, app_id, user_id, pack_id, credits, amount_usd_cents, transaction_id).await {
        Ok(id) => id,
        Err(AppError::Refunded(_)) => return Ok(Credit::Refunded),
        Err(error) => return Err(error),
    };
    if let Some(token) = store_token {
        if let Err(error) = purchase::remember_store_token(db, app_id, transaction_id, token).await {
            super::log::error(&format!("the store token of {transaction_id} could not be kept: {error:?}"));
        }
    }
    match ledger.complete(&purchase_id).await {
        Ok(()) => Ok(Credit::Credited { purchase_id }),
        Err(AppError::Refunded(_)) => Ok(Credit::Refunded),
        Err(error) => Err(error),
    }
}
