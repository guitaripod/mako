use std::collections::BTreeMap;

use super::books::Books;
use super::types::{Balance, Entry, Kind, StoredEntry, WalletError};

/// The wallet books held in memory, for tests that need no SQLite.
#[derive(Debug, Default)]
pub struct MemoryBooks {
    rows: Vec<StoredEntry>,
    balances: BTreeMap<String, Balance>,
    next_seq: i64,
}

impl Books for MemoryBooks {
    fn find(&self, key: &str) -> Result<Option<StoredEntry>, WalletError> {
        Ok(self.rows.iter().find(|r| r.key == key).cloned())
    }

    fn balance(&self, tenant: &str) -> Result<Balance, WalletError> {
        Ok(self.balances.get(tenant).copied().unwrap_or_default())
    }

    fn append(&mut self, entry: &Entry, after: &Balance, now: i64) -> Result<i64, WalletError> {
        self.next_seq += 1;
        self.rows.push(StoredEntry {
            seq: self.next_seq,
            key: entry.key.clone(),
            tenant: entry.tenant.clone(),
            kind: entry.kind,
            amount: entry.amount,
            balance_after: after.balance,
            source: entry.source.clone(),
            reference: entry.reference.clone(),
            description: entry.description.clone(),
            at: now,
        });
        self.balances.insert(entry.tenant.clone(), *after);
        Ok(self.next_seq)
    }

    fn entries(&self, tenant: &str, before: Option<i64>, limit: usize) -> Result<Vec<StoredEntry>, WalletError> {
        let upper = before.unwrap_or(i64::MAX);
        Ok(self.rows.iter().rev().filter(|r| r.tenant == tenant && r.seq < upper).take(limit).cloned().collect())
    }

    fn count(&self, tenant: &str) -> Result<usize, WalletError> {
        Ok(self.rows.iter().filter(|r| r.tenant == tenant && r.kind != Kind::Marker).count())
    }

    fn chronological(&self, tenant: &str) -> Result<Vec<StoredEntry>, WalletError> {
        Ok(self.rows.iter().filter(|r| r.tenant == tenant).cloned().collect())
    }

    fn tenants(&self) -> Result<Vec<String>, WalletError> {
        let mut all: Vec<String> = self.balances.keys().cloned().collect();
        all.extend(self.rows.iter().map(|r| r.tenant.clone()));
        all.sort();
        all.dedup();
        Ok(all)
    }

    fn erase(&mut self, tenant: &str) -> Result<usize, WalletError> {
        let before = self.rows.len();
        self.rows.retain(|r| r.tenant != tenant);
        self.balances.remove(tenant);
        Ok(before - self.rows.len())
    }
}

impl MemoryBooks {
    /// Overwrites a tenant's stored totals, to simulate corruption for `verify`.
    pub fn corrupt_balance(&mut self, tenant: &str, balance: Balance) {
        self.balances.insert(tenant.to_string(), balance);
    }
}
