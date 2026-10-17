use chrono::{TimeZone, Utc};
use serde_json::Value;

use super::client::{DoWallets, Moved, Spent, Totals, WalletTransport, Wallets};
use super::d1::D1Exec;
use super::log;
use super::sql::Val;
use super::types::{Entry, Kind, StoredEntry};
use crate::error::AppError;

/// A debit of a wallet partition, keyed so a repeat of the same request charges once.
#[derive(Debug, Clone)]
pub struct Charge<'a> {
    pub key: String,
    pub amount: u32,
    pub description: &'a str,
    pub reference: &'a str,
}

/// A credit to a wallet partition, keyed so a repeat of the same event credits once.
#[derive(Debug, Clone)]
pub struct Grant<'a> {
    pub key: String,
    pub kind: Kind,
    pub source: &'static str,
    pub amount: u32,
    pub description: &'a str,
    pub reference: Option<&'a str>,
}

/// A clamped debit of a wallet partition: at most `amount` is taken.
#[derive(Debug, Clone)]
pub struct Reversal<'a> {
    pub key: String,
    pub kind: Kind,
    pub source: &'static str,
    pub amount: u32,
    pub description: &'a str,
    pub reference: Option<&'a str>,
}

/// What a grant did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantResult {
    pub balance: i32,
    pub replayed: bool,
}

/// A wallet answer is a credit count a request can carry; a balance past `i32` cannot occur under the amount cap.
fn credits(value: i64) -> i32 {
    value.clamp(0, i32::MAX as i64) as i32
}

/// The wallet entry kind of a D1 transaction type, for call sites that name the type the old way.
pub fn kind_of(transaction_type: &str) -> Kind {
    match transaction_type {
        "purchase" => Kind::Purchase,
        "bonus" => Kind::Bonus,
        "refund" => Kind::Refund,
        _ => Kind::Adjust,
    }
}

/// The `type` the transactions list shows for a wallet entry, using the spellings the D1 ledger uses.
fn transaction_type(kind: Kind) -> &'static str {
    match kind {
        Kind::Reversal | Kind::Adjust => "admin_adjustment",
        other => other.as_str(),
    }
}

const MIRROR: &str = "INSERT INTO wallet_summary (app_id, midgar_id, balance, purchased, spent, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
    ON CONFLICT(app_id, midgar_id) DO UPDATE SET balance = excluded.balance, purchased = excluded.purchased,
    spent = excluded.spent, updated_at = excluded.updated_at";

/// Everything the credit code does to a wallet account: balance, check, spend, grant, reverse, history and the
/// advisory D1 mirror. Spends and balance reads fail closed with `WalletUnavailable`; there is no D1 fallback.
pub struct WalletOps<'a, W: WalletTransport, D: D1Exec> {
    pub wallets: &'a W,
    pub db: &'a D,
}

impl<W: WalletTransport, D: D1Exec> WalletOps<'_, W, D> {
    fn client(&self) -> Wallets<'_, W> {
        Wallets { transport: self.wallets }
    }

    /// Writes the tenant's totals to `wallet_summary`. A failure is logged and never fails the request: no
    /// decision reads the table.
    pub async fn mirror(&self, midgar_id: &str, app_id: &str, totals: Totals) {
        let now = Utc::now().to_rfc3339();
        let params: [Val; 6] = [app_id.into(), midgar_id.into(), totals.balance.into(), totals.purchased.into(), totals.spent.into(), now.into()];
        if let Err(e) = self.db.exec(MIRROR, &params).await {
            log::error(&format!("wallet_summary mirror failed: {e:?}"));
        }
    }

    /// Mirrors the totals of a change the wallet applied or recognised.
    async fn mirrored(&self, midgar_id: &str, app_id: &str, moved: Moved) -> Moved {
        self.mirror(midgar_id, app_id, moved.totals).await;
        moved
    }

    /// The tenant's balance in the wallet of `midgar_id`.
    pub async fn balance(&self, midgar_id: &str, app_id: &str) -> Result<i32, AppError> {
        Ok(credits(self.client().balance(midgar_id, app_id).await?.balance))
    }

    /// Fails with 402 when the balance is below `required`; holds nothing, like the D1 check.
    pub async fn check(&self, midgar_id: &str, app_id: &str, required: u32) -> Result<(), AppError> {
        let balance = self.balance(midgar_id, app_id).await?;
        if balance < required as i32 {
            return Err(AppError::PaymentRequired(
                format!("Insufficient credits. Need {} credits, have {}. Purchase more at /credits", required, balance),
                Some(balance),
            ));
        }
        Ok(())
    }

    /// Debits the wallet once per `charge.key` and returns the balance after. A repeat of the key returns the
    /// current balance and charges nothing.
    pub async fn spend(&self, midgar_id: &str, app_id: &str, charge: &Charge<'_>) -> Result<i32, AppError> {
        if charge.amount == 0 {
            return self.balance(midgar_id, app_id).await;
        }
        let entry = Entry {
            key: charge.key.clone(),
            tenant: app_id.to_string(),
            kind: Kind::Spend,
            amount: -(charge.amount as i64),
            source: "usage".to_string(),
            reference: Some(charge.reference.to_string()),
            description: charge.description.to_string(),
        };
        match self.client().spend(midgar_id, &entry).await? {
            Spent::Done(moved) => Ok(credits(self.mirrored(midgar_id, app_id, moved).await.totals.balance)),
            Spent::Insufficient { balance } => Err(AppError::PaymentRequired(
                format!("Insufficient credits. Need {} credits, have {}", charge.amount, balance),
                Some(credits(balance)),
            )),
        }
    }

    /// Credits the wallet once per `grant.key`.
    pub async fn grant(&self, midgar_id: &str, app_id: &str, grant: &Grant<'_>) -> Result<GrantResult, AppError> {
        if grant.amount == 0 {
            return Ok(GrantResult { balance: self.balance(midgar_id, app_id).await?, replayed: false });
        }
        let entry = Entry {
            key: grant.key.clone(),
            tenant: app_id.to_string(),
            kind: grant.kind,
            amount: grant.amount as i64,
            source: grant.source.to_string(),
            reference: grant.reference.map(str::to_string),
            description: grant.description.to_string(),
        };
        let moved = self.mirrored(midgar_id, app_id, self.client().grant(midgar_id, &entry).await?).await;
        Ok(GrantResult { balance: credits(moved.totals.balance), replayed: moved.replayed })
    }

    /// Takes back at most `reversal.amount`, once per key, returning the balance after and what was taken.
    pub async fn reverse(&self, midgar_id: &str, app_id: &str, reversal: &Reversal<'_>) -> Result<(i32, u32), AppError> {
        if reversal.amount == 0 {
            return Ok((self.balance(midgar_id, app_id).await?, 0));
        }
        let entry = Entry {
            key: reversal.key.clone(),
            tenant: app_id.to_string(),
            kind: reversal.kind,
            amount: -(reversal.amount as i64),
            source: reversal.source.to_string(),
            reference: reversal.reference.map(str::to_string),
            description: reversal.description.to_string(),
        };
        let taken = self.client().reverse(midgar_id, &entry).await?;
        let moved = self.mirrored(midgar_id, app_id, taken.moved).await;
        Ok((credits(moved.totals.balance), taken.taken.max(0) as u32))
    }

    /// The spend stored under a charge key, for a refund to read the amount and time of the charge.
    pub async fn spend_entry(&self, midgar_id: &str, app_id: &str, key: &str) -> Result<Option<StoredEntry>, AppError> {
        Ok(self.client().entry(midgar_id, app_id, key).await?.filter(|e| e.kind == Kind::Spend))
    }

    /// The tenant's wallet entries as the transactions list shows them: newest first, `skip` entries dropped and at
    /// most `take` returned, plus how many entries the tenant holds in all.
    pub async fn history(&self, midgar_id: &str, app_id: &str, user_id: &str, skip: usize, take: usize) -> Result<(Vec<Value>, usize), AppError> {
        const PAGE: usize = 200;
        let mut collected: Vec<StoredEntry> = Vec::new();
        let mut page = self.client().entries(midgar_id, app_id, None, PAGE).await?;
        let total = page.total.max(0) as usize;
        loop {
            collected.extend(page.entries.into_iter().filter(|e| e.kind != Kind::Marker));
            match page.next_before {
                Some(before) if collected.len() < skip + take => page = self.client().entries(midgar_id, app_id, Some(before), PAGE).await?,
                _ => break,
            }
        }
        let rows = collected.iter().skip(skip).take(take).map(|e| transaction_row(e, user_id)).collect();
        Ok((rows, total))
    }
}

/// One wallet entry in the shape of a D1 `credit_transactions` row.
fn transaction_row(entry: &StoredEntry, user_id: &str) -> Value {
    let created_at = Utc.timestamp_opt(entry.at, 0).single().map(|t| t.to_rfc3339()).unwrap_or_default();
    serde_json::json!({
        "id": format!("wallet:{}", entry.seq),
        "user_id": user_id,
        "type": transaction_type(entry.kind),
        "amount": entry.amount,
        "balance_after": entry.balance_after,
        "description": entry.description,
        "reference_id": entry.reference,
        "created_at": created_at,
    })
}

/// A wallet operation set over the Durable Object namespace and D1.
pub type LiveOps<'a> = WalletOps<'a, DoWallets, worker::D1Database>;
