use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::{pin, Pin};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::task::noop_waker_ref;
use rusqlite::backup::Backup;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{params_from_iter, Connection};
use serde_json::{Map, Value};

use super::client::WalletTransport;
use super::d1::{D1Exec, Stmt};
use super::link::account_of;
use super::ops::{Reversal, WalletOps};
use super::pack_credit::PurchaseLedger;
use super::{links, purchase};
use super::route::handle;
use super::sql::{self, SqlBooks, SqlExec, Val};
use super::types::WalletError;
use crate::error::AppError;
use crate::ledgers::Account;

/// The clock every host test uses for the wallet objects.
pub const NOW: i64 = 1_760_000_000;

fn bound(params: &[Val]) -> Vec<SqlValue> {
    params
        .iter()
        .map(|v| match v {
            Val::Null => SqlValue::Null,
            Val::Int(n) => SqlValue::Integer(*n),
            Val::Text(s) => SqlValue::Text(s.clone()),
        })
        .collect()
}

/// `SqlExec` over an in-memory `rusqlite` connection, running the object's real queries.
pub struct Rusqlite(pub Connection);

impl SqlExec for Rusqlite {
    fn run(&self, sql: &str, params: &[Val]) -> Result<Vec<Vec<Val>>, WalletError> {
        let storage = |e: rusqlite::Error| WalletError::Storage(e.to_string());
        let mut statement = self.0.prepare_cached(sql).map_err(storage)?;
        let columns = statement.column_count();
        let mut rows = statement.query(params_from_iter(bound(params))).map_err(storage)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(storage)? {
            let mut values = Vec::new();
            for i in 0..columns {
                values.push(match row.get_ref(i).map_err(storage)? {
                    ValueRef::Null => Val::Null,
                    ValueRef::Integer(n) => Val::Int(n),
                    ValueRef::Text(t) => Val::Text(String::from_utf8_lossy(t).into_owned()),
                    other => return Err(WalletError::Storage(format!("unexpected {other:?}"))),
                });
            }
            out.push(values);
        }
        Ok(out)
    }
}

thread_local! {
    static MIGRATED: Connection = replay_migrations();
}

/// Every migration of the service, replayed in order on a fresh in-memory database.
fn replay_migrations() -> Connection {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".sql"))
        .collect();
    names.sort();
    let db = Connection::open_in_memory().unwrap();
    for name in names {
        let sql = std::fs::read_to_string(dir.join(&name)).unwrap();
        db.execute_batch(&sql).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    db
}

/// A D1 stand-in: SQLite with every migration of the service applied, running the exact statements production sends.
pub struct LocalD1(pub Connection);

impl LocalD1 {
    /// A fresh database with all migrations applied, copied from one replay per thread so a test that builds thousands
    /// of worlds does not replay thirty files each time.
    pub fn migrated() -> Self {
        let copy = MIGRATED.with(|template| {
            let mut copy = Connection::open_in_memory().unwrap();
            Backup::new(template, &mut copy).unwrap().run_to_completion(i32::MAX, Duration::ZERO, None).unwrap();
            copy
        });
        copy.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        Self(copy)
    }

    /// Runs a statement the test itself needs, panicking on failure.
    pub fn run(&self, sql: &str, params: &[Val]) {
        self.0.execute(sql, params_from_iter(bound(params))).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// One integer the query returns, for assertions.
    pub fn int(&self, sql: &str, params: &[Val]) -> i64 {
        self.0.query_row(sql, params_from_iter(bound(params)), |r| r.get(0)).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    /// Creates a user and its D1 balance row, as registration does.
    pub fn user(&self, app_id: &str, user_id: &str, balance: i64) {
        let now = "2026-10-01T00:00:00+00:00";
        self.run(
            "INSERT INTO users (id, provider, provider_id, name, created_at, updated_at, app_id) VALUES (?1, 'anonymous', ?1, 'T', ?2, ?2, ?3)",
            &[user_id.into(), now.into(), app_id.into()],
        );
        self.run(
            "INSERT INTO user_credits (app_id, user_id, balance, lifetime_purchased, lifetime_spent, created_at, updated_at) VALUES (?1, ?2, ?3, 0, 0, ?4, ?4)",
            &[app_id.into(), user_id.into(), balance.into(), now.into()],
        );
    }

    /// Records a ledger row in D1, such as the welcome bonus.
    pub fn transaction(&self, app_id: &str, user_id: &str, kind: &str, amount: i64, description: &str) {
        self.run(
            "INSERT INTO credit_transactions (id, app_id, user_id, type, amount, balance_after, description, created_at)
             VALUES (lower(hex(randomblob(8))), ?1, ?2, ?3, ?4, 0, ?5, '2026-10-01T00:00:00+00:00')",
            &[app_id.into(), user_id.into(), kind.into(), amount.into(), description.into()],
        );
    }

    pub fn d1_balance(&self, app_id: &str, user_id: &str) -> i64 {
        self.int("SELECT balance FROM user_credits WHERE app_id = ?1 AND user_id = ?2", &[app_id.into(), user_id.into()])
    }
}

fn failure(e: rusqlite::Error) -> AppError {
    AppError::InternalError(format!("sqlite: {e}"))
}

impl D1Exec for LocalD1 {
    async fn rows(&self, sql: &str, params: &[Val]) -> Result<Vec<Value>, AppError> {
        let mut statement = self.0.prepare_cached(sql).map_err(failure)?;
        let names: Vec<String> = statement.column_names().iter().map(|n| n.to_string()).collect();
        let mut rows = statement.query(params_from_iter(bound(params))).map_err(failure)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(failure)? {
            let mut object = Map::new();
            for (i, name) in names.iter().enumerate() {
                let value = match row.get_ref(i).map_err(failure)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => Value::from(n),
                    ValueRef::Real(f) => Value::from(f),
                    ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
                    ValueRef::Blob(_) => Value::Null,
                };
                object.insert(name.clone(), value);
            }
            out.push(Value::Object(object));
        }
        Ok(out)
    }

    async fn exec(&self, sql: &str, params: &[Val]) -> Result<u64, AppError> {
        let mut statement = self.0.prepare_cached(sql).map_err(failure)?;
        statement.execute(params_from_iter(bound(params))).map(|n| n as u64).map_err(failure)
    }

    async fn batch(&self, statements: &[Stmt]) -> Result<(), AppError> {
        let transaction = self.0.unchecked_transaction().map_err(failure)?;
        for statement in statements {
            self.0.execute(&statement.sql, params_from_iter(bound(&statement.params))).map_err(failure)?;
        }
        transaction.commit().map_err(failure)
    }
}

/// The wallet objects of several Midgar IDs, each on a real SQLite schema, answering through the object's own
/// request handler.
#[derive(Default)]
pub struct FakeWallets {
    books: RefCell<HashMap<String, SqlBooks<Rusqlite>>>,
    pub calls: RefCell<Vec<String>>,
}

impl FakeWallets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends a request and returns status and body, like the transport does.
    pub fn call(&self, midgar_id: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let mut books = self.books.borrow_mut();
        let object = books.entry(midgar_id.to_string()).or_insert_with(|| {
            let sql = Rusqlite(Connection::open_in_memory().unwrap());
            sql::init(&sql).unwrap();
            SqlBooks::new(sql)
        });
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let bytes = body.map(|b| b.to_string()).unwrap_or_default();
        self.calls.borrow_mut().push(format!("{method} {route}"));
        handle(object, method, route, query, bytes.as_bytes(), NOW)
    }

    /// The balance of a tenant in a Midgar ID's wallet, zero for a wallet never used.
    pub fn balance(&self, midgar_id: &str, tenant: &str) -> i64 {
        self.call(midgar_id, "GET", &format!("/balance?tenant={tenant}"), None).1["balance"].as_i64().unwrap_or(0)
    }

    /// Whether a wallet's own ledger recomputes to its stored totals.
    pub fn verified(&self, midgar_id: &str) -> bool {
        self.call(midgar_id, "GET", "/verify", None).1["ok"].as_bool().unwrap_or(false)
    }

    /// How many requests of a kind (`"POST /migrate"`) have reached any wallet.
    pub fn count(&self, what: &str) -> usize {
        self.calls.borrow().iter().filter(|c| c.as_str() == what).count()
    }
}

impl WalletTransport for FakeWallets {
    async fn send(&self, midgar_id: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), AppError> {
        Ok(self.call(midgar_id, method, path, body))
    }
}

/// A budget of operations that succeed before every later write fails: the crash injector. Each write counts twice, once
/// before it reaches the store and once after, so a budget walks the crash point through "never ran" and "ran but
/// its answer was lost" for every write of a flow.
#[derive(Default)]
pub struct Budget(Cell<Option<usize>>);

impl Budget {
    pub fn unlimited() -> Self {
        Self(Cell::new(None))
    }

    pub fn of(ticks: usize) -> Self {
        Self(Cell::new(Some(ticks)))
    }

    /// Spends one tick, or fails when none is left.
    fn tick(&self) -> Result<(), AppError> {
        match self.0.get() {
            None => Ok(()),
            Some(0) => Err(AppError::WalletUnavailable),
            Some(n) => {
                self.0.set(Some(n - 1));
                Ok(())
            }
        }
    }

    /// Lifts the budget, as a retry after the crash would run unhindered.
    pub fn lift(&self) {
        self.0.set(None);
    }

    /// True once the budget ran out at least once, that is when a crash was injected.
    pub fn exhausted(&self) -> bool {
        self.0.get() == Some(0)
    }
}

/// A store whose writes fail once the shared budget is spent.
pub struct Flaky<'a, T> {
    pub inner: &'a T,
    pub budget: &'a Budget,
}

impl D1Exec for Flaky<'_, LocalD1> {
    async fn rows(&self, sql: &str, params: &[Val]) -> Result<Vec<Value>, AppError> {
        self.inner.rows(sql, params).await
    }

    async fn exec(&self, sql: &str, params: &[Val]) -> Result<u64, AppError> {
        self.budget.tick()?;
        let changed = self.inner.exec(sql, params).await?;
        self.budget.tick()?;
        Ok(changed)
    }

    async fn batch(&self, statements: &[Stmt]) -> Result<(), AppError> {
        self.budget.tick()?;
        self.inner.batch(statements).await?;
        self.budget.tick()
    }
}

impl WalletTransport for Flaky<'_, FakeWallets> {
    async fn send(&self, midgar_id: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), AppError> {
        if method == "GET" {
            return self.inner.send(midgar_id, method, path, body).await;
        }
        self.budget.tick()?;
        let answer = self.inner.send(midgar_id, method, path, body).await?;
        self.budget.tick()?;
        Ok(answer)
    }
}

/// A future that is pending the first time it is polled, so a flow parks right before the operation that follows it.
struct Park(bool);

impl Future for Park {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// True for a statement that reads or writes the state a credit and a refund of one transaction race on.
fn contested(sql: &str) -> bool {
    sql.contains("credit_purchases") || sql.contains("purchase_refunds")
}

/// A store that parks its flow before every contested D1 statement and every wallet call, so `race` can run another
/// flow between any two of them. Each parked operation then runs as one atomic step, which is how D1 applies a statement
/// and how a wallet object applies a request.
pub struct Stepped<'a, T> {
    pub inner: &'a T,
}

impl<T: D1Exec> D1Exec for Stepped<'_, T> {
    async fn rows(&self, sql: &str, params: &[Val]) -> Result<Vec<Value>, AppError> {
        if contested(sql) {
            Park(false).await;
        }
        self.inner.rows(sql, params).await
    }

    async fn exec(&self, sql: &str, params: &[Val]) -> Result<u64, AppError> {
        if contested(sql) {
            Park(false).await;
        }
        self.inner.exec(sql, params).await
    }

    async fn batch(&self, statements: &[Stmt]) -> Result<(), AppError> {
        if statements.iter().any(|s| contested(&s.sql)) {
            Park(false).await;
        }
        self.inner.batch(statements).await
    }
}

impl<T: WalletTransport> WalletTransport for Stepped<'_, T> {
    async fn send(&self, midgar_id: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), AppError> {
        Park(false).await;
        self.inner.send(midgar_id, method, path, body).await
    }
}

/// Which flow ran at one point where both were parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    pub chosen: usize,
}

/// Runs two flows to the end on one thread, taking turns at their parking points. Both first run up to their first
/// contested operation, which is not yet a choice. At each point where both are parked
/// the next entry of `prefix` says which one runs (0 or 1), and the first flow once the prefix is used up. Returns both
/// results and the choices made, so `interleavings` can branch on every one of them.
pub fn race<A, B>(first: impl Future<Output = A>, second: impl Future<Output = B>, prefix: &[usize]) -> (A, B, Vec<Pick>) {
    let mut first = pin!(first);
    let mut second = pin!(second);
    let mut cx = Context::from_waker(noop_waker_ref());
    let mut done_first = match first.as_mut().poll(&mut cx) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    };
    let mut done_second = match second.as_mut().poll(&mut cx) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    };
    let mut picks = Vec::new();
    loop {
        let chosen = match (done_first.is_some(), done_second.is_some()) {
            (false, false) => {
                let chosen = prefix.get(picks.len()).copied().unwrap_or(0);
                picks.push(Pick { chosen });
                chosen
            }
            (false, true) => 0,
            (true, false) => 1,
            (true, true) => break,
        };
        if chosen == 0 {
            if let Poll::Ready(value) = first.as_mut().poll(&mut cx) {
                done_first = Some(value);
            }
        } else if let Poll::Ready(value) = second.as_mut().poll(&mut cx) {
            done_second = Some(value);
        }
    }
    (done_first.unwrap(), done_second.unwrap(), picks)
}

/// How many times the running flow changes along a schedule that follows `choices` and then runs the first flow to the
/// end: a change back to the first flow after the last choice counts.
fn switches(choices: &[usize]) -> usize {
    let changes = choices.windows(2).filter(|pair| pair[0] != pair[1]).count();
    let back = usize::from(choices.last() == Some(&1));
    changes + back
}

/// Visits every interleaving of two flows exactly once: `run` receives the choices to follow, runs a fresh world through
/// `race` with them and checks it, and returns the choices `race` made. The next runs branch at every point where the
/// default (the first flow) was taken. With `max_switches` set, schedules that change the running flow more often are
/// left out, which keeps a search that also walks a crash through every write affordable. Returns how many
/// interleavings were run.
pub fn interleavings(max_switches: Option<usize>, mut run: impl FnMut(&[usize]) -> Vec<Pick>) -> usize {
    let mut pending: Vec<Vec<usize>> = vec![Vec::new()];
    let mut visited = 0;
    while let Some(prefix) = pending.pop() {
        let picks = run(&prefix);
        visited += 1;
        for at in prefix.len()..picks.len() {
            if picks[at].chosen == 0 {
                let mut branch: Vec<usize> = picks[..at].iter().map(|p| p.chosen).collect();
                branch.push(1);
                if max_switches.is_none_or(|bound| switches(&branch) <= bound) {
                    pending.push(branch);
                }
            }
        }
    }
    visited
}

/// The `PurchaseLedger` of a world with linked users, over D1 and the wallets alone: what `Ledgers` does for a wallet
/// account, without the environment it needs.
pub struct WalletLedger<'a, D: D1Exec, W: WalletTransport> {
    pub db: &'a D,
    pub wallets: &'a W,
}

impl<D: D1Exec, W: WalletTransport> PurchaseLedger for WalletLedger<'_, D, W> {
    async fn complete(&self, purchase_id: &str) -> Result<(), AppError> {
        let row = purchase::read(self.db, purchase_id).await?.ok_or_else(|| AppError::NotFound("Purchase not found".to_string()))?;
        let account = self.account_for(&row.app_id, &row.user_id).await?;
        purchase::complete_in_wallet(self.db, self.wallets, &account, &row).await?;
        Ok(())
    }

    async fn account_for(&self, app_id: &str, user_id: &str) -> Result<Account, AppError> {
        Ok(match links::read(self.db, app_id, user_id).await? {
            Some(link) => account_of(&link),
            None => Account::local(app_id, user_id),
        })
    }

    async fn reverse(&self, account: &Account, reversal: &Reversal<'_>) -> Result<(i32, u32), AppError> {
        let Account::Wallet { midgar_id, app_id, .. } = account else {
            return Err(AppError::InternalError("the test ledger reverses wallet accounts only".to_string()));
        };
        WalletOps { wallets: self.wallets, db: self.db }.reverse(midgar_id, app_id, reversal).await
    }
}
