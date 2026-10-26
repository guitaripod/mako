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

/// Credits the consumable a verified StoreKit 2 transaction describes to the buyer its `appAccountToken` names, once per
/// store transaction: the wallet of a Midgar ID some user is linked to (MidgarStore apps), or the user whose mako id it
/// is (RevenueCat apps), in that user's wallet when linked and in the D1 balance otherwise. The grant key is
/// `tx:<transactionId>` and the purchase row is the RevenueCat one, which the RevenueCat webhook and the client's
/// validate call share, so no two paths credit one purchase twice. The tenant comes from the product id, and the
/// transaction is held to the rules of the validate call (bundle, product, consumable, not revoked, purchased, quantity
/// one). A Midgar ID no user is linked to is refused: identity may not know it, and the client's
/// validate call credits it. A purchase row left `pending` by a crash is finished, and one that belongs to another
/// user who is not linked is left to its own path. A
/// transaction whose refund was claimed (it arrived first, or the pack was already reversed) is refused for good, also
/// when the claim lands between this call's checks: the purchase row is created by one statement that does not run
/// while a claim exists (`purchase::record_pending`), and the flip to `completed` never overwrites `refunded`.
pub async fn credit<D: D1Exec, W: WalletTransport>(
    db: &D,
    wallets: &W,
    transaction: &LedgerTransaction,
) -> Result<Outcome, AppError> {
    let token = match transaction.app_account_token.as_deref().map(str::to_lowercase) {
        Some(token) if !token.is_empty() => token,
        _ => return Ok(Outcome::Refused(Reason::NotAMidgarId)),
    };
    let Some((app_id, _prefix)) = tenant_of_product(db, &transaction.product_id).await? else {
        return Ok(Outcome::Refused(Reason::UnknownProduct));
    };
    let Some(facts) = tenant_facts(db, &app_id).await? else { return Ok(Outcome::Refused(Reason::UnknownTenant)) };
    let expected = Expected {
        product_id: &transaction.product_id,
        midgar_id: &token,
        tenant_slug: &facts.slug,
        tenant_bundle: facts.bundle.as_deref(),
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
    let Some(buyer) = buyer_of(db, &app_id, &token).await? else {
        return Ok(Outcome::Refused(if is_midgar_id(&token) { Reason::NotLinked } else { Reason::NotAMidgarId }));
    };

    let credits = (pack.credits + pack.bonus_credits).max(0) as u32;
    let purchase_id = match purchase::record_pending(
        db,
        &app_id,
        &buyer,
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
    let completed = match links::read(db, &app_id, &row.user_id).await? {
        Some(owner) => purchase::complete_in_wallet(db, wallets, &account_of(&owner), &row).await,
        None if row.user_id == buyer => purchase::complete_locally(db, &purchase_id).await,
        None => return Ok(Outcome::Refused(Reason::Elsewhere)),
    };
    match completed {
        Ok(true) => Ok(Outcome::Credited { app_id, user_id: row.user_id, purchase_id, pack_id, credits, amount_usd_cents: row.amount_usd_cents }),
        Ok(false) => Ok(Outcome::Duplicate { purchase_id }),
        Err(AppError::Refunded(_)) => Ok(Outcome::Refused(Reason::Refunded)),
        Err(error) => Err(error),
    }
}

/// The user of the tenant the transaction's `appAccountToken` names: the oldest user linked to it as a Midgar ID, else
/// the tenant's user with that id. Apps on the RevenueCat SDK log RevenueCat in as their mako user and RevenueCat copies
/// that UUID into `appAccountToken`, so their purchases name the user directly, linked to a wallet or not.
async fn buyer_of<D: D1Exec>(db: &D, app_id: &str, token: &str) -> Result<Option<String>, AppError> {
    if let Some(link) = links::oldest_for_midgar(db, app_id, token).await? {
        return Ok(Some(link.user_id));
    }
    if links::user_exists(db, app_id, token).await? {
        return Ok(Some(token.to_string()));
    }
    Ok(None)
}
