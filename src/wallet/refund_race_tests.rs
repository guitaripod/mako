use futures::executor::block_on;
use serde_json::json;

use super::pack_credit::{credit_pack, refund_pack};
use super::pack_refund;
use super::purchase;
use super::testing::{race, interleavings, Budget, FakeWallets, Flaky, LocalD1, Pick, Stepped, WalletLedger};
use super::{ledger_grant, ledger_refund};
use crate::error::AppError;
use crate::storekit::{already_settled, LedgerTransaction};

const APP: &str = "dreameater";
const BUNDLE: &str = "com.marcusziade.DreamEater";
const PRODUCT: &str = "com.dreameater.credits.small";
const PACK_CREDITS: i64 = 5;
const MIDGAR: &str = "0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d47";
const TX: &str = "2000000123";
const OTHER_TX: &str = "2000000124";

/// The ways a store transaction gets credited.
#[derive(Debug, Clone, Copy)]
enum CreditPath {
    LedgerGrant,
    SignedValidate,
    RevenueCatPurchase,
}

/// The ways a store transaction gets refunded.
#[derive(Debug, Clone, Copy)]
enum RefundPath {
    Ledger,
    RevenueCat,
}

/// What the world holds before the two paths run.
#[derive(Debug, Clone, Copy)]
enum Start {
    Nothing,
    PendingRow,
}

const PAIRS: [(CreditPath, RefundPath); 4] = [
    (CreditPath::LedgerGrant, RefundPath::Ledger),
    (CreditPath::SignedValidate, RefundPath::Ledger),
    (CreditPath::RevenueCatPurchase, RefundPath::RevenueCat),
    (CreditPath::LedgerGrant, RefundPath::RevenueCat),
];

/// Context switches a search that also crashes a flow after every write allows between the two flows.
const CRASH_SWITCHES: usize = 2;

const STARTS: [Start; 2] = [Start::Nothing, Start::PendingRow];

fn transaction(id: &str) -> LedgerTransaction {
    LedgerTransaction {
        transaction_id: id.to_string(),
        original_transaction_id: id.to_string(),
        bundle_id: BUNDLE.to_string(),
        product_id: PRODUCT.to_string(),
        kind: "Consumable".to_string(),
        app_account_token: Some(MIDGAR.to_string()),
        environment: "Production".to_string(),
        purchase_date: 1_760_000_000_000,
        revocation_date: None,
        in_app_ownership_type: Some("PURCHASED".to_string()),
        quantity: 1,
        registry_slug: Some("dream-eater".to_string()),
    }
}

fn refunded(id: &str) -> LedgerTransaction {
    LedgerTransaction { revocation_date: Some(1_760_000_100_000), ..transaction(id) }
}

thread_local! {
    static D1: &'static LocalD1 = Box::leak(Box::new(linked_d1()));
}

/// The one migrated database a thread's worlds share: the buyer linked to a wallet, no purchase and no claim.
fn linked_d1() -> LocalD1 {
    let d1 = LocalD1::migrated();
    d1.user(APP, "u1", 0);
    d1.run(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, 'u1', ?2, 'linked', '2026-10-01T00:00:00+00:00')",
        &[APP.into(), MIDGAR.into()],
    );
    d1
}

struct World {
    d1: &'static LocalD1,
    wallets: FakeWallets,
}

impl World {
    /// A world with the other pack already credited and the tables the two paths race on emptied, so thousands of
    /// interleavings do not each migrate a database.
    fn new(start: Start) -> Self {
        let world = Self { d1: D1.with(|d1| *d1), wallets: FakeWallets::new() };
        for table in ["credit_purchases", "purchase_refunds", "wallet_summary"] {
            world.d1.run(&format!("DELETE FROM {table}"), &[]);
        }
        let other = json!({"key": format!("tx:{OTHER_TX}"), "tenant": APP, "kind": "purchase", "amount": PACK_CREDITS, "source": "revenuecat", "description": "Another pack"});
        assert_eq!(world.wallets.call(MIDGAR, "POST", "/grant", Some(&other)).0, 200);
        if let Start::PendingRow = start {
            block_on(purchase::record_pending(world.d1, APP, "u1", "small", PACK_CREDITS as u32, 299, TX)).unwrap();
        }
        world
    }

    fn entries(&self, key: &str) -> usize {
        let (_, body) = self.wallets.call(MIDGAR, "GET", &format!("/entries?tenant={APP}&limit=200"), None);
        body["entries"].as_array().unwrap().iter().filter(|e| e["key"] == key).count()
    }

    fn rows(&self) -> i64 {
        self.d1.int("SELECT COUNT(*) FROM credit_purchases WHERE payment_id = ?1", &[TX.into()])
    }

    fn claims(&self) -> i64 {
        self.d1.int("SELECT COUNT(*) FROM purchase_refunds WHERE transaction_id = ?1", &[TX.into()])
    }

    fn status(&self) -> Option<String> {
        block_on(purchase::revenuecat_status(self.d1, APP, TX)).unwrap()
    }
}

/// One credit path of the transaction, every store call parked at its contested statements and wallet calls and failing
/// once the budget is spent.
async fn credit(path: CreditPath, world: &World, budget: &Budget) -> Result<(), AppError> {
    let (flaky_d1, flaky_wallets) = (Flaky { inner: world.d1, budget }, Flaky { inner: &world.wallets, budget });
    let (d1, wallets) = (Stepped { inner: &flaky_d1 }, Stepped { inner: &flaky_wallets });
    match path {
        CreditPath::LedgerGrant => {
            ledger_grant::credit(&d1, &wallets, true, &transaction(TX)).await?;
        }
        CreditPath::SignedValidate => {
            if pack_refund::refunded_error(&d1, APP, TX).await?.is_some() {
                return Ok(());
            }
            if already_settled(purchase::revenuecat_status(&d1, APP, TX).await?.as_deref()) {
                return Ok(());
            }
            let ledger = WalletLedger { db: &d1, wallets: &wallets };
            credit_pack(&d1, &ledger, APP, "u1", "small", PACK_CREDITS as u32, 299, TX).await?;
        }
        CreditPath::RevenueCatPurchase => {
            let ledger = WalletLedger { db: &d1, wallets: &wallets };
            credit_pack(&d1, &ledger, APP, "u1", "small", PACK_CREDITS as u32, 299, TX).await?;
        }
    }
    Ok(())
}

/// One refund path of the transaction, parked and budgeted like `credit`.
async fn refund(path: RefundPath, world: &World, budget: &Budget) -> Result<(), AppError> {
    let (flaky_d1, flaky_wallets) = (Flaky { inner: world.d1, budget }, Flaky { inner: &world.wallets, budget });
    let (d1, wallets) = (Stepped { inner: &flaky_d1 }, Stepped { inner: &flaky_wallets });
    match path {
        RefundPath::Ledger => {
            ledger_refund::reverse(&d1, &wallets, &refunded(TX)).await?;
        }
        RefundPath::RevenueCat => {
            let ledger = WalletLedger { db: &d1, wallets: &wallets };
            refund_pack(&d1, &ledger, APP, "small", TX).await?;
        }
    }
    Ok(())
}

/// The two end states a credit and a refund of one transaction may reach: never credited, with the refund claimed as the
/// tombstone, or credited once and reversed once. Anything else, such as a credit nobody reverses, a reversal without a
/// credit, a second reversal or a second row, fails.
fn assert_linearizable(world: &World, context: &str) {
    let (status, grants, reversals) = (world.status(), world.entries(&format!("tx:{TX}")), world.entries(&pack_refund::reversal_key(TX)));
    let allowed = match (status.as_deref(), grants, reversals) {
        (None, 0, 0) => "never credited",
        (Some("refunded"), 1, 1) => "credited and reversed once",
        other => panic!("{context}: {other:?} is neither never credited nor credited and reversed once"),
    };
    assert_eq!(world.claims(), 1, "{context}: {allowed}");
    assert!(world.rows() <= 1, "{context}: one purchase row at most");
    assert_eq!(world.wallets.balance(MIDGAR, APP), PACK_CREDITS, "{context}: {allowed}, only the other pack is left");
    assert!(world.wallets.verified(MIDGAR), "{context}: the wallet's ledger recomputes");
}

/// A crash budget for each of the two flows: `None` runs it unhindered.
#[derive(Clone, Copy)]
struct Crash {
    credit: Option<usize>,
    refund: Option<usize>,
}

impl Crash {
    fn none() -> Self {
        Self { credit: None, refund: None }
    }
}

fn budget(ticks: Option<usize>) -> Budget {
    ticks.map(Budget::of).unwrap_or_else(Budget::unlimited)
}

/// Runs one interleaving of a credit and a refund with the crash budgets, retries every delivery that failed once the
/// other one is done, checks the end state, and reports whether a crash was injected.
fn run(pair: (CreditPath, RefundPath), start: Start, crash: Crash, prefix: &[usize], crashed: &mut bool) -> Vec<Pick> {
    let world = World::new(start);
    let (credit_budget, refund_budget) = (budget(crash.credit), budget(crash.refund));
    let (credited, refunded, picks) =
        race(credit(pair.0, &world, &credit_budget), refund(pair.1, &world, &refund_budget), prefix);
    let context = format!("{pair:?} {start:?} prefix {prefix:?} crash credit {:?} refund {:?}", crash.credit, crash.refund);
    if credit_budget.exhausted() || refund_budget.exhausted() {
        *crashed = true;
    }
    credit_budget.lift();
    refund_budget.lift();
    if crash.credit.is_none() {
        credited.as_ref().unwrap_or_else(|e| panic!("{context}: the credit failed without a crash: {e:?}"));
    }
    if crash.refund.is_none() {
        refunded.as_ref().unwrap_or_else(|e| panic!("{context}: the refund failed without a crash: {e:?}"));
    }
    if credited.is_err() {
        block_on(credit(pair.0, &world, &credit_budget)).unwrap_or_else(|e| panic!("{context}: the credit's retry failed: {e:?}"));
    }
    if refunded.is_err() {
        block_on(refund(pair.1, &world, &refund_budget)).unwrap_or_else(|e| panic!("{context}: the refund's retry failed: {e:?}"));
    }
    assert_linearizable(&world, &context);
    picks
}

/// Every interleaving of the pair's steps from one start, with no crash. Returns how many were run.
fn all_interleavings(pair: (CreditPath, RefundPath), start: Start) -> usize {
    let mut crashed = false;
    let count = interleavings(None, |prefix| run(pair, start, Crash::none(), prefix, &mut crashed));
    assert!(!crashed);
    count
}

/// Runs every interleaving of the pair from both starts, with no crash.
fn check_every_interleaving(pair: (CreditPath, RefundPath)) {
    for start in STARTS {
        let count = all_interleavings(pair, start);
        eprintln!("interleavings {pair:?} {start:?}: {count}");
        assert!(count >= 100, "{pair:?} {start:?}: {count} interleavings is too few to cover the steps");
    }
}

/// Runs every interleaving of the pair from both starts with each flow crashing after each of its writes in turn.
fn check_every_interleaving_with_a_crash(pair: (CreditPath, RefundPath)) {
    for start in STARTS {
        for credit_crashes in [true, false] {
            let (runs, crash_points) = crash_everywhere(pair, start, credit_crashes);
            eprintln!("interleavings with a crash {pair:?} {start:?} credit crashes {credit_crashes}: {runs} over {crash_points} crash points");
            assert!(crash_points >= 3 && runs >= 100, "{pair:?} {start:?}: the injector walked through the writes ({crash_points}, {runs})");
        }
    }
}

#[test]
fn every_interleaving_of_a_ledger_grant_and_a_ledger_refund_ends_in_an_allowed_state() {
    check_every_interleaving(PAIRS[0]);
}

#[test]
fn every_interleaving_of_a_signed_validate_and_a_ledger_refund_ends_in_an_allowed_state() {
    check_every_interleaving(PAIRS[1]);
}

#[test]
fn every_interleaving_of_a_revenuecat_purchase_and_a_revenuecat_refund_ends_in_an_allowed_state() {
    check_every_interleaving(PAIRS[2]);
}

#[test]
fn every_interleaving_of_a_ledger_grant_and_a_revenuecat_refund_ends_in_an_allowed_state() {
    check_every_interleaving(PAIRS[3]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_ledger_grant_and_a_ledger_refund_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(PAIRS[0]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_signed_validate_and_a_ledger_refund_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(PAIRS[1]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_revenuecat_purchase_and_a_revenuecat_refund_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(PAIRS[2]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_ledger_grant_and_a_revenuecat_refund_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(PAIRS[3]);
}

/// Every interleaving of the pair's steps with one flow crashing after each of its writes in turn (and before it, so
/// the answer can be lost), the other flow running to the end, and the crashed delivery retried afterwards. Returns how
/// many interleavings were run and how many crash points were walked through.
fn crash_everywhere(pair: (CreditPath, RefundPath), start: Start, credit_crashes: bool) -> (usize, usize) {
    let (mut runs, mut crash_points) = (0, 0);
    for ticks in 0.. {
        let crash = if credit_crashes { Crash { credit: Some(ticks), refund: None } } else { Crash { credit: None, refund: Some(ticks) } };
        let mut crashed = false;
        runs += interleavings(Some(CRASH_SWITCHES), |prefix| run(pair, start, crash, prefix, &mut crashed));
        if !crashed {
            break;
        }
        crash_points += 1;
    }
    (runs, crash_points)
}

/// Where the crashed delivery's retry runs relative to the other path when they do not overlap.
#[derive(Debug, Clone, Copy)]
enum Order {
    CrashRetryThenOther,
    CrashOtherThenRetry,
    OtherThenCrashRetry,
}

const ORDERS: [Order; 3] = [Order::CrashRetryThenOther, Order::CrashOtherThenRetry, Order::OtherThenCrashRetry];

/// One crash of one path, its retry, and the other path, run one after the other in the given order. Returns whether a
/// crash was injected.
fn crash_in_sequence(pair: (CreditPath, RefundPath), start: Start, credit_crashes: bool, ticks: usize, order: Order) -> bool {
    let world = World::new(start);
    let context = format!("{pair:?} {start:?} {order:?} credit crashes {credit_crashes} after {ticks} ticks");
    let crashing_path = |budget: &Budget| {
        if credit_crashes {
            block_on(credit(pair.0, &world, budget))
        } else {
            block_on(refund(pair.1, &world, budget))
        }
    };
    let other_path = || {
        let unhindered = Budget::unlimited();
        let outcome = if credit_crashes { block_on(refund(pair.1, &world, &unhindered)) } else { block_on(credit(pair.0, &world, &unhindered)) };
        outcome.unwrap_or_else(|e| panic!("{context}: the other path failed: {e:?}"));
    };
    let crash = || {
        let budget = Budget::of(ticks);
        let _ = crashing_path(&budget);
        budget.exhausted()
    };
    let retry = || crashing_path(&Budget::unlimited()).unwrap_or_else(|e| panic!("{context}: the retry failed: {e:?}"));
    let crashed = match order {
        Order::CrashRetryThenOther => {
            let crashed = crash();
            retry();
            other_path();
            crashed
        }
        Order::CrashOtherThenRetry => {
            let crashed = crash();
            other_path();
            retry();
            crashed
        }
        Order::OtherThenCrashRetry => {
            other_path();
            let crashed = crash();
            retry();
            crashed
        }
    };
    assert_linearizable(&world, &context);
    crashed
}

#[test]
fn a_crash_after_any_write_then_a_retry_ends_never_credited_or_credited_and_reversed_once_in_every_order() {
    for pair in PAIRS {
        for start in STARTS {
            for credit_crashes in [true, false] {
                let mut crash_points = 0;
                for order in ORDERS {
                    for ticks in 0.. {
                        if !crash_in_sequence(pair, start, credit_crashes, ticks, order) {
                            break;
                        }
                        crash_points += 1;
                    }
                }
                eprintln!("crash points {pair:?} {start:?} credit crashes {credit_crashes}: {crash_points} over the three orders");
                assert!(crash_points >= 3, "{pair:?} {start:?}: the injector walked through the writes ({crash_points})");
            }
        }
    }
}

