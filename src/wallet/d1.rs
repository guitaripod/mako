use serde_json::Value;
use worker::wasm_bindgen::JsValue;
use worker::D1Database;

use super::sql::Val;
use crate::error::AppError;

/// One statement of a batch with its bound values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    pub sql: String,
    pub params: Vec<Val>,
}

impl Stmt {
    pub fn new(sql: &str, params: Vec<Val>) -> Self {
        Self { sql: sql.to_string(), params }
    }
}

/// The D1 operations the wallet code needs. Production runs them on `D1Database`; the host tests run the very same
/// statements on SQLite, so the SQL of the saga is exercised against the real migrations.
#[allow(async_fn_in_trait)]
pub trait D1Exec {
    /// Every row of a query as a JSON object keyed by column name.
    async fn rows(&self, sql: &str, params: &[Val]) -> Result<Vec<Value>, AppError>;

    /// Runs a statement and returns how many rows it changed.
    async fn exec(&self, sql: &str, params: &[Val]) -> Result<u64, AppError>;

    /// Runs statements in order as one transaction: all of them apply or none does.
    async fn batch(&self, statements: &[Stmt]) -> Result<(), AppError>;

    /// Like `batch`, and returns how many rows each statement changed, in order. A statement can only be made
    /// conditional on an earlier one by what the earlier one wrote, so the count of the statement that decides is how a
    /// caller learns whether the whole batch took effect.
    async fn batch_changes(&self, statements: &[Stmt]) -> Result<Vec<u64>, AppError>;
}

/// Converts bound values to what D1 binds.
fn bindings(params: &[Val]) -> Vec<JsValue> {
    params
        .iter()
        .map(|v| match v {
            Val::Null => JsValue::NULL,
            Val::Int(n) => JsValue::from_f64(*n as f64),
            Val::Text(s) => JsValue::from_str(s),
        })
        .collect()
}

impl D1Exec for D1Database {
    async fn rows(&self, sql: &str, params: &[Val]) -> Result<Vec<Value>, AppError> {
        let result = self.prepare(sql).bind(&bindings(params))?.all().await?;
        Ok(result.results::<Value>()?)
    }

    async fn exec(&self, sql: &str, params: &[Val]) -> Result<u64, AppError> {
        let result = self.prepare(sql).bind(&bindings(params))?.run().await?;
        Ok(result.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0) as u64)
    }

    async fn batch(&self, statements: &[Stmt]) -> Result<(), AppError> {
        let prepared = statements
            .iter()
            .map(|s| self.prepare(&s.sql).bind(&bindings(&s.params)))
            .collect::<Result<Vec<_>, _>>()?;
        self.batch(prepared).await?;
        Ok(())
    }

    async fn batch_changes(&self, statements: &[Stmt]) -> Result<Vec<u64>, AppError> {
        let prepared = statements
            .iter()
            .map(|s| self.prepare(&s.sql).bind(&bindings(&s.params)))
            .collect::<Result<Vec<_>, _>>()?;
        let results = self.batch(prepared).await?;
        Ok(results.iter().map(|r| r.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0) as u64).collect())
    }
}

/// An integer column of a row.
pub fn int(row: &Value, column: &str) -> Option<i64> {
    row.get(column).and_then(Value::as_i64)
}

/// A text column of a row.
pub fn text<'a>(row: &'a Value, column: &str) -> Option<&'a str> {
    row.get(column).and_then(Value::as_str)
}
