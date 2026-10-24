use super::client::WalletTransport;
use super::d1::D1Exec;
use super::link::account_of;
use super::ops::WalletOps;
use super::pack_refund::{CreditedPurchase, Reverser, Settlement, Step};
use super::{links, pack_refund, purchase};
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
    NotLinked,
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
            Reason::NotLinked => "not_linked",
            Reason::LinkingOff => "wallet_linking_off",
        }
    }
}

/// What a forwarded refund came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The credits were taken back, down to a zero balance: `taken` is at most the pack's `credits`.
    Reversed { purchase_id: String, credits: u32, taken: u32 },
    /// The transaction was reversed before, by this route or the RevenueCat webhook.
    Duplicate { purchase_id: String },
    /// No credit path has seen the transaction yet: the refund is recorded and every later credit of it is refused.
    Recorded,
    Refused(Reason),
}

/// Takes back the credits of the credit pack a refunded StoreKit 2 transaction describes, once per store transaction,
/// down to a zero balance. The transaction must be a consumable made in the app of a tenant that sells its product as a
/// pack, so a subscription or any other product is never touched. A transaction nothing has credited yet is only
/// recorded (`Outcome::Recorded`): the claim in `purchase_refunds` makes the grant, the validate call and the RevenueCat
/// webhook refuse it afterwards. The claim is written before the purchase row is read (`pack_refund::claim_and_settle`),
/// so a credit cannot slip in between: the statement that creates a purchase row never runs once a claim exists. A
/// purchase still `pending` is finished first and then reversed, so a credit a crash left half done cannot outlive its
/// refund. The reversal needs the purchase to be in a linked wallet (a purchase RevenueCat credited to a local balance
/// is left to the RevenueCat webhook, and the claim this call wrote is released). The claim is the one the RevenueCat
/// webhook writes and the wallet key `reverse:<transactionId>` is the one it reverses under, so a refund that reaches
/// both paths is reversed once whichever arrives first. A claim whose reversal failed is released, and a claim left by
/// a crash is finished by the retry, because the wallet replays a key it has seen.
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
    let reverser = LedgerReverser { db, wallets, app_id: &app_id, pack_id: &pack_id, transaction_id: &transaction.transaction_id };
    Ok(match pack_refund::claim_and_settle(db, &app_id, &transaction.transaction_id, &reverser).await? {
        Settlement::Tombstone => Outcome::Recorded,
        Settlement::Settled(purchase) => Outcome::Duplicate { purchase_id: purchase.id },
        Settlement::Reversed { purchase, taken } => Outcome::Reversed { purchase_id: purchase.id, credits: purchase.credits, taken },
        Settlement::Declined(_) => Outcome::Refused(Reason::NotLinked),
    })
}

/// The ledger route's half of a refund: the purchase's user must be linked to a wallet, a purchase still `pending` is
/// finished in that wallet, and the pack is reversed under `reverse:<transactionId>`.
struct LedgerReverser<'a, D: D1Exec, W: WalletTransport> {
    db: &'a D,
    wallets: &'a W,
    app_id: &'a str,
    pack_id: &'a str,
    transaction_id: &'a str,
}

impl<D: D1Exec, W: WalletTransport> Reverser for LedgerReverser<'_, D, W> {
    async fn reverse(&self, purchase: &CreditedPurchase, _first_claim: bool) -> Result<Step, AppError> {
        let Some(link) = links::read(self.db, self.app_id, &purchase.user_id).await? else { return Ok(Step::Declined) };
        let account = account_of(&link);
        let Account::Wallet { midgar_id, .. } = &account else {
            return Err(AppError::InternalError("a link is not a wallet account".to_string()));
        };
        if purchase.is_pending() {
            let row = purchase::read(self.db, &purchase.id).await?.ok_or_else(|| AppError::InternalError("purchase row vanished".to_string()))?;
            match purchase::complete_in_wallet(self.db, self.wallets, &account, &row).await {
                Ok(_) => {}
                Err(AppError::Refunded(_)) => return Ok(Step::Settled),
                Err(error) => return Err(error),
            }
        }
        let description = pack_refund::description(self.pack_id);
        let reference = crate::purchases::refund_reference(self.transaction_id);
        let reversal = pack_refund::reversal(self.transaction_id, "ledger", purchase.credits, &description, &reference);
        let (_, taken) = (WalletOps { wallets: self.wallets, db: self.db }).reverse(midgar_id, self.app_id, &reversal).await?;
        Ok(Step::Taken(taken))
    }
}
