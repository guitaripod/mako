use worker::{D1Database, Env, Result};

use crate::credits;
use crate::error::AppError;
use crate::wallet::client::DoWallets;
use crate::wallet::link::{self, Linker};
use crate::wallet::ops::{Charge, Grant, GrantResult, Reversal, WalletOps};
use crate::wallet::purchase;
use crate::wallet::{resolve, target};

/// Where a caller's credits live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Account {
    /// The per-app balance in D1 (`user_credits`): every account while `WALLET_LINKING` is off, and the account
    /// of a user who never linked a Midgar ID.
    Local { app_id: String, user_id: String },
    /// The balance in the Durable Object wallet of a Midgar ID, partitioned by `app_id`.
    Wallet { midgar_id: String, app_id: String, user_id: String },
}

impl Account {
    pub fn local(app_id: &str, user_id: &str) -> Self {
        Self::Local { app_id: app_id.to_string(), user_id: user_id.to_string() }
    }

    pub fn app_id(&self) -> &str {
        match self {
            Self::Local { app_id, .. } | Self::Wallet { app_id, .. } => app_id,
        }
    }

    pub fn user_id(&self) -> &str {
        match self {
            Self::Local { user_id, .. } | Self::Wallet { user_id, .. } => user_id,
        }
    }

    /// The Midgar ID behind a wallet account, `None` for a local one.
    pub fn midgar_id(&self) -> Option<&str> {
        match self {
            Self::Local { .. } => None,
            Self::Wallet { midgar_id, .. } => Some(midgar_id),
        }
    }
}

/// True when an error is the 402 a spend or a check answers with.
fn is_payment_required(error: &worker::Error) -> bool {
    matches!(AppError::from(worker::Error::RustError(error.to_string())), AppError::PaymentRequired(_))
}

/// The idempotency key of a spend: the reference each handler mints once per charge.
pub fn spend_key(reference: &str) -> String {
    format!("spend:{reference}")
}

/// The wallet key of a flat-capability charge. A reference the caller named is its idempotency key within that
/// capability, so a retry of the same charge is free and the same reference on another capability is another charge.
/// Without one every call is a new charge, so the key carries a nonce and can never be shared with another call,
/// which would otherwise be answered as a repeat and go unpaid.
pub fn capability_key(named_reference: Option<&str>, capability: &str, nonce: &str) -> String {
    match named_reference.filter(|r| !r.is_empty()) {
        Some(named) => format!("spend:{capability}:{named}"),
        None => format!("spend:{capability}:{nonce}"),
    }
}

/// Every credit read and write a handler makes, addressed by `Account`. A local account runs the D1 code in
/// `credits` unchanged. A wallet account goes to the Durable Object of its Midgar ID, with an idempotency key per
/// change, and fails closed with 503 `wallet_unavailable` when the object cannot answer: there is no D1 fallback.
/// The method names are the old function names, so a call site changes from
/// `deduct_credits(&auth.app_id, &auth.user_id, ..., &db)` to `ledgers.deduct_credits(&auth.account, ...)`.
pub struct Ledgers<'a> {
    db: &'a D1Database,
    wallets: Option<DoWallets>,
    identity: Option<target::IdentityService>,
    linking: bool,
}

impl<'a> Ledgers<'a> {
    pub fn new(env: &Env, db: &'a D1Database) -> Self {
        let linking = link::enabled(env);
        let wallets = if linking { env.durable_object("WALLET").ok().map(DoWallets) } else { None };
        let identity = if linking { target::IdentityService::open(env).ok() } else { None };
        Self { db, wallets, identity, linking }
    }

    /// True when `WALLET_LINKING` is `on`. Off, every account is local and no wallet code runs.
    pub fn linking(&self) -> bool {
        self.linking
    }

    fn ops(&self) -> std::result::Result<WalletOps<'_, DoWallets, D1Database>, AppError> {
        let wallets = self.wallets.as_ref().ok_or(AppError::WalletUnavailable)?;
        Ok(WalletOps { wallets, db: self.db })
    }

    fn linker(&self) -> std::result::Result<Linker<'_, D1Database, DoWallets>, AppError> {
        let wallets = self.wallets.as_ref().ok_or(AppError::WalletUnavailable)?;
        Ok(Linker { db: self.db, wallets })
    }

    /// The wallet account a local user turned into while a request was in flight, if it did: the link lookup a 402
    /// triggers once, finishing a `moving` link first. A wallet that cannot answer is an error the caller reports as
    /// 503, not a missing link; any other failure is logged and read as no link.
    async fn relinked(&self, app_id: &str, user_id: &str) -> Result<Option<Account>> {
        if !self.linking {
            return Ok(None);
        }
        match self.linker()?.relink(app_id, user_id).await {
            Ok(account) => Ok(account),
            Err(AppError::WalletUnavailable) => Err(AppError::WalletUnavailable.into()),
            Err(e) => {
                crate::wallet::log::error(&format!("wallet relink failed: {e:?}"));
                Ok(None)
            }
        }
    }

    pub async fn get_user_balance(&self, account: &Account) -> Result<i32> {
        match account {
            Account::Local { app_id, user_id } => credits::get_user_balance(app_id, user_id, self.db).await,
            Account::Wallet { midgar_id, app_id, .. } => Ok(self.ops()?.balance(midgar_id, app_id).await?),
        }
    }

    pub async fn check_and_reserve_credits(&self, account: &Account, required_credits: u32) -> Result<()> {
        match account {
            Account::Local { app_id, user_id } => {
                match credits::check_and_reserve_credits(app_id, user_id, required_credits, self.db).await {
                    Err(e) if is_payment_required(&e) => match self.relinked(app_id, user_id).await? {
                        Some(wallet) => Box::pin(self.check_and_reserve_credits(&wallet, required_credits)).await,
                        None => Err(e),
                    },
                    other => other,
                }
            }
            Account::Wallet { midgar_id, app_id, .. } => Ok(self.ops()?.check(midgar_id, app_id, required_credits).await?),
        }
    }

    /// Debits `amount`. A wallet account is charged under `spend:<reference_id>`, so a repeat of the same reference
    /// charges once.
    pub async fn deduct_credits(&self, account: &Account, amount: u32, description: &str, reference_id: &str) -> Result<i32> {
        if reference_id.is_empty() && matches!(account, Account::Wallet { .. }) {
            return Err(AppError::InternalError("a wallet charge needs a reference to key it by".to_string()).into());
        }
        self.deduct_credits_keyed(account, amount, description, reference_id, &spend_key(reference_id)).await
    }

    /// `deduct_credits` with the wallet idempotency key named by the caller, for a reference that two different
    /// debits share.
    pub async fn deduct_credits_keyed(&self, account: &Account, amount: u32, description: &str, reference_id: &str, key: &str) -> Result<i32> {
        match account {
            Account::Local { app_id, user_id } => {
                match credits::deduct_credits(app_id, user_id, amount, description, reference_id, self.db).await {
                    Err(e) if is_payment_required(&e) => match self.relinked(app_id, user_id).await? {
                        Some(wallet) => Box::pin(self.deduct_credits_keyed(&wallet, amount, description, reference_id, key)).await,
                        None => Err(e),
                    },
                    other => other,
                }
            }
            Account::Wallet { midgar_id, app_id, .. } => {
                let charge = Charge { key: key.to_string(), amount, description, reference: reference_id };
                Ok(self.ops()?.spend(midgar_id, app_id, &charge).await?)
            }
        }
    }

    /// Adds credits to a local account. A wallet account needs an idempotency key, so it goes through `grant`.
    pub async fn add_credits(
        &self,
        account: &Account,
        amount: u32,
        transaction_type: &str,
        description: &str,
        reference_id: Option<&str>,
    ) -> Result<i32> {
        match account {
            Account::Local { app_id, user_id } => {
                credits::add_credits(app_id, user_id, amount, transaction_type, description, reference_id, self.db).await
            }
            Account::Wallet { .. } => Err(AppError::InternalError("a wallet grant needs an idempotency key".to_string()).into()),
        }
    }

    /// Credits an account once per `grant.key`. A local account has no keys and adds the credits as before.
    pub async fn grant(&self, account: &Account, grant: &Grant<'_>) -> Result<GrantResult> {
        match account {
            Account::Local { .. } => {
                let kind = grant.kind.as_str();
                let transaction_type = if kind == "adjust" { "admin_adjustment" } else { kind };
                let balance = self.add_credits(account, grant.amount, transaction_type, grant.description, grant.reference).await?;
                Ok(GrantResult { balance, replayed: false })
            }
            Account::Wallet { midgar_id, app_id, .. } => Ok(self.ops()?.grant(midgar_id, app_id, grant).await?),
        }
    }

    /// Takes back at most `reversal.amount`, returning the balance after and what was taken. A local account clamps
    /// through the D1 compare-and-swap loop; a wallet account through the object, once per key.
    pub async fn reverse(&self, account: &Account, reversal: &Reversal<'_>, transaction_type: &str) -> Result<(i32, u32)> {
        match account {
            Account::Local { app_id, user_id } => {
                let (balance, taken) = credits::deduct_credits_clamped_with_reference(
                    app_id,
                    user_id,
                    reversal.amount,
                    transaction_type,
                    reversal.description,
                    reversal.reference,
                    self.db,
                )
                .await?;
                if taken >= reversal.amount {
                    return Ok((balance, taken));
                }
                let Some(wallet) = self.relinked(app_id, user_id).await? else { return Ok((balance, taken)) };
                let rest = Reversal { key: format!("{}:late", reversal.key), amount: reversal.amount - taken, ..reversal.clone() };
                let (after, also_taken) = Box::pin(self.reverse(&wallet, &rest, transaction_type)).await?;
                Ok((after, taken + also_taken))
            }
            Account::Wallet { midgar_id, app_id, .. } => Ok(self.ops()?.reverse(midgar_id, app_id, reversal).await?),
        }
    }

    /// Best effort like the function it wraps.
    pub async fn record_paywall_event(&self, account: &Account, capability: &str, balance: i32) {
        credits::record_paywall_event(self.db, account.app_id(), account.user_id(), capability, balance).await
    }

    /// The account a grant addressed to a RevenueCat `app_user_id` goes to (design section 5). Only meaningful with
    /// linking on; off, the id is a local user as before.
    pub async fn credit_target(&self, app_id: &str, id: &str) -> Result<Account> {
        if !self.linking {
            return Ok(Account::local(app_id, id));
        }
        let identity = self.identity.as_ref().ok_or_else(|| AppError::InternalError("identity lookup is not configured".to_string()))?;
        Ok(resolve::credit_target(true, self.db, identity, app_id, id).await?)
    }

    /// The account a stored user has right now, without running the saga: the wallet when a link row exists, local
    /// otherwise. Off, always local.
    pub async fn account_for_user(&self, app_id: &str, user_id: &str) -> Result<Account> {
        if !self.linking {
            return Ok(Account::local(app_id, user_id));
        }
        Ok(resolve::account_for_user(true, &self.linker()?, app_id, user_id).await?)
    }

    /// Records a RevenueCat purchase as `pending` once per store transaction and returns its id.
    pub async fn record_pending_purchase(
        &self,
        app_id: &str,
        user_id: &str,
        pack_id: &str,
        credits: u32,
        amount_usd_cents: u32,
        payment_id: &str,
    ) -> Result<String> {
        Ok(purchase::record_pending(self.db, app_id, user_id, pack_id, credits, amount_usd_cents, payment_id).await?)
    }

    /// Completes a purchase into whichever account its user has now. A wallet account grants first under the
    /// purchase's key and flips after, so a crash in between leaves `pending` for a retry to finish. A local account
    /// keeps the flip-then-credit order of `credits::complete_purchase`, but looks at the link again once the flip is
    /// won: a user who linked while the purchase was being processed is credited in the wallet under the same key a
    /// concurrent wallet caller uses, so the purchase can never be credited in both places.
    pub async fn complete_purchase(&self, purchase_id: &str) -> Result<()> {
        if !self.linking {
            return credits::complete_purchase(purchase_id, self.db).await;
        }
        let Some(row) = purchase::read(self.db, purchase_id).await? else {
            return credits::complete_purchase(purchase_id, self.db).await;
        };
        let account = self.account_for_user(&row.app_id, &row.user_id).await?;
        let wallets = self.wallets.as_ref().ok_or(AppError::WalletUnavailable)?;
        if matches!(account, Account::Wallet { .. }) {
            if purchase::complete_in_wallet(self.db, wallets, &account, &row).await? {
                credits::tag_purchase_attribution(self.db, &row.app_id, &row.user_id, &row.id, &row.pack_id, row.amount_usd_cents).await;
            }
            return Ok(());
        }
        let Some(claimed) = credits::claim_purchase(purchase_id, self.db).await? else {
            return credits::complete_purchase_already_claimed(purchase_id, self.db).await;
        };
        match self.account_for_user(&row.app_id, &row.user_id).await {
            Ok(Account::Local { .. }) => credits::credit_claimed_purchase(&claimed, purchase_id, self.db).await,
            Ok(wallet) => {
                if let Err(e) = purchase::grant_purchase(self.db, wallets, &wallet, &row).await {
                    self.release_claim(purchase_id).await;
                    return Err(e.into());
                }
                credits::tag_purchase_attribution(self.db, &row.app_id, &row.user_id, &row.id, &row.pack_id, row.amount_usd_cents).await;
                Ok(())
            }
            Err(e) => {
                self.release_claim(purchase_id).await;
                Err(e)
            }
        }
    }

    /// Puts a claimed purchase back to `pending` after this call could not credit it, so a retry or the cron does.
    async fn release_claim(&self, purchase_id: &str) {
        if let Err(e) = purchase::release_claim(self.db, purchase_id).await {
            crate::wallet::log::error(&format!("purchase {purchase_id} stays completed without a credit: {e:?}"));
        }
    }

    /// Cron: finishes RevenueCat purchases that stayed `pending` for more than five minutes. Returns how many it
    /// completed; one that fails is logged and left for the next run.
    pub async fn sweep_pending_purchases(&self) -> Result<u32> {
        let mut completed = 0;
        for id in purchase::stale_revenuecat(self.db, purchase::SWEEP_LIMIT).await? {
            match self.complete_purchase(&id).await {
                Ok(()) => completed += 1,
                Err(e) => crate::wallet::log::error(&format!("pending purchase {id} could not be completed: {e:?}")),
            }
        }
        Ok(completed)
    }

    /// One page of the transactions list of a wallet account: wallet entries newest first, then the older D1 rows.
    /// Returns the rows of the page and the total over both.
    pub async fn wallet_transactions(&self, account: &Account, offset: i32, per_page: i32) -> Result<(Vec<serde_json::Value>, i64)> {
        let Account::Wallet { midgar_id, app_id, user_id } = account else {
            return Err(AppError::InternalError("wallet_transactions needs a wallet account".to_string()).into());
        };
        let (offset, per_page) = (offset.max(0) as usize, per_page.max(0) as usize);
        let (mut rows, wallet_total) = self.ops()?.history(midgar_id, app_id, user_id, offset, per_page).await?;
        let d1_total = credits::count_user_transactions(app_id, user_id, self.db).await?;
        let wanted_end = offset + per_page;
        if wanted_end > wallet_total {
            let d1_offset = offset.saturating_sub(wallet_total);
            let d1_limit = wanted_end - offset.max(wallet_total);
            let older = credits::get_user_transactions(app_id, user_id, d1_limit as i32, d1_offset as i32, self.db).await?;
            rows.extend(older.iter().filter_map(|t| serde_json::to_value(t).ok()));
        }
        Ok((rows, wallet_total as i64 + d1_total))
    }

    /// The spend stored under a charge reference in a wallet account, with the amount and time of the charge.
    pub async fn wallet_spend(&self, account: &Account, reference: &str) -> Result<Option<crate::wallet::StoredEntry>> {
        let Account::Wallet { midgar_id, app_id, .. } = account else {
            return Err(AppError::InternalError("wallet_spend needs a wallet account".to_string()).into());
        };
        Ok(self.ops()?.spend_entry(midgar_id, app_id, &spend_key(reference)).await?)
    }

    /// Account deletion: erases the tenant's partition of a linked user's wallet. Nothing to do for a local account.
    /// Fails (and the deletion with it) when the wallet cannot answer, so no data is left behind unnoticed.
    pub async fn erase_partition(&self, account: &Account) -> Result<()> {
        let Account::Wallet { midgar_id, app_id, .. } = account else { return Ok(()) };
        let wallets = self.wallets.as_ref().ok_or(AppError::WalletUnavailable)?;
        crate::wallet::client::Wallets { transport: wallets }.erase(midgar_id, app_id).await?;
        let _ = self.db.prepare("DELETE FROM wallet_summary WHERE app_id = ?1 AND midgar_id = ?2").bind(&[app_id.into(), midgar_id.into()])?.run().await;
        Ok(())
    }

    /// Cron: finishes stale links, sweeps residue and completes stale purchases. Each part logs its own failure and
    /// the others still run.
    pub async fn run_wallet_cron(&self) {
        if !self.linking {
            return;
        }
        match self.linker() {
            Ok(linker) => {
                match linker.resume_stale(link::CRON_LIMIT).await {
                    Ok(n) if n > 0 => crate::wallet::log::info(&format!("wallet cron resumed {n} links")),
                    Ok(_) => {}
                    Err(e) => crate::wallet::log::error(&format!("wallet link resume failed: {e:?}")),
                }
                match linker.sweep_residue(link::CRON_LIMIT).await {
                    Ok(n) if n > 0 => crate::wallet::log::info(&format!("wallet cron moved residue for {n} users")),
                    Ok(_) => {}
                    Err(e) => crate::wallet::log::error(&format!("wallet residue sweep failed: {e:?}")),
                }
            }
            Err(e) => crate::wallet::log::error(&format!("wallet cron has no wallet binding: {e:?}")),
        }
        match self.sweep_pending_purchases().await {
            Ok(n) if n > 0 => crate::wallet::log::info(&format!("wallet cron completed {n} pending purchases")),
            Ok(_) => {}
            Err(e) => crate::wallet::log::error(&format!("pending purchase sweep failed: {e:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_account_addresses_its_d1_row() {
        let account = Account::local("dreameater", "u-1");
        assert_eq!((account.app_id(), account.user_id(), account.midgar_id()), ("dreameater", "u-1", None));
    }

    #[test]
    fn a_wallet_account_names_its_midgar_id() {
        let account = Account::Wallet { midgar_id: "m".into(), app_id: "dreameater".into(), user_id: "u-1".into() };
        assert_eq!((account.app_id(), account.user_id(), account.midgar_id()), ("dreameater", "u-1", Some("m")));
    }

    #[test]
    fn a_spend_key_carries_the_reference() {
        assert_eq!(spend_key("chat:abc"), "spend:chat:abc");
    }

    #[test]
    fn a_named_capability_reference_is_the_idempotency_key() {
        assert_eq!(capability_key(Some("invoice-7"), "peppol.send", "n1"), capability_key(Some("invoice-7"), "peppol.send", "n2"));
        assert_eq!(capability_key(Some("invoice-7"), "peppol.send", "n1"), "spend:peppol.send:invoice-7");
        assert_ne!(capability_key(Some("invoice-7"), "peppol.send", "n1"), capability_key(Some("invoice-7"), "other.send", "n1"));
    }

    #[test]
    fn an_unnamed_capability_charge_never_shares_a_key() {
        for named in [None, Some("")] {
            let first = capability_key(named, "peppol.send", "n1");
            let second = capability_key(named, "peppol.send", "n2");
            assert_ne!(first, second);
            assert_ne!(first, spend_key("peppol.send"));
            assert_ne!(first, "spend:");
        }
    }

    #[test]
    fn a_402_is_recognised_through_the_worker_error() {
        let payment: worker::Error = AppError::PaymentRequired("no".into()).into();
        let other: worker::Error = AppError::InternalError("no".into()).into();
        assert!(is_payment_required(&payment));
        assert!(!is_payment_required(&other));
    }
}
