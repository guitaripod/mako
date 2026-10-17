use worker::*;

use super::route;
use super::sql::{self, SqlBooks, SqlExec, Val};
use super::types::WalletError;

/// The wallet of one Midgar ID: a SQLite-backed Durable Object holding the entries and balances
/// of every tenant for that person. It answers JSON over `fetch` on the paths in `route::handle`.
#[durable_object]
pub struct Wallet {
    state: State,
    ready: std::result::Result<(), WalletError>,
}

/// The object's SQLite storage as a `SqlExec`.
struct ObjectSql(worker::SqlStorage);

impl SqlExec for ObjectSql {
    fn run(&self, query: &str, params: &[Val]) -> std::result::Result<Vec<Vec<Val>>, WalletError> {
        let bindings = params
            .iter()
            .map(|v| match v {
                Val::Null => SqlStorageValue::Null,
                Val::Int(n) => SqlStorageValue::Integer(*n),
                Val::Text(s) => SqlStorageValue::String(s.clone()),
            })
            .collect::<Vec<_>>();
        let storage = |e: Error| WalletError::Storage(e.to_string());
        let cursor = self.0.exec(query, bindings).map_err(storage)?;
        cursor
            .raw()
            .map(|row| {
                let row = row.map_err(storage)?;
                row.into_iter().map(to_val).collect()
            })
            .collect()
    }
}

/// Converts one stored column to a `Val`, refusing types the schema never produces.
fn to_val(value: SqlStorageValue) -> std::result::Result<Val, WalletError> {
    match value {
        SqlStorageValue::Null => Ok(Val::Null),
        SqlStorageValue::Integer(n) => Ok(Val::Int(n)),
        SqlStorageValue::String(s) => Ok(Val::Text(s)),
        other => Err(WalletError::Storage(format!("unexpected column value {other:?}"))),
    }
}

impl DurableObject for Wallet {
    fn new(state: State, _env: Env) -> Self {
        let ready = sql::init(&ObjectSql(state.storage().sql()));
        Self { state, ready }
    }

    /// Reads the body, then runs the whole request synchronously: no `await` follows, so the
    /// reads and writes of one request cannot interleave with another request's.
    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let method = req.method().to_string();
        let url = req.url()?;
        let body = req.bytes().await?;
        let (status, json) = match &self.ready {
            Err(e) => (500, serde_json::json!({ "error": e.code(), "message": e.message() })),
            Ok(()) => {
                let mut books = SqlBooks::new(ObjectSql(self.state.storage().sql()));
                let now = (Date::now().as_millis() / 1000) as i64;
                route::handle(&mut books, &method, url.path(), url.query().unwrap_or(""), &body, now)
            }
        };
        Ok(Response::from_json(&json)?.with_status(status))
    }
}
