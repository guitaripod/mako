use super::client::WalletTransport;
use super::d1::D1Exec;
use super::link::account_of;
use super::d1::text;
use super::ops::{Reversal, WalletOps};
use super::types::Kind;
use super::pack_refund::{CreditedPurchase, Reverser, Settlement, Step};
use super::{links, log, pack_refund, purchase};
use crate::credits::packs_for_app;
use crate::error::AppError;
use crate::ledgers::Account;
use crate::storekit::{made_in_tenant, pack_of_product, tenant_facts, tenant_of_product, LedgerTransaction, CONSUMABLE};

/// Why the ledger's refunded transaction takes nothing back. Every reason is final: the ledger is told so and does not
/// retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    UnknownProduct,
    UnknownTenant,
    UnknownPack,
    WrongApp,
    NotConsumable,
    LinkingOff,
}

impl Reason {
    /// The stable spelling the answer carries.
    pub fn as_str(&self) -> &'static str {
        match self {
            Reason::UnknownProduct => "unknown_product",
            Reason::UnknownTenant => "unknown_tenant",
            Reason::UnknownPack => "unknown_pack",
            Reason::WrongApp => "wrong_app",
            Reason::NotConsumable => "not_consumable",
            Reason::LinkingOff => "wallet_linking_off",
        }
    }
}

/// What a forwarded refund came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The credits were taken back, down to a zero balance: `taken` is at most the pack's `credits`.
    Reversed { purchase_id: String, credits: u32, taken: u32 },
    /// The transaction was reversed before, by an earlier delivery of the same refund.
    Duplicate { purchase_id: String },
    /// No credit path has seen the transaction yet: the refund is recorded and every later credit of it is refused.
    Recorded,
    Refused(Reason),
}

/// Takes back the credits of the credit pack a refunded StoreKit 2 transaction describes, once per store transaction,
/// down to a zero balance. The transaction must be a consumable made in the app of a tenant that sells its product as a
/// pack, so a subscription or any other product is never touched. A transaction nothing has credited yet is only
/// recorded (`Outcome::Recorded`): the claim in `purchase_refunds` makes the grant and the validate call refuse it
/// afterwards. The claim is written before the purchase row is read (`pack_refund::claim_and_settle`), so a credit
/// cannot slip in between: the statement that creates a purchase row never runs once a claim exists. A purchase still
/// `pending` is finished first and then reversed, so a credit a crash left half done cannot outlive its refund. A
/// purchase whose user is linked is reversed in the wallet under `reverse:<transactionId>`; any other in the user's D1
/// balance, together with the flip to `refunded` (`purchase::reverse_locally`). A claim whose reversal failed is
/// released, and a claim left by a crash is finished by the retry, because the wallet replays a key it has seen and the
/// local batch only applies to a `completed` purchase.
pub async fn reverse<D: D1Exec, W: WalletTransport>(db: &D, wallets: &W, transaction: &LedgerTransaction) -> Result<Outcome, AppError> {
    let Some((app_id, _prefix)) = tenant_of_product(db, &transaction.product_id).await? else {
        return Ok(Outcome::Refused(Reason::UnknownProduct));
    };
    let Some(facts) = tenant_facts(db, &app_id).await? else { return Ok(Outcome::Refused(Reason::UnknownTenant)) };
    if !made_in_tenant(transaction, &facts.slug, facts.bundle.as_deref()) {
        return Ok(Outcome::Refused(Reason::WrongApp));
    }
    if transaction.kind != CONSUMABLE {
        return Ok(Outcome::Refused(Reason::NotConsumable));
    }
    let Some(pack_id) = pack_of_product(db, &app_id, &transaction.product_id).await? else {
        return Ok(Outcome::Refused(Reason::UnknownProduct));
    };
    if !packs_for_app(db, &app_id).await.iter().any(|p| p.id == pack_id) {
        return Ok(Outcome::Refused(Reason::UnknownPack));
    }
    let reverser = LedgerReverser { db, wallets: Some(wallets), app_id: &app_id, transaction_id: &transaction.transaction_id };
    settle(db, &app_id, &transaction.transaction_id, &reverser).await
}

/// Takes back the credits of the pack a voided Google Play order bought (`payment_id` is `gp:<orderId>`), once, down to
/// a zero balance, exactly as `reverse` does for an App Store refund: the claim in `purchase_refunds` comes first, a
/// purchase still `pending` is finished and then reversed, and an order nothing has credited yet is only recorded, so
/// its validate call is refused afterwards. With `wallets` (wallet linking on) a linked buyer is reversed in the wallet
/// under `reverse:gp:<orderId>`; without, every buyer is a local D1 balance, as every credit was.
pub async fn reverse_play_order<D: D1Exec, W: WalletTransport>(db: &D, wallets: Option<&W>, app_id: &str, payment_id: &str) -> Result<Outcome, AppError> {
    let reverser = LedgerReverser { db, wallets, app_id, transaction_id: payment_id };
    settle(db, app_id, payment_id, &reverser).await
}

/// Where a refund of a store transaction came from and how its reversal is described: a Google Play order (`gp:`) or
/// an App Store transaction forwarded by the ledger. Derived from the id alone, so a retry rebuilds the same entry.
fn store_of(transaction_id: &str) -> (&'static str, fn(&str) -> String) {
    if transaction_id.starts_with(purchase::PLAY_PAYMENT_PREFIX) {
        (purchase::PLAY_STORE, pack_refund::play_description)
    } else {
        ("ledger", pack_refund::description)
    }
}

/// The reversal of the part of a refund the D1 balance could not cover, under `pack_refund::late_key`.
async fn take_late<D: D1Exec, W: WalletTransport>(
    db: &D,
    wallets: &W,
    midgar_id: &str,
    app_id: &str,
    transaction_id: &str,
    pack_id: &str,
    owed: u32,
) -> Result<u32, AppError> {
    let (source, describe) = store_of(transaction_id);
    let description = describe(pack_id);
    let reference = crate::purchases::refund_reference(transaction_id);
    let late = Reversal {
        key: pack_refund::late_key(transaction_id),
        kind: Kind::Reversal,
        source,
        amount: owed,
        description: &description,
        reference: Some(&reference),
    };
    let (_, taken) = (WalletOps { wallets, db }).reverse(midgar_id, app_id, &late).await?;
    Ok(taken)
}

/// Wallet cron: takes the late part of every refund that still owes one, under its `late_key`, so a retry of a call
/// whose answer was lost takes nothing twice. A buyer whose link is gone keeps the debt for a later run; it is logged.
/// Returns how many were settled.
pub async fn retry_late_reversals<D: D1Exec, W: WalletTransport>(db: &D, wallets: &W, limit: i64) -> Result<u32, AppError> {
    let mut settled = 0;
    for owed in pack_refund::late_reversals(db, limit).await? {
        let Some(link) = links::read(db, &owed.app_id, &owed.user_id).await? else {
            log::error(&format!("refund {} owes {} credits but its buyer has no wallet link", owed.transaction_id, owed.owed));
            continue;
        };
        let pack_id = purchase_pack(db, &owed.app_id, &owed.transaction_id).await?;
        match take_late(db, wallets, &link.midgar_id, &owed.app_id, &owed.transaction_id, &pack_id, owed.owed).await {
            Ok(_) => {
                pack_refund::settle_late(db, &owed.app_id, &owed.transaction_id).await?;
                settled += 1;
            }
            Err(error) => log::error(&format!("the late part of refund {} is still owed: {error:?}", owed.transaction_id)),
        }
    }
    Ok(settled)
}

/// The pack of the store purchase a refund concerns, empty when the row is gone.
async fn purchase_pack(db: &impl D1Exec, app_id: &str, transaction_id: &str) -> Result<String, AppError> {
    let rows = db
        .rows(
            &format!("SELECT pack_id FROM credit_purchases WHERE app_id = ?1 AND payment_id = ?2 AND {} LIMIT 1", purchase::IS_STORE_PACK),
            &[app_id.into(), transaction_id.into()],
        )
        .await?;
    Ok(rows.first().and_then(|row| text(row, "pack_id")).unwrap_or_default().to_string())
}

async fn settle<D: D1Exec, W: WalletTransport>(db: &D, app_id: &str, transaction_id: &str, reverser: &LedgerReverser<'_, D, W>) -> Result<Outcome, AppError> {
    Ok(match pack_refund::claim_and_settle(db, app_id, transaction_id, reverser).await? {
        Settlement::Tombstone => Outcome::Recorded,
        Settlement::Settled(purchase) => Outcome::Duplicate { purchase_id: purchase.id },
        Settlement::Reversed { purchase, taken } => Outcome::Reversed { purchase_id: purchase.id, credits: purchase.credits, taken },
    })
}

/// A store refund's half of the reversal: a purchase still `pending` is finished in the account its user has now, and
/// the pack is reversed there, under `reverse:<transactionId>` in a wallet or in one batch with the flip in a D1
/// balance. Without wallets every account is local.
struct LedgerReverser<'a, D: D1Exec, W: WalletTransport> {
    db: &'a D,
    wallets: Option<&'a W>,
    app_id: &'a str,
    transaction_id: &'a str,
}

impl<D: D1Exec, W: WalletTransport> Reverser for LedgerReverser<'_, D, W> {
    async fn reverse(&self, purchase: &CreditedPurchase) -> Result<Step, AppError> {
        let (source, describe) = store_of(self.transaction_id);
        let description = describe(&purchase.pack_id);
        let reference = crate::purchases::refund_reference(self.transaction_id);
        let reversal = pack_refund::reversal(self.transaction_id, source, purchase.credits, &description, &reference);
        let link = match self.wallets {
            Some(_) => links::read(self.db, self.app_id, &purchase.user_id).await?,
            None => None,
        };
        match link {
            Some(link) => self.reverse_in_wallet(&account_of(&link), purchase, &reversal).await,
            None => self.reverse_locally(purchase, &reversal).await,
        }
    }
}

impl<D: D1Exec, W: WalletTransport> LedgerReverser<'_, D, W> {
    /// Finishes a `pending` purchase in the user's wallet and takes the pack back from it.
    async fn reverse_in_wallet(&self, account: &Account, purchase: &CreditedPurchase, reversal: &Reversal<'_>) -> Result<Step, AppError> {
        let Account::Wallet { midgar_id, .. } = account else {
            return Err(AppError::InternalError("a link is not a wallet account".to_string()));
        };
        let wallets = self.wallets.ok_or(AppError::WalletUnavailable)?;
        if purchase.is_pending() {
            let row = purchase::read(self.db, &purchase.id).await?.ok_or_else(|| AppError::InternalError("purchase row vanished".to_string()))?;
            match purchase::complete_in_wallet(self.db, wallets, account, &row).await {
                Ok(_) => {}
                Err(AppError::Refunded(_)) => return Ok(Step::Settled),
                Err(error) => return Err(error),
            }
        }
        let (_, taken) = (WalletOps { wallets, db: self.db }).reverse(midgar_id, self.app_id, reversal).await?;
        Ok(Step::Taken(taken))
    }

    /// Finishes a `pending` purchase into the user's D1 balance and takes the pack back from it in the batch that flips
    /// the purchase to `refunded`. Whatever the balance could not cover is taken from the wallet when the user linked in
    /// the meantime (under `reverse:<transactionId>:late`), since the link moved the balance there. That part is
    /// recorded as owed (`purchase_refunds.late_owed`) before it is tried: the purchase already reads `refunded`, so a
    /// retry of the refund would settle nothing, and the wallet cron takes the debt instead (`retry_late_reversals`).
    async fn reverse_locally(&self, purchase: &CreditedPurchase, reversal: &Reversal<'_>) -> Result<Step, AppError> {
        if purchase.is_pending() {
            match purchase::complete_locally(self.db, &purchase.id).await {
                Ok(_) => {}
                Err(AppError::Refunded(_)) => return Ok(Step::Settled),
                Err(error) => return Err(error),
            }
        }
        let Some(taken) = purchase::reverse_locally(self.db, self.app_id, &purchase.user_id, &purchase.id, reversal).await? else {
            return Ok(Step::Settled);
        };
        if taken >= reversal.amount {
            return Ok(Step::Taken(taken));
        }
        let Some(wallets) = self.wallets else { return Ok(Step::Taken(taken)) };
        let Some(link) = links::read(self.db, self.app_id, &purchase.user_id).await? else { return Ok(Step::Taken(taken)) };
        let Account::Wallet { midgar_id, .. } = account_of(&link) else { return Ok(Step::Taken(taken)) };
        let owed = reversal.amount - taken;
        pack_refund::owe_late(self.db, self.app_id, self.transaction_id, owed).await?;
        match take_late(self.db, wallets, &midgar_id, self.app_id, self.transaction_id, &purchase.pack_id, owed).await {
            Ok(also_taken) => {
                pack_refund::settle_late(self.db, self.app_id, self.transaction_id).await?;
                Ok(Step::Taken(taken + also_taken))
            }
            Err(error) => {
                log::error(&format!("refund {} owes {owed} credits from the wallet, the cron retries: {error:?}", self.transaction_id));
                Ok(Step::Taken(taken))
            }
        }
    }
}
