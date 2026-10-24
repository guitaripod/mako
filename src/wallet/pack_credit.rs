use super::d1::D1Exec;
use super::ops::Reversal;
use super::pack_refund::{self, CreditedPurchase, Reverser, Settlement, Step};
use super::purchase;
use crate::error::AppError;
use crate::ledgers::Account;
use crate::storekit::already_settled;

/// What the RevenueCat webhook and the validate calls do with a purchase row once it exists: finish it, find the
/// account its user has now, and take credits back from that account. `Ledgers` is the production implementation; the
/// host tests run the same code over SQLite and the wallet fakes.
#[allow(async_fn_in_trait)]
pub trait PurchaseLedger {
    /// Completes a purchase into whichever account its user has now.
    async fn complete(&self, purchase_id: &str) -> Result<(), AppError>;

    /// The account a stored user has right now.
    async fn account_for(&self, app_id: &str, user_id: &str) -> Result<Account, AppError>;

    /// Takes back at most `reversal.amount` from an account: what was taken is the second value.
    async fn reverse(&self, account: &Account, reversal: &Reversal<'_>) -> Result<(i32, u32), AppError>;
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

/// Credits the pack of a RevenueCat transaction, once per store transaction: the webhook, the validate calls and the
/// ledger's grant all land here or in `ledger_grant`. A claimed refund is refused and a row that is no longer `pending` is
/// a duplicate; both checks are only shortcuts, because the row is created unless the refund was claimed in the same
/// statement (`purchase::record_pending`, the linearization point) and completed through the ledger, whose wallet grant
/// is idempotent by the transaction key and whose flip never overwrites `refunded`.
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
) -> Result<Credit, AppError> {
    if pack_refund::is_claimed(db, app_id, transaction_id).await? {
        return Ok(Credit::Refunded);
    }
    let status = purchase::revenuecat_status(db, app_id, transaction_id).await?;
    if already_settled(status.as_deref()) {
        return Ok(Credit::Duplicate);
    }
    let purchase_id = match purchase::record_pending(db, app_id, user_id, pack_id, credits, amount_usd_cents, transaction_id).await {
        Ok(id) => id,
        Err(AppError::Refunded(_)) => return Ok(Credit::Refunded),
        Err(error) => return Err(error),
    };
    match ledger.complete(&purchase_id).await {
        Ok(()) => Ok(Credit::Credited { purchase_id }),
        Err(AppError::Refunded(_)) => Ok(Credit::Refunded),
        Err(error) => Err(error),
    }
}

/// The RevenueCat refund's half of a reversal: a purchase still `pending` is finished, and the credits are taken back
/// from the account the purchase's user has now. A local balance has no idempotency key, so it is reversed only by the
/// call that wrote the claim.
struct RevenueCatReverser<'a, L: PurchaseLedger> {
    ledger: &'a L,
    app_id: &'a str,
    pack_id: &'a str,
    transaction_id: &'a str,
}

impl<L: PurchaseLedger> Reverser for RevenueCatReverser<'_, L> {
    async fn reverse(&self, purchase: &CreditedPurchase, first_claim: bool) -> Result<Step, AppError> {
        if purchase.is_pending() {
            match self.ledger.complete(&purchase.id).await {
                Ok(()) => {}
                Err(AppError::Refunded(_)) => return Ok(Step::Settled),
                Err(error) => return Err(error),
            }
        }
        let account = self.ledger.account_for(self.app_id, &purchase.user_id).await?;
        if matches!(account, Account::Local { .. }) && !first_claim {
            return Ok(Step::Settled);
        }
        let description = pack_refund::description(self.pack_id);
        let reference = crate::purchases::refund_reference(self.transaction_id);
        let reversal = pack_refund::reversal(self.transaction_id, "revenuecat", purchase.credits, &description, &reference);
        let (_, taken) = self.ledger.reverse(&account, &reversal).await?;
        Ok(Step::Taken(taken))
    }
}

/// Takes back the credits of a refunded RevenueCat pack purchase, once per transaction: the claim, the purchase read, the
/// finishing of a `pending` purchase, the reversal and the `refunded` flip are `pack_refund::claim_and_settle`, shared
/// with the ledger's refund route.
pub async fn refund_pack<D: D1Exec, L: PurchaseLedger>(
    db: &D,
    ledger: &L,
    app_id: &str,
    pack_id: &str,
    transaction_id: &str,
) -> Result<Settlement, AppError> {
    let reverser = RevenueCatReverser { ledger, app_id, pack_id, transaction_id };
    pack_refund::claim_and_settle(db, app_id, transaction_id, &reverser).await
}
