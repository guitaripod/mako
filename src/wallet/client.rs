use std::time::Duration;

use futures::future::{select, Either};
use serde_json::{json, Value};
use worker::wasm_bindgen::JsValue;
use worker::{Delay, Headers, Method, ObjectNamespace, Request, RequestInit};

use super::types::{Entry, StoredEntry};
use crate::error::AppError;

/// How long a call to a wallet object may take before it counts as unavailable.
const CALL_TIMEOUT_MS: u64 = 5_000;

/// The host name wallet requests carry; the object only looks at the path.
const WALLET_ORIGIN: &str = "https://wallet.internal";

/// A way to reach the wallet object of one Midgar ID. Production sends the request to the Durable Object; the host
/// tests run the object's own request handler in memory.
#[allow(async_fn_in_trait)]
pub trait WalletTransport {
    /// Sends one request to the object named `midgar_id` and returns its status and JSON body. Any failure to
    /// obtain an answer is `AppError::WalletUnavailable`.
    async fn send(&self, midgar_id: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), AppError>;
}

/// The `WALLET` Durable Object namespace as a transport.
pub struct DoWallets(pub ObjectNamespace);

impl DoWallets {
    /// Builds the request for the object, `None` when it cannot be formed.
    fn request(method: &str, path: &str, body: Option<&Value>) -> Option<Request> {
        let headers = Headers::new();
        headers.set("Content-Type", "application/json").ok()?;
        let mut init = RequestInit::new();
        init.with_method(if method == "GET" { Method::Get } else { Method::Post }).with_headers(headers);
        if let Some(body) = body {
            init.with_body(Some(JsValue::from_str(&body.to_string())));
        }
        Request::new_with_init(&format!("{WALLET_ORIGIN}{path}"), &init).ok()
    }

    /// Fetches from the object and reads its JSON answer.
    async fn fetch(&self, midgar_id: &str, request: Request) -> Option<(u16, Value)> {
        let stub = self.0.get_by_name(midgar_id).ok()?;
        let mut response = stub.fetch_with_request(request).await.ok()?;
        let status = response.status_code();
        let body = response.json::<Value>().await.ok()?;
        Some((status, body))
    }
}

impl WalletTransport for DoWallets {
    async fn send(&self, midgar_id: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), AppError> {
        let request = Self::request(method, path, body).ok_or(AppError::WalletUnavailable)?;
        let call = Box::pin(self.fetch(midgar_id, request));
        let timeout = Box::pin(Delay::from(Duration::from_millis(CALL_TIMEOUT_MS)));
        match select(call, timeout).await {
            Either::Left((Some(answer), _)) => Ok(answer),
            _ => Err(AppError::WalletUnavailable),
        }
    }
}

/// The totals a wallet answer carries for the tenant after a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub balance: i64,
    pub purchased: i64,
    pub spent: i64,
}

/// A change the object applied or recognised as a repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moved {
    pub totals: Totals,
    pub replayed: bool,
}

/// The answer to a spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spent {
    Done(Moved),
    Insufficient { balance: i64 },
}

/// The answer to a clamped debit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Taken {
    pub taken: i64,
    pub moved: Moved,
}

/// The answer to moving a D1 balance in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    pub amount: i64,
    pub moved: Moved,
}

/// A page of entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPage {
    pub entries: Vec<StoredEntry>,
    pub next_before: Option<i64>,
    pub total: i64,
}

/// Typed calls to a wallet object over any transport.
pub struct Wallets<'a, W: WalletTransport> {
    pub transport: &'a W,
}

/// Reads an integer field of an answer.
fn field(body: &Value, name: &str) -> Option<i64> {
    body.get(name).and_then(Value::as_i64)
}

/// Reads the totals out of a successful answer. `balance_now` is the balance after the call, also for a repeat,
/// whose own `balance_after` is the balance at the time of the original entry.
fn moved(body: &Value) -> Result<Moved, AppError> {
    let replayed = body.get("replayed").and_then(Value::as_bool).unwrap_or(false) || body.get("outcome").and_then(Value::as_str) == Some("replayed");
    let balance = field(body, "balance_now")
        .or_else(|| field(body, "balance"))
        .or_else(|| field(body, "balance_after"))
        .ok_or(AppError::WalletUnavailable)?;
    let totals = Totals { balance, purchased: field(body, "purchased").unwrap_or(0), spent: field(body, "spent").unwrap_or(0) };
    Ok(Moved { totals, replayed })
}

/// Maps a status that is not a success to the error a caller should see. 5xx and anything unexpected is the
/// wallet being unavailable; a conflict is the caller reusing a key.
fn failure(status: u16, body: &Value) -> AppError {
    match status {
        409 => AppError::Conflict(body.get("message").and_then(Value::as_str).unwrap_or("idempotency_conflict").to_string()),
        400 => AppError::InternalError(format!("wallet rejected a request: {}", body.get("message").and_then(Value::as_str).unwrap_or(""))),
        _ => AppError::WalletUnavailable,
    }
}

impl<W: WalletTransport> Wallets<'_, W> {
    /// `GET /balance` for one tenant.
    pub async fn balance(&self, midgar_id: &str, tenant: &str) -> Result<Totals, AppError> {
        let path = format!("/balance?tenant={}", urlencoding::encode(tenant));
        let (status, body) = self.transport.send(midgar_id, "GET", &path, None).await?;
        if status != 200 {
            return Err(failure(status, &body));
        }
        let balance = field(&body, "balance").ok_or(AppError::WalletUnavailable)?;
        Ok(Totals { balance, purchased: field(&body, "purchased").unwrap_or(0), spent: field(&body, "spent").unwrap_or(0) })
    }

    /// `POST /grant`.
    pub async fn grant(&self, midgar_id: &str, entry: &Entry) -> Result<Moved, AppError> {
        let (status, body) = self.transport.send(midgar_id, "POST", "/grant", Some(&json!(entry))).await?;
        if status != 200 {
            return Err(failure(status, &body));
        }
        moved(&body)
    }

    /// `POST /spend`.
    pub async fn spend(&self, midgar_id: &str, entry: &Entry) -> Result<Spent, AppError> {
        let (status, body) = self.transport.send(midgar_id, "POST", "/spend", Some(&json!(entry))).await?;
        match status {
            200 => Ok(Spent::Done(moved(&body)?)),
            402 => Ok(Spent::Insufficient { balance: field(&body, "balance").unwrap_or(0) }),
            _ => Err(failure(status, &body)),
        }
    }

    /// `POST /reverse`: a debit of at most the requested amount.
    pub async fn reverse(&self, midgar_id: &str, entry: &Entry) -> Result<Taken, AppError> {
        let (status, body) = self.transport.send(midgar_id, "POST", "/reverse", Some(&json!(entry))).await?;
        if status != 200 {
            return Err(failure(status, &body));
        }
        let taken = field(&body, "taken").ok_or(AppError::WalletUnavailable)?;
        Ok(Taken { taken, moved: moved(&body)? })
    }

    /// `POST /migrate`.
    pub async fn migrate(&self, midgar_id: &str, tenant: &str, key: &str, moved_credits: i64, welcome_part: i64) -> Result<Migration, AppError> {
        let request = json!({ "tenant": tenant, "key": key, "moved": moved_credits, "welcome_part": welcome_part });
        let (status, body) = self.transport.send(midgar_id, "POST", "/migrate", Some(&request)).await?;
        if status != 200 {
            return Err(failure(status, &body));
        }
        let amount = field(&body, "amount").ok_or(AppError::WalletUnavailable)?;
        Ok(Migration { amount, moved: moved(&body)? })
    }

    /// `GET /entries`, newest first.
    pub async fn entries(&self, midgar_id: &str, tenant: &str, before: Option<i64>, limit: usize) -> Result<EntryPage, AppError> {
        let mut path = format!("/entries?tenant={}&limit={limit}", urlencoding::encode(tenant));
        if let Some(before) = before {
            path.push_str(&format!("&before={before}"));
        }
        let (status, body) = self.transport.send(midgar_id, "GET", &path, None).await?;
        if status != 200 {
            return Err(failure(status, &body));
        }
        let entries = serde_json::from_value(body.get("entries").cloned().unwrap_or(Value::Null)).map_err(|_| AppError::WalletUnavailable)?;
        Ok(EntryPage { entries, next_before: field(&body, "next_before"), total: field(&body, "total").unwrap_or(0) })
    }

    /// `GET /entry`: the entry under a key, `None` when the tenant has none.
    pub async fn entry(&self, midgar_id: &str, tenant: &str, key: &str) -> Result<Option<StoredEntry>, AppError> {
        let path = format!("/entry?tenant={}&key={}", urlencoding::encode(tenant), urlencoding::encode(key));
        let (status, body) = self.transport.send(midgar_id, "GET", &path, None).await?;
        match status {
            200 => serde_json::from_value(body).map(Some).map_err(|_| AppError::WalletUnavailable),
            404 => Ok(None),
            _ => Err(failure(status, &body)),
        }
    }

    /// `POST /erase` for one tenant, returning how many entries went.
    pub async fn erase(&self, midgar_id: &str, tenant: &str) -> Result<i64, AppError> {
        let (status, body) = self.transport.send(midgar_id, "POST", "/erase", Some(&json!({ "tenant": tenant }))).await?;
        if status != 200 {
            return Err(failure(status, &body));
        }
        Ok(field(&body, "erased").unwrap_or(0))
    }
}
