use super::client::WalletTransport;
use super::d1::D1Exec;
use super::link::account_of;
use super::target::is_midgar_id;
use super::{links, pack_refund, purchase};
use crate::credits::packs_for_app;
use crate::error::AppError;
use crate::storekit::{check, pack_of_product, tenant_facts, tenant_of_product, Expected, LedgerTransaction, Refusal};

/// Why the ledger's transaction is not credited. Every reason is final: the ledger is told so and does not retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    NotAMidgarId,
    UnknownProduct,
    UnknownTenant,
    UnknownPack,
    Transaction(Refusal),
    NotLinked,
    LinkingOff,
    Elsewhere,
    Refunded,
}

impl Reason {
    /// The stable spelling the answer carries.
    pub fn as_str(&self) -> &'static str {
        match self {
            Reason::NotAMidgarId => "not_a_midgar_id",
            Reason::UnknownProduct => "unknown_product",
            Reason::UnknownTenant => "unknown_tenant",
            Reason::UnknownPack => "unknown_pack",
            Reason::NotLinked => "not_linked",
            Reason::LinkingOff => "wallet_linking_off",
            Reason::Elsewhere => "credited_elsewhere",
            Reason::Refunded => "refunded",
            Reason::Transaction(Refusal::WrongApp) => "wrong_app",
            Reason::Transaction(Refusal::NotYours) => "not_yours",
            Reason::Transaction(Refusal::WrongProduct) => "wrong_product",
            Reason::Transaction(Refusal::NotConsumable) => "not_consumable",
            Reason::Transaction(Refusal::Revoked) => "revoked",
            Reason::Transaction(Refusal::NotPurchased) => "not_purchased",
            Reason::Transaction(Refusal::Quantity) => "quantity",
            Reason::Transaction(Refusal::Sandbox) => "sandbox",
        }
    }
}

/// What a forwarded transaction came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// This call credited the pack; the caller tags the purchase with its ad attribution.
    Credited { app_id: String, user_id: String, purchase_id: String, pack_id: String, credits: u32, amount_usd_cents: i64 },
    /// The transaction was credited before, by this route, the RevenueCat webhook or the client's validate call.
    Duplicate { purchase_id: String },
    Refused(Reason),
}

/// Credits the consumable a verified StoreKit 2 transaction describes into the wallet of the Midgar ID in its
/// `appAccountToken`, once per store transaction. The grant key is `tx:<transactionId>`, the key the RevenueCat webhook
/// and the client's validate call use, so the three paths cannot credit one purchase twice. The tenant comes from the
/// product id, and the transaction is held to the rules of the validate call (bundle, product, consumable, not
/// revoked, purchased, quantity one, the sandbox rule). Only a Midgar ID some user is already linked to is credited: an
/// unlinked ID would be credited a second time through RevenueCat's local `credit_purchases` row. A purchase row left
/// `pending` by a crash is finished, and one that belongs to a user who is not linked is left to its own path. A
/// transaction whose refund was claimed (it arrived first, or the pack was already reversed) is refused for good, also
/// when the claim lands between this call's checks: the purchase row is created by one statement that does not run
/// while a claim exists (`purchase::record_pending`), and the flip to `completed` never overwrites `refunded`.
pub async fn credit<D: D1Exec, W: WalletTransport>(
    db: &D,
    wallets: &W,
    production: bool,
    transaction: &LedgerTransaction,
) -> Result<Outcome, AppError> {
    let midgar_id = match transaction.app_account_token.as_deref().map(str::to_lowercase) {
        Some(id) if is_midgar_id(&id) => id,
        _ => return Ok(Outcome::Refused(Reason::NotAMidgarId)),
    };
    let Some((app_id, _prefix)) = tenant_of_product(db, &transaction.product_id).await? else {
        return Ok(Outcome::Refused(Reason::UnknownProduct));
    };
    let Some(facts) = tenant_facts(db, &app_id).await? else { return Ok(Outcome::Refused(Reason::UnknownTenant)) };
    let expected = Expected {
        product_id: &transaction.product_id,
        midgar_id: &midgar_id,
        tenant_slug: &facts.slug,
        tenant_bundle: facts.bundle.as_deref(),
        strict: facts.strict,
        production,
    };
    if let Err(refusal) = check(transaction, &expected) {
        return Ok(Outcome::Refused(Reason::Transaction(refusal)));
    }
    let Some(pack_id) = pack_of_product(db, &app_id, &transaction.product_id).await? else {
        return Ok(Outcome::Refused(Reason::UnknownProduct));
    };
    let Some(pack) = packs_for_app(db, &app_id).await.into_iter().find(|p| p.id == pack_id) else {
        return Ok(Outcome::Refused(Reason::UnknownPack));
    };
    if pack_refund::is_claimed(db, &app_id, &transaction.transaction_id).await? {
        return Ok(Outcome::Refused(Reason::Refunded));
    }
    let Some(link) = links::oldest_for_midgar(db, &app_id, &midgar_id).await? else {
        return Ok(Outcome::Refused(Reason::NotLinked));
    };

    let credits = (pack.credits + pack.bonus_credits).max(0) as u32;
    let purchase_id = match purchase::record_pending(
        db,
        &app_id,
        &link.user_id,
        &pack_id,
        credits,
        pack.price_usd_cents.max(0) as u32,
        &transaction.transaction_id,
    )
    .await
    {
        Ok(id) => id,
        Err(AppError::Refunded(_)) => return Ok(Outcome::Refused(Reason::Refunded)),
        Err(error) => return Err(error),
    };
    let Some(row) = purchase::read(db, &purchase_id).await? else {
        return Err(AppError::InternalError("purchase row vanished".to_string()));
    };
    if row.status == "refunded" {
        return Ok(Outcome::Refused(Reason::Refunded));
    }
    if row.status != "pending" {
        return Ok(Outcome::Duplicate { purchase_id });
    }
    let Some(owner) = links::read(db, &app_id, &row.user_id).await? else {
        return Ok(Outcome::Refused(Reason::Elsewhere));
    };
    match purchase::complete_in_wallet(db, wallets, &account_of(&owner), &row).await {
        Ok(true) => Ok(Outcome::Credited { app_id, user_id: row.user_id, purchase_id, pack_id, credits, amount_usd_cents: row.amount_usd_cents }),
        Ok(false) => Ok(Outcome::Duplicate { purchase_id }),
        Err(AppError::Refunded(_)) => Ok(Outcome::Refused(Reason::Refunded)),
        Err(error) => Err(error),
    }
}
