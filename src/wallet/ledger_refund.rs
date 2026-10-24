use super::client::WalletTransport;
use super::d1::D1Exec;
use super::link::account_of;
use super::ops::WalletOps;
use super::{links, pack_refund};
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
    NotCredited,
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
            Reason::NotCredited => "not_credited",
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
    Refused(Reason),
}

/// Takes back the credits of the credit pack a refunded StoreKit 2 transaction describes, once per store transaction,
/// down to a zero balance. The transaction must be a consumable made in the app of a tenant that sells its product as a
/// pack, so a subscription or any other product is never touched, and it must have been credited to a linked wallet
/// (a purchase RevenueCat credited to a local balance is left to the RevenueCat webhook). The claim in
/// `purchase_refunds` is the one the RevenueCat webhook writes and the wallet key `reverse:<transactionId>` is the one
/// it reverses under, so a refund that reaches both paths is reversed once whichever arrives first. A claim whose
/// reversal failed is released, and a claim left by a crash is finished by the retry, because the wallet replays a key
/// it has seen.
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
    let Some(purchase) = pack_refund::credited_purchase(db, &app_id, &transaction.transaction_id).await? else {
        return Ok(Outcome::Refused(Reason::NotCredited));
    };
    if purchase.is_refunded() {
        return Ok(Outcome::Duplicate { purchase_id: purchase.id });
    }
    let Some(link) = links::read(db, &app_id, &purchase.user_id).await? else { return Ok(Outcome::Refused(Reason::NotLinked)) };
    let Account::Wallet { midgar_id, .. } = account_of(&link) else {
        return Err(AppError::InternalError("a link is not a wallet account".to_string()));
    };

    let newly_claimed = pack_refund::claim(db, &app_id, &transaction.transaction_id, &purchase.user_id, purchase.credits).await?;
    let description = pack_refund::description(&pack_id);
    let reference = crate::purchases::refund_reference(&transaction.transaction_id);
    let reversal = pack_refund::reversal(&transaction.transaction_id, "ledger", purchase.credits, &description, &reference);
    match (WalletOps { wallets, db }).reverse(&midgar_id, &app_id, &reversal).await {
        Ok((_, taken)) => {
            pack_refund::mark_refunded(db, &purchase.id).await?;
            Ok(Outcome::Reversed { purchase_id: purchase.id, credits: purchase.credits, taken })
        }
        Err(error) => {
            if newly_claimed {
                let _ = pack_refund::release(db, &app_id, &transaction.transaction_id).await;
            }
            Err(error)
        }
    }
}
