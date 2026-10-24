use futures::executor::block_on;
use serde_json::json;

use super::pack_credit::{credit_pack, refund_pack};
use super::d1::D1Exec;
use super::pack_refund;
use super::purchase;
use super::testing::{race, interleavings, Budget, FakeWallets, Flaky, LocalD1, Pick, Stepped, TestLedger};
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

/// The ways a store transaction gets credited. The two `Local` paths are the ones that credit a D1 balance: the RevenueCat
/// webhook and the legacy validate call for a user with no wallet.
#[derive(Debug, Clone, Copy)]
enum CreditPath {
    LedgerGrant,
    SignedValidate,
    RevenueCatPurchase,
    LocalRevenueCatPurchase,
    LocalLegacyValidate,
}

/// Where the buyer's credits live.
#[derive(Debug, Clone, Copy)]
enum Accounts {
    Wallet,
    Local,
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
    Credited,
}

/// A credit path, a refund path and the account both act on.
type Case = (CreditPath, RefundPath, Accounts);

const CASES: [Case; 7] = [
    (CreditPath::LedgerGrant, RefundPath::Ledger, Accounts::Wallet),
    (CreditPath::SignedValidate, RefundPath::Ledger, Accounts::Wallet),
    (CreditPath::RevenueCatPurchase, RefundPath::RevenueCat, Accounts::Wallet),
    (CreditPath::LedgerGrant, RefundPath::RevenueCat, Accounts::Wallet),
    (CreditPath::SignedValidate, RefundPath::RevenueCat, Accounts::Local),
    (CreditPath::LocalRevenueCatPurchase, RefundPath::RevenueCat, Accounts::Local),
    (CreditPath::LocalLegacyValidate, RefundPath::RevenueCat, Accounts::Local),
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
    static LINKED: &'static LocalD1 = Box::leak(Box::new(prepared(true)));
    static UNLINKED: &'static LocalD1 = Box::leak(Box::new(prepared(false)));
}

/// A migrated database a thread's worlds share: the buyer with a D1 balance of the other pack's credits, linked to a wallet
/// or not, no purchase and no claim.
fn prepared(linked: bool) -> LocalD1 {
    let d1 = LocalD1::migrated();
    d1.user(APP, "u1", PACK_CREDITS);
    if linked {
        d1.run(
            "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, 'u1', ?2, 'linked', '2026-10-01T00:00:00+00:00')",
            &[APP.into(), MIDGAR.into()],
        );
    }
    d1
}

struct World {
    d1: &'static LocalD1,
    wallets: FakeWallets,
    accounts: Accounts,
}

impl World {
    /// A world with the other pack already credited and the tables the two paths race on emptied, so thousands of
    /// interleavings do not each migrate a database.
    fn new(start: Start, accounts: Accounts) -> Self {
        let d1 = match accounts {
            Accounts::Wallet => LINKED.with(|d1| *d1),
            Accounts::Local => UNLINKED.with(|d1| *d1),
        };
        let world = Self { d1, wallets: FakeWallets::new(), accounts };
        for table in ["credit_purchases", "purchase_refunds", "wallet_summary", "credit_transactions"] {
            world.d1.run(&format!("DELETE FROM {table}"), &[]);
        }
        world.d1.run("UPDATE user_credits SET balance = ?1, lifetime_purchased = 0, lifetime_spent = 0", &[PACK_CREDITS.into()]);
        if let Accounts::Wallet = accounts {
            let other = json!({"key": format!("tx:{OTHER_TX}"), "tenant": APP, "kind": "purchase", "amount": PACK_CREDITS, "source": "revenuecat", "description": "Another pack"});
            assert_eq!(world.wallets.call(MIDGAR, "POST", "/grant", Some(&other)).0, 200);
        }
        match start {
            Start::Nothing => {}
            Start::PendingRow => {
                block_on(purchase::record_pending(world.d1, APP, "u1", "small", PACK_CREDITS as u32, 299, TX)).unwrap();
            }
            Start::Credited => block_on(credit(CreditPath::SignedValidate, &world, &Budget::unlimited())).unwrap(),
        }
        world
    }

    /// How many times the transaction's pack was granted: wallet entries under `tx:<id>`, or the D1 ledger rows of the
    /// purchase.
    fn grants(&self) -> usize {
        match self.accounts {
            Accounts::Wallet => self.entries(&format!("tx:{TX}")),
            Accounts::Local => self.d1.int(
                "SELECT COUNT(*) FROM credit_transactions t JOIN credit_purchases p ON p.id = t.reference_id
                 WHERE p.payment_id = ?1 AND t.type = 'purchase'",
                &[TX.into()],
            ) as usize,
        }
    }

    /// How many times the transaction's pack was taken back.
    fn reversals(&self) -> usize {
        match self.accounts {
            Accounts::Wallet => self.entries(&pack_refund::reversal_key(TX)),
            Accounts::Local => {
                self.d1.int("SELECT COUNT(*) FROM credit_transactions WHERE reference_id = ?1", &[crate::purchases::refund_reference(TX).into()]) as usize
            }
        }
    }

    fn balance(&self) -> i64 {
        match self.accounts {
            Accounts::Wallet => self.wallets.balance(MIDGAR, APP),
            Accounts::Local => self.d1.d1_balance(APP, "u1"),
        }
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
            let ledger = TestLedger { db: &d1, wallets: &wallets };
            credit_pack(&d1, &ledger, APP, "u1", "small", PACK_CREDITS as u32, 299, TX).await?;
        }
        CreditPath::RevenueCatPurchase => {
            let ledger = TestLedger { db: &d1, wallets: &wallets };
            credit_pack(&d1, &ledger, APP, "u1", "small", PACK_CREDITS as u32, 299, TX).await?;
        }
        CreditPath::LocalRevenueCatPurchase => {
            if pack_refund::is_claimed(&d1, APP, TX).await? || purchase::revenuecat_status(&d1, APP, TX).await?.is_some() {
                return Ok(());
            }
            local_credit(&d1).await?;
        }
        CreditPath::LocalLegacyValidate => {
            if purchase::revenuecat_status(&d1, APP, TX).await?.as_deref() == Some("completed") {
                return Ok(());
            }
            if pack_refund::refunded_error(&d1, APP, TX).await?.is_some() {
                return Ok(());
            }
            local_credit(&d1).await?;
        }
    }
    Ok(())
}

/// What the webhook's local branch and the legacy validate call end with: the purchase row and the D1 credit, where a
/// refund claimed in the meantime is the refusal both answer without a credit.
async fn local_credit(d1: &impl D1Exec) -> Result<(), AppError> {
    match purchase::record_and_complete_locally(d1, APP, "u1", "small", PACK_CREDITS as u32, 299, TX).await {
        Ok(_) | Err(AppError::Refunded(_)) => Ok(()),
        Err(error) => Err(error),
    }
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
            let ledger = TestLedger { db: &d1, wallets: &wallets };
            refund_pack(&d1, &ledger, APP, "small", TX).await?;
        }
    }
    Ok(())
}

/// The two end states a credit and a refund of one transaction may reach: never credited, with the refund claimed as the
/// tombstone, or credited once and reversed once. Anything else, such as a credit nobody reverses, a reversal without a
/// credit, a second reversal or a second row, fails.
fn assert_linearizable(world: &World, context: &str) {
    let (status, grants, reversals) = (world.status(), world.grants(), world.reversals());
    let allowed = match (status.as_deref(), grants, reversals) {
        (None, 0, 0) => "never credited",
        (Some("refunded"), 1, 1) => "credited and reversed once",
        other => panic!("{context}: {other:?} is neither never credited nor credited and reversed once"),
    };
    assert_eq!(world.claims(), 1, "{context}: {allowed}");
    assert!(world.rows() <= 1, "{context}: one purchase row at most");
    assert_eq!(world.balance(), PACK_CREDITS, "{context}: {allowed}, only the other pack is left");
    match world.accounts {
        Accounts::Wallet => assert!(world.wallets.verified(MIDGAR), "{context}: the wallet's ledger recomputes"),
        Accounts::Local => {
            let (purchased, spent) = (world.d1.int("SELECT lifetime_purchased FROM user_credits", &[]), world.d1.int("SELECT lifetime_spent FROM user_credits", &[]));
            assert_eq!((purchased, spent), (grants as i64 * PACK_CREDITS, reversals as i64 * PACK_CREDITS), "{context}: the lifetime totals follow the ledger rows");
            let rows_balance = world.d1.int("SELECT COALESCE(SUM(amount), 0) FROM credit_transactions", &[]);
            assert_eq!(rows_balance, world.balance() - PACK_CREDITS, "{context}: the ledger rows add up to the balance");
        }
    }
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
fn run(pair: Case, start: Start, crash: Crash, prefix: &[usize], crashed: &mut bool) -> Vec<Pick> {
    let world = World::new(start, pair.2);
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
fn all_interleavings(pair: Case, start: Start) -> usize {
    let mut crashed = false;
    let count = interleavings(None, |prefix| run(pair, start, Crash::none(), prefix, &mut crashed));
    assert!(!crashed);
    count
}

/// Runs every interleaving of the pair from both starts, with no crash.
fn check_every_interleaving(pair: Case) {
    for start in STARTS {
        let count = all_interleavings(pair, start);
        eprintln!("interleavings {pair:?} {start:?}: {count}");
        let least = match start {
            Start::Nothing => 100,
            Start::PendingRow | Start::Credited => 10,
        };
        assert!(count >= least, "{pair:?} {start:?}: {count} interleavings is too few to cover the steps");
    }
}

/// Runs every interleaving of the pair from both starts with each flow crashing after each of its writes in turn.
fn check_every_interleaving_with_a_crash(pair: Case) {
    for start in STARTS {
        for credit_crashes in [true, false] {
            let (runs, crash_points) = crash_everywhere(pair, start, credit_crashes);
            eprintln!("interleavings with a crash {pair:?} {start:?} credit crashes {credit_crashes}: {runs} over {crash_points} crash points");
            let (least_points, least_runs) = match start {
                Start::Nothing => (3, 100),
                Start::PendingRow | Start::Credited => (1, 10),
            };
            assert!(crash_points >= least_points && runs >= least_runs, "{pair:?} {start:?}: the injector walked through the writes ({crash_points}, {runs})");
        }
    }
}

#[test]
fn every_interleaving_of_a_ledger_grant_and_a_ledger_refund_ends_in_an_allowed_state() {
    check_every_interleaving(CASES[0]);
}

#[test]
fn every_interleaving_of_a_signed_validate_and_a_ledger_refund_ends_in_an_allowed_state() {
    check_every_interleaving(CASES[1]);
}

#[test]
fn every_interleaving_of_a_revenuecat_purchase_and_a_revenuecat_refund_ends_in_an_allowed_state() {
    check_every_interleaving(CASES[2]);
}

#[test]
fn every_interleaving_of_a_ledger_grant_and_a_revenuecat_refund_ends_in_an_allowed_state() {
    check_every_interleaving(CASES[3]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_ledger_grant_and_a_ledger_refund_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(CASES[0]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_signed_validate_and_a_ledger_refund_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(CASES[1]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_revenuecat_purchase_and_a_revenuecat_refund_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(CASES[2]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_ledger_grant_and_a_revenuecat_refund_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(CASES[3]);
}

#[test]
fn every_interleaving_of_a_signed_validate_and_a_revenuecat_refund_for_an_unlinked_user_ends_in_an_allowed_state() {
    check_every_interleaving(CASES[4]);
}

#[test]
fn every_interleaving_of_a_revenuecat_purchase_and_a_revenuecat_refund_for_an_unlinked_user_ends_in_an_allowed_state() {
    check_every_interleaving(CASES[5]);
}

#[test]
fn every_interleaving_of_a_legacy_validate_and_a_revenuecat_refund_for_an_unlinked_user_ends_in_an_allowed_state() {
    check_every_interleaving(CASES[6]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_signed_validate_and_a_revenuecat_refund_for_an_unlinked_user_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(CASES[4]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_revenuecat_purchase_and_a_revenuecat_refund_for_an_unlinked_user_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(CASES[5]);
}

#[test]
fn a_crash_in_any_interleaving_of_a_legacy_validate_and_a_revenuecat_refund_for_an_unlinked_user_is_finished_by_the_retry() {
    check_every_interleaving_with_a_crash(CASES[6]);
}

/// Every interleaving of two refund deliveries of one credited transaction, from the two paths that reverse a pack.
fn two_refunds(first: RefundPath, second: RefundPath, accounts: Accounts) -> usize {
    interleavings(None, |prefix| {
        let world = World::new(Start::Credited, accounts);
        let (unhindered, other) = (Budget::unlimited(), Budget::unlimited());
        let (one, two, picks) = race(refund(first, &world, &unhindered), refund(second, &world, &other), prefix);
        let context = format!("{first:?} and {second:?} {accounts:?} prefix {prefix:?}");
        one.unwrap_or_else(|e| panic!("{context}: {e:?}"));
        two.unwrap_or_else(|e| panic!("{context}: {e:?}"));
        assert_eq!((world.status().as_deref(), world.grants(), world.reversals()), (Some("refunded"), 1, 1), "{context}");
        assert_eq!((world.claims(), world.balance()), (1, PACK_CREDITS), "{context}");
        picks
    })
}

#[test]
fn every_interleaving_of_two_refund_deliveries_reverses_the_pack_once() {
    for (first, second, accounts) in [
        (RefundPath::Ledger, RefundPath::Ledger, Accounts::Wallet),
        (RefundPath::Ledger, RefundPath::RevenueCat, Accounts::Wallet),
        (RefundPath::RevenueCat, RefundPath::RevenueCat, Accounts::Wallet),
        (RefundPath::RevenueCat, RefundPath::RevenueCat, Accounts::Local),
    ] {
        let count = two_refunds(first, second, accounts);
        eprintln!("interleavings of two refunds {first:?} {second:?} {accounts:?}: {count}");
        assert!(count >= 10);
    }
}

/// Every interleaving of the pair's steps with one flow crashing after each of its writes in turn (and before it, so
/// the answer can be lost), the other flow running to the end, and the crashed delivery retried afterwards. Returns how
/// many interleavings were run and how many crash points were walked through.
fn crash_everywhere(pair: Case, start: Start, credit_crashes: bool) -> (usize, usize) {
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
fn crash_in_sequence(pair: Case, start: Start, credit_crashes: bool, ticks: usize, order: Order) -> bool {
    let world = World::new(start, pair.2);
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
    for pair in CASES {
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
                let least = match start {
                    Start::Nothing => 3,
                    Start::PendingRow | Start::Credited => 1,
                };
                assert!(crash_points >= least, "{pair:?} {start:?}: the injector walked through the writes ({crash_points})");
            }
        }
    }
}

