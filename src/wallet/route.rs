use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::books::Books;
use super::ledger::{self, validate_tenant};
use super::types::{Entry, Kind, Outcome, WalletError};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

/// The body of `POST /migrate`.
#[derive(Debug, Deserialize)]
struct MigrateRequest {
    tenant: String,
    key: String,
    moved: i64,
    welcome_part: i64,
}

/// The body of `POST /erase`.
#[derive(Debug, Deserialize)]
struct EraseRequest {
    tenant: String,
}

/// A status code with the JSON body to send.
pub type Reply = (u16, Value);

/// Handles one request to a wallet object. Pure and synchronous over `Books`, so the object calls
/// it with no `await` between the reads and writes and the tests call it on any storage.
pub fn handle(books: &mut impl Books, method: &str, path: &str, query: &str, body: &[u8], now: i64) -> Reply {
    let result = match (method, path) {
        ("POST", "/grant") => grant(books, body, now),
        ("POST", "/spend") => spend(books, body, now),
        ("POST", "/reverse") => reverse(books, body, now),
        ("POST", "/migrate") => migrate(books, body, now),
        ("POST", "/erase") => erase(books, body),
        ("GET", "/balance") => balance(books, query),
        ("GET", "/entries") => entries(books, query),
        ("GET", "/entry") => entry(books, query),
        ("GET", "/verify") => ledger::verify(books).and_then(to_reply),
        (_, "/grant" | "/spend" | "/reverse" | "/migrate" | "/erase" | "/balance" | "/entries" | "/entry" | "/verify") => {
            return (405, json!({ "error": "method_not_allowed" }));
        }
        _ => return (404, json!({ "error": "not_found" })),
    };
    result.unwrap_or_else(|e| (e.status(), json!({ "error": e.code(), "message": e.message() })))
}

/// Serialises a successful result with status 200.
fn to_reply(value: impl Serialize) -> Result<Reply, WalletError> {
    serde_json::to_value(value).map(|v| (200, v)).map_err(|e| WalletError::Storage(e.to_string()))
}

/// Adds the tenant's current `balance_now`, `purchased` and `spent` to a successful reply, read in the same
/// synchronous run, so a caller can mirror the totals, and report a repeat's current balance, without a second request.
fn with_totals(books: &impl Books, tenant: &str, reply: Reply) -> Result<Reply, WalletError> {
    let (status, mut body) = reply;
    if status == 200 {
        let totals = books.balance(tenant)?;
        if let Some(map) = body.as_object_mut() {
            map.insert("balance_now".into(), json!(totals.balance));
            map.insert("purchased".into(), json!(totals.purchased));
            map.insert("spent".into(), json!(totals.spent));
        }
    }
    Ok((status, body))
}

/// Parses a JSON request body.
fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, WalletError> {
    serde_json::from_slice(body).map_err(|e| WalletError::BadRequest(format!("invalid json: {e}")))
}

/// Reads one query parameter.
fn param(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

/// Reads the required, valid `tenant` query parameter.
fn tenant_param(query: &str) -> Result<String, WalletError> {
    let tenant = param(query, "tenant").unwrap_or_default();
    validate_tenant(&tenant)?;
    Ok(tenant)
}

/// Reads an optional non-negative integer query parameter.
fn number_param(query: &str, name: &str) -> Result<Option<i64>, WalletError> {
    match param(query, name) {
        None => Ok(None),
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .map(Some)
            .ok_or_else(|| WalletError::BadRequest(format!("{name} must be a non-negative integer"))),
    }
}

/// `POST /grant`: a positive entry of kind purchase, bonus, refund or adjust.
fn grant(books: &mut impl Books, body: &[u8], now: i64) -> Result<Reply, WalletError> {
    let entry: Entry = parse(body)?;
    if !matches!(entry.kind, Kind::Purchase | Kind::Bonus | Kind::Refund | Kind::Adjust) || entry.amount <= 0 {
        return Err(WalletError::BadRequest("grant needs kind purchase, bonus, refund or adjust and a positive amount".into()));
    }
    let reply = to_reply(ledger::apply(books, &entry, now)?)?;
    with_totals(books, &entry.tenant, reply)
}

/// `POST /spend`: a negative entry of kind spend; insufficient funds answer 402 and write nothing.
fn spend(books: &mut impl Books, body: &[u8], now: i64) -> Result<Reply, WalletError> {
    let entry: Entry = parse(body)?;
    if entry.kind != Kind::Spend || entry.amount >= 0 {
        return Err(WalletError::BadRequest("spend needs kind spend and a negative amount".into()));
    }
    match ledger::apply(books, &entry, now)? {
        Outcome::Insufficient { balance } => Ok((402, json!({ "error": "insufficient_funds", "balance": balance }))),
        outcome => with_totals(books, &entry.tenant, to_reply(outcome)?),
    }
}

/// `POST /reverse`: a clamped debit, answering `{taken, balance}`.
fn reverse(books: &mut impl Books, body: &[u8], now: i64) -> Result<Reply, WalletError> {
    let entry: Entry = parse(body)?;
    let reply = to_reply(ledger::reverse(books, &entry, now)?)?;
    with_totals(books, &entry.tenant, reply)
}

/// `POST /migrate`: moves a frozen D1 balance in, once per key.
fn migrate(books: &mut impl Books, body: &[u8], now: i64) -> Result<Reply, WalletError> {
    let req: MigrateRequest = parse(body)?;
    let reply = to_reply(ledger::migrate(books, &req.tenant, &req.key, req.moved, req.welcome_part, now)?)?;
    with_totals(books, &req.tenant, reply)
}

/// `POST /erase`: deletes one tenant's entries and balance.
fn erase(books: &mut impl Books, body: &[u8]) -> Result<Reply, WalletError> {
    let req: EraseRequest = parse(body)?;
    validate_tenant(&req.tenant)?;
    let erased = books.erase(&req.tenant)?;
    Ok((200, json!({ "erased": erased })))
}

/// `GET /balance?tenant=`.
fn balance(books: &mut impl Books, query: &str) -> Result<Reply, WalletError> {
    let tenant = tenant_param(query)?;
    to_reply(books.balance(&tenant)?)
}

/// `GET /entries?tenant=&before=&limit=`: newest first with a cursor for the next page.
fn entries(books: &mut impl Books, query: &str) -> Result<Reply, WalletError> {
    let tenant = tenant_param(query)?;
    let before = number_param(query, "before")?;
    let limit = number_param(query, "limit")?.map_or(DEFAULT_LIMIT, |n| (n as usize).clamp(1, MAX_LIMIT));
    let rows = books.entries(&tenant, before, limit)?;
    let next_before = if rows.len() == limit { rows.last().map(|r| r.seq) } else { None };
    Ok((200, json!({ "entries": rows, "next_before": next_before, "total": books.count(&tenant)? })))
}

/// `GET /entry?tenant=&key=`: the entry stored under an idempotency key, 404 when the tenant holds none.
fn entry(books: &mut impl Books, query: &str) -> Result<Reply, WalletError> {
    let tenant = tenant_param(query)?;
    let key = param(query, "key").unwrap_or_default();
    if key.is_empty() {
        return Err(WalletError::BadRequest("key is required".into()));
    }
    match books.find(&key)? {
        Some(stored) if stored.tenant == tenant => to_reply(stored),
        _ => Ok((404, json!({ "error": "not_found" }))),
    }
}
