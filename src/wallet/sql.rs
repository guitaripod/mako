use super::books::Books;
use super::schema;
use super::types::{Balance, Entry, Kind, StoredEntry, WalletError};

/// The largest integer JavaScript represents exactly, used as "no upper bound" for cursors.
const MAX_SEQ: i64 = 9_007_199_254_740_991;

const ENTRY_COLUMNS: &str = "seq, idem_key, tenant, kind, amount, balance_after, source, reference, description, at";

/// A value crossing the SQL boundary; the wallet only needs these three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Val {
    Null,
    Int(i64),
    Text(String),
}

impl From<i64> for Val {
    fn from(value: i64) -> Self {
        Val::Int(value)
    }
}

impl From<String> for Val {
    fn from(value: String) -> Self {
        Val::Text(value)
    }
}

impl From<&str> for Val {
    fn from(value: &str) -> Self {
        Val::Text(value.to_string())
    }
}

impl From<&Option<String>> for Val {
    fn from(value: &Option<String>) -> Self {
        value.as_deref().map_or(Val::Null, Val::from)
    }
}

/// Runs one SQL statement synchronously and returns every row. The object implements it over its
/// SQLite storage and the host tests over `rusqlite`, so both execute the same queries.
pub trait SqlExec {
    fn run(&self, sql: &str, params: &[Val]) -> Result<Vec<Vec<Val>>, WalletError>;
}

/// Creates the tables when missing; safe to call on every start.
pub fn init(sql: &impl SqlExec) -> Result<(), WalletError> {
    for statement in schema::STATEMENTS {
        sql.run(statement, &[])?;
    }
    Ok(())
}

/// The wallet books over a SQLite database reached through `SqlExec`.
pub struct SqlBooks<S: SqlExec> {
    sql: S,
}

impl<S: SqlExec> SqlBooks<S> {
    /// Wraps a SQL executor whose database already went through `init`.
    pub fn new(sql: S) -> Self {
        Self { sql }
    }

    /// The executor under the books, for tests that need to reach the database directly.
    #[cfg(test)]
    pub fn executor(&self) -> &S {
        &self.sql
    }

    /// Runs one statement on the executor.
    fn query(&self, sql: &str, params: &[Val]) -> Result<Vec<Vec<Val>>, WalletError> {
        self.sql.run(sql, params)
    }
}

/// Reads column `i` as an integer.
fn int(row: &[Val], i: usize) -> Result<i64, WalletError> {
    match row.get(i) {
        Some(Val::Int(v)) => Ok(*v),
        other => Err(WalletError::Storage(format!("column {i} is not an integer: {other:?}"))),
    }
}

/// Reads column `i` as text.
fn text(row: &[Val], i: usize) -> Result<String, WalletError> {
    match row.get(i) {
        Some(Val::Text(v)) => Ok(v.clone()),
        other => Err(WalletError::Storage(format!("column {i} is not text: {other:?}"))),
    }
}

/// Reads column `i` as nullable text.
fn opt_text(row: &[Val], i: usize) -> Result<Option<String>, WalletError> {
    match row.get(i) {
        Some(Val::Null) => Ok(None),
        Some(Val::Text(v)) => Ok(Some(v.clone())),
        other => Err(WalletError::Storage(format!("column {i} is not nullable text: {other:?}"))),
    }
}

/// Builds an entry from a row selected with `ENTRY_COLUMNS`.
fn stored(row: &[Val]) -> Result<StoredEntry, WalletError> {
    let kind = text(row, 3)?;
    Ok(StoredEntry {
        seq: int(row, 0)?,
        key: text(row, 1)?,
        tenant: text(row, 2)?,
        kind: Kind::parse(&kind).ok_or_else(|| WalletError::Storage(format!("unknown kind {kind}")))?,
        amount: int(row, 4)?,
        balance_after: int(row, 5)?,
        source: text(row, 6)?,
        reference: opt_text(row, 7)?,
        description: text(row, 8)?,
        at: int(row, 9)?,
    })
}

impl<S: SqlExec> Books for SqlBooks<S> {
    fn find(&self, key: &str) -> Result<Option<StoredEntry>, WalletError> {
        let rows = self.query(&format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE idem_key = ?"), &[key.into()])?;
        rows.first().map(|row| stored(row)).transpose()
    }

    fn balance(&self, tenant: &str) -> Result<Balance, WalletError> {
        let rows = self.query("SELECT balance, purchased, spent FROM balances WHERE tenant = ?", &[tenant.into()])?;
        match rows.first() {
            None => Ok(Balance::default()),
            Some(row) => Ok(Balance { balance: int(row, 0)?, purchased: int(row, 1)?, spent: int(row, 2)? }),
        }
    }

    fn append(&mut self, entry: &Entry, after: &Balance, now: i64) -> Result<i64, WalletError> {
        let rows = self.query(
            "INSERT INTO entries (idem_key, tenant, kind, amount, balance_after, source, reference, description, at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING seq",
            &[
                entry.key.as_str().into(),
                entry.tenant.as_str().into(),
                entry.kind.as_str().into(),
                entry.amount.into(),
                after.balance.into(),
                entry.source.as_str().into(),
                (&entry.reference).into(),
                entry.description.as_str().into(),
                now.into(),
            ],
        )?;
        self.query(
            "INSERT INTO balances (tenant, balance, purchased, spent, updated_at) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(tenant) DO UPDATE SET balance = excluded.balance, purchased = excluded.purchased,
             spent = excluded.spent, updated_at = excluded.updated_at",
            &[entry.tenant.as_str().into(), after.balance.into(), after.purchased.into(), after.spent.into(), now.into()],
        )?;
        rows.first().map(|row| int(row, 0)).unwrap_or_else(|| Err(WalletError::Storage("insert returned no seq".into())))
    }

    fn entries(&self, tenant: &str, before: Option<i64>, limit: usize) -> Result<Vec<StoredEntry>, WalletError> {
        let rows = self.query(
            &format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE tenant = ? AND seq < ? ORDER BY seq DESC LIMIT ?"),
            &[tenant.into(), before.unwrap_or(MAX_SEQ).into(), (limit as i64).into()],
        )?;
        rows.iter().map(|row| stored(row)).collect()
    }

    fn count(&self, tenant: &str) -> Result<usize, WalletError> {
        let rows = self.query("SELECT COUNT(*) FROM entries WHERE tenant = ? AND kind != 'marker'", &[tenant.into()])?;
        rows.first().map(|row| int(row, 0).map(|n| n as usize)).transpose().map(|n| n.unwrap_or(0))
    }

    fn chronological(&self, tenant: &str) -> Result<Vec<StoredEntry>, WalletError> {
        let rows = self.query(&format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE tenant = ? ORDER BY seq"), &[tenant.into()])?;
        rows.iter().map(|row| stored(row)).collect()
    }

    fn tenants(&self) -> Result<Vec<String>, WalletError> {
        let rows = self.query("SELECT tenant FROM balances UNION SELECT tenant FROM entries ORDER BY 1", &[])?;
        rows.iter().map(|row| text(row, 0)).collect()
    }

    fn erase(&mut self, tenant: &str) -> Result<usize, WalletError> {
        let count = self.query("SELECT COUNT(*) FROM entries WHERE tenant = ?", &[tenant.into()])?;
        let removed = count.first().map(|row| int(row, 0)).transpose()?.unwrap_or(0);
        self.query("DELETE FROM entries WHERE tenant = ?", &[tenant.into()])?;
        self.query("DELETE FROM balances WHERE tenant = ?", &[tenant.into()])?;
        Ok(removed as usize)
    }

    fn meta(&self, key: &str) -> Result<Option<String>, WalletError> {
        let rows = self.query("SELECT value FROM meta WHERE key = ?", &[key.into()])?;
        rows.first().map(|row| text(row, 0)).transpose()
    }

    fn set_meta(&mut self, key: &str, value: &str) -> Result<(), WalletError> {
        self.query(
            "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            &[key.into(), value.into()],
        )?;
        Ok(())
    }

    fn delete_meta(&mut self, key: &str) -> Result<(), WalletError> {
        self.query("DELETE FROM meta WHERE key = ?", &[key.into()])?;
        Ok(())
    }

    fn meta_keys(&self, prefix: &str) -> Result<Vec<String>, WalletError> {
        let rows = self.query("SELECT key FROM meta WHERE substr(key, 1, length(?)) = ? ORDER BY key", &[prefix.into(), prefix.into()])?;
        rows.iter().map(|row| text(row, 0)).collect()
    }
}
