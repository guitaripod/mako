use worker::{console_error, D1Database, Result};

use crate::credits;
use crate::error::AppError;

/// Where a caller's credits live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Account {
    /// The per-app balance in D1 (`user_credits`), the only kind any request resolves to today.
    Local { app_id: String, user_id: String },
    /// The balance in the Durable Object wallet of a Midgar ID, partitioned by `app_id`. Nothing builds this
    /// yet; linking (P3.07 step 5) will.
    #[allow(dead_code)]
    Wallet { midgar_id: String, app_id: String, user_id: String },
}

impl Account {
    pub fn local(app_id: &str, user_id: &str) -> Self {
        Self::Local { app_id: app_id.to_string(), user_id: user_id.to_string() }
    }

    /// The D1 ids of a local account; a wallet account has no D1 balance to address.
    fn local_ids(&self) -> Result<(&str, &str)> {
        match self {
            Self::Local { app_id, user_id } => Ok((app_id, user_id)),
            Self::Wallet { .. } => Err(AppError::InternalError("wallet accounts are not enabled yet".to_string()).into()),
        }
    }
}

/// Every credit read and write a handler makes, addressed by `Account`. A local account runs the D1 code in
/// `credits` unchanged. The method names are the old function names, so a call site changes from
/// `deduct_credits(&auth.app_id, &auth.user_id, ..., &db)` to `ledgers.deduct_credits(&auth.account, ...)`.
pub struct Ledgers<'a> {
    db: &'a D1Database,
}

impl<'a> Ledgers<'a> {
    pub fn new(db: &'a D1Database) -> Self {
        Self { db }
    }

    pub async fn get_user_balance(&self, account: &Account) -> Result<i32> {
        let (app_id, user_id) = account.local_ids()?;
        credits::get_user_balance(app_id, user_id, self.db).await
    }

    pub async fn check_and_reserve_credits(&self, account: &Account, required_credits: u32) -> Result<()> {
        let (app_id, user_id) = account.local_ids()?;
        credits::check_and_reserve_credits(app_id, user_id, required_credits, self.db).await
    }

    pub async fn deduct_credits(&self, account: &Account, amount: u32, description: &str, reference_id: &str) -> Result<i32> {
        let (app_id, user_id) = account.local_ids()?;
        credits::deduct_credits(app_id, user_id, amount, description, reference_id, self.db).await
    }

    pub async fn add_credits(
        &self,
        account: &Account,
        amount: u32,
        transaction_type: &str,
        description: &str,
        reference_id: Option<&str>,
    ) -> Result<i32> {
        let (app_id, user_id) = account.local_ids()?;
        credits::add_credits(app_id, user_id, amount, transaction_type, description, reference_id, self.db).await
    }

    /// Best effort like the function it wraps: a wallet account is logged and skipped.
    pub async fn record_paywall_event(&self, account: &Account, capability: &str, balance: i32) {
        match account.local_ids() {
            Ok((app_id, user_id)) => credits::record_paywall_event(self.db, app_id, user_id, capability, balance).await,
            Err(e) => console_error!("paywall event skipped: {:?}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_account_addresses_its_d1_row() {
        let account = Account::local("dreameater", "u-1");
        assert_eq!(account.local_ids().unwrap(), ("dreameater", "u-1"));
    }

    #[test]
    fn a_wallet_account_has_no_d1_row() {
        let account = Account::Wallet { midgar_id: "m".into(), app_id: "dreameater".into(), user_id: "u-1".into() };
        assert!(account.local_ids().is_err());
    }
}
