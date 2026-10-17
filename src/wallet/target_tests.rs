use std::cell::Cell;

use futures::executor::block_on;

use super::client::WalletTransport;
use super::d1::D1Exec;
use super::link::Linker;
use super::links;
use super::purchase::{self, PurchaseRow};
use super::resolve;
use super::target::{credit_target, is_midgar_id, IdentityLookup};
use super::testing::{Budget, FakeWallets, Flaky, LocalD1};
use crate::error::AppError;
use crate::ledgers::Account;

const APP: &str = "dreameater";
const MIDGAR: &str = "0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d47";
const OTHER: &str = "0195aaaa-7b1c-4d2e-9a60-3c5e8f1b2d47";

struct Identity {
    knows: Vec<&'static str>,
    down: bool,
    asked: Cell<usize>,
}

impl Identity {
    fn knowing(ids: &[&'static str]) -> Self {
        Self { knows: ids.to_vec(), down: false, asked: Cell::new(0) }
    }
}

impl IdentityLookup for Identity {
    async fn exists(&self, midgar_id: &str) -> Result<bool, AppError> {
        self.asked.set(self.asked.get() + 1);
        if self.down {
            return Err(AppError::InternalError("identity is down".into()));
        }
        Ok(self.knows.contains(&midgar_id))
    }
}

struct NoDb;

impl D1Exec for NoDb {
    async fn rows(&self, _: &str, _: &[super::sql::Val]) -> Result<Vec<serde_json::Value>, AppError> {
        panic!("linking off must not read D1")
    }
    async fn exec(&self, _: &str, _: &[super::sql::Val]) -> Result<u64, AppError> {
        panic!("linking off must not write D1")
    }
    async fn batch(&self, _: &[super::d1::Stmt]) -> Result<(), AppError> {
        panic!("linking off must not write D1")
    }
}

struct NoWallet;

impl WalletTransport for NoWallet {
    async fn send(&self, _: &str, _: &str, _: &str, _: Option<&serde_json::Value>) -> Result<(u16, serde_json::Value), AppError> {
        panic!("linking off must not reach a wallet")
    }
}

fn target(d1: &LocalD1, identity: &Identity, id: &str) -> Result<Account, AppError> {
    block_on(credit_target(d1, identity, APP, id))
}

fn wallet(midgar: &str, user: &str) -> Account {
    Account::Wallet { midgar_id: midgar.into(), app_id: APP.into(), user_id: user.into() }
}

fn link(d1: &LocalD1, user: &str, midgar: &str, state: &str) {
    d1.run(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, ?2, ?3, ?4, '2026-10-01T00:00:00+00:00')",
        &[APP.into(), user.into(), midgar.into(), state.into()],
    );
}

#[test]
fn a_linked_user_in_any_state_is_a_wallet() {
    let d1 = LocalD1::migrated();
    d1.user(APP, "linked-user", 0);
    d1.user(APP, "moving-user", 0);
    link(&d1, "linked-user", MIDGAR, "linked");
    link(&d1, "moving-user", OTHER, "moving");
    let identity = Identity::knowing(&[]);
    assert_eq!(target(&d1, &identity, "linked-user").unwrap(), wallet(MIDGAR, "linked-user"));
    assert_eq!(target(&d1, &identity, "moving-user").unwrap(), wallet(OTHER, "moving-user"));
    assert_eq!(identity.asked.get(), 0);
}

#[test]
fn a_known_user_without_a_link_is_local_even_when_its_id_is_a_uuid() {
    let d1 = LocalD1::migrated();
    d1.user(APP, MIDGAR, 5);
    let identity = Identity::knowing(&[MIDGAR]);
    assert_eq!(target(&d1, &identity, MIDGAR).unwrap(), Account::local(APP, MIDGAR));
    assert_eq!(identity.asked.get(), 0, "a user the app knows is never looked up in identity");
}

#[test]
fn a_midgar_id_some_user_is_linked_to_goes_to_that_users_wallet_in_any_letter_case() {
    let d1 = LocalD1::migrated();
    d1.user(APP, "newer", 0);
    d1.user(APP, "older", 0);
    d1.run(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, 'newer', ?2, 'linked', '2026-10-02T00:00:00+00:00')",
        &[APP.into(), MIDGAR.into()],
    );
    d1.run(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, 'older', ?2, 'linked', '2026-10-01T00:00:00+00:00')",
        &[APP.into(), MIDGAR.into()],
    );
    let identity = Identity::knowing(&[]);
    assert_eq!(target(&d1, &identity, MIDGAR).unwrap(), wallet(MIDGAR, "older"));
    assert_eq!(target(&d1, &identity, &MIDGAR.to_uppercase()).unwrap(), wallet(MIDGAR, "older"));
    assert_eq!(identity.asked.get(), 0);
}

#[test]
fn an_unknown_midgar_id_that_identity_holds_gets_a_user_and_a_linked_wallet() {
    let d1 = LocalD1::migrated();
    let identity = Identity::knowing(&[MIDGAR]);
    let Account::Wallet { midgar_id, user_id, .. } = target(&d1, &identity, &MIDGAR.to_uppercase()).unwrap() else { panic!("expected a wallet") };
    assert_eq!(midgar_id, MIDGAR);
    assert_eq!(d1.int("SELECT COUNT(*) FROM users WHERE id = ?1 AND provider = 'midgar'", &[user_id.as_str().into()]), 1);
    assert_eq!(d1.int("SELECT COUNT(*) FROM wallet_links WHERE user_id = ?1 AND state = 'linked' AND moved_credits = 0", &[user_id.as_str().into()]), 1);
    let again = target(&d1, &identity, MIDGAR).unwrap();
    assert_eq!(again, wallet(MIDGAR, &user_id));
    assert_eq!(d1.int("SELECT COUNT(*) FROM users WHERE provider = 'midgar'", &[]), 1);
    assert_eq!(identity.asked.get(), 1, "the second grant found the link and asked nobody");
}

#[test]
fn an_unknown_midgar_id_identity_does_not_hold_is_an_error_and_creates_nothing() {
    let d1 = LocalD1::migrated();
    let identity = Identity::knowing(&[OTHER]);
    assert!(target(&d1, &identity, MIDGAR).is_err());
    assert_eq!(d1.int("SELECT COUNT(*) FROM users", &[]), 0);
    assert_eq!(d1.int("SELECT COUNT(*) FROM wallet_links", &[]), 0);
}

#[test]
fn an_identity_outage_is_an_error_and_creates_nothing() {
    let d1 = LocalD1::migrated();
    let identity = Identity { knows: vec![MIDGAR], down: true, asked: Cell::new(0) };
    assert!(target(&d1, &identity, MIDGAR).is_err());
    assert_eq!(d1.int("SELECT COUNT(*) FROM users", &[]), 0);
}

#[test]
fn anything_that_is_not_a_known_user_or_a_midgar_id_is_an_error_without_a_lookup() {
    let d1 = LocalD1::migrated();
    let identity = Identity::knowing(&[MIDGAR]);
    for id in ["$RCAnonymousID:abc", "", "not-a-uuid", &MIDGAR.replace('-', "")] {
        assert!(target(&d1, &identity, id).is_err(), "{id}");
    }
    assert_eq!(identity.asked.get(), 0);
    assert!(is_midgar_id(MIDGAR) && is_midgar_id(&MIDGAR.to_uppercase()) && !is_midgar_id(&MIDGAR.replace('-', "")));
}

#[test]
fn with_linking_off_every_grant_target_is_local_and_nothing_is_read() {
    let identity = Identity::knowing(&[MIDGAR]);
    for id in ["a-mako-user", MIDGAR, "$RCAnonymousID:abc"] {
        assert_eq!(block_on(resolve::credit_target(false, &NoDb, &identity, APP, id)).unwrap(), Account::local(APP, id));
    }
    assert_eq!(identity.asked.get(), 0);
    let linker = Linker { db: &NoDb, wallets: &NoWallet };
    assert_eq!(block_on(resolve::account_for_user(false, &linker, APP, "u1")).unwrap(), Account::local(APP, "u1"));
}

#[test]
fn with_linking_off_a_user_with_a_link_row_is_still_local() {
    let d1 = LocalD1::migrated();
    d1.user(APP, "u1", 0);
    link(&d1, "u1", MIDGAR, "linked");
    let wallets = FakeWallets::new();
    let linker = Linker { db: &d1, wallets: &wallets };
    assert_eq!(block_on(resolve::account_for_user(false, &linker, APP, "u1")).unwrap(), Account::local(APP, "u1"));
    assert_eq!(block_on(resolve::account_for_user(true, &linker, APP, "u1")).unwrap(), wallet(MIDGAR, "u1"));
    assert_eq!(block_on(resolve::account_for_user(true, &linker, APP, "stranger")).unwrap(), Account::local(APP, "stranger"));
}

#[test]
fn a_link_that_is_still_moving_is_finished_before_anything_is_addressed_to_it() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    d1.user(APP, "u1", 25);
    link(&d1, "u1", MIDGAR, "moving");
    let linker = Linker { db: &d1, wallets: &wallets };
    assert_eq!(block_on(resolve::account_for_user(true, &linker, APP, "u1")).unwrap(), wallet(MIDGAR, "u1"));
    assert_eq!(wallets.balance(MIDGAR, APP), 25, "the balance is in the wallet before a refund or an adjustment reaches it");
    assert_eq!(d1.d1_balance(APP, "u1"), 0);
    assert_eq!(d1.int("SELECT COUNT(*) FROM wallet_links WHERE user_id = 'u1' AND state = 'linked'", &[]), 1);
}

#[test]
fn the_flag_is_on_only_for_the_exact_value_on() {
    assert!(resolve::flag_is_on(Some("on")));
    for value in [None, Some("off"), Some("ON"), Some("true"), Some("1"), Some(""), Some(" on")] {
        assert!(!resolve::flag_is_on(value), "{value:?}");
    }
}

fn pending(d1: &LocalD1, id: &str, user: &str, provider: &str, payment_id: &str, credits: i64) {
    d1.run(
        "INSERT INTO credit_purchases (id, app_id, user_id, pack_id, credits, amount_usd_cents, payment_provider, payment_id, status, created_at)
         VALUES (?1, ?2, ?3, 'small', ?4, 299, ?5, ?6, 'pending', '2026-10-01T00:00:00+00:00')",
        &[id.into(), APP.into(), user.into(), credits.into(), provider.into(), payment_id.into()],
    );
}

fn row(d1: &LocalD1, id: &str) -> PurchaseRow {
    block_on(purchase::read(d1, id)).unwrap().expect("purchase row")
}

fn complete<W: WalletTransport>(d1: &impl D1Exec, wallets: &W, id: &str, account: &Account) -> Result<bool, AppError> {
    let purchase_row = block_on(purchase::read(d1, id))?.expect("purchase row");
    block_on(purchase::complete_in_wallet(d1, wallets, account, &purchase_row))
}

#[test]
fn a_replayed_revenuecat_webhook_grants_once() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    d1.user(APP, "u1", 0);
    link(&d1, "u1", MIDGAR, "linked");
    let account = wallet(MIDGAR, "u1");
    for _ in 0..3 {
        let id = block_on(purchase::record_pending(&d1, APP, "u1", "small", 100, 299, "2000000123")).unwrap();
        complete(&d1, &wallets, &id, &account).unwrap();
    }
    assert_eq!(wallets.balance(MIDGAR, APP), 100);
    assert_eq!(d1.int("SELECT COUNT(*) FROM credit_purchases", &[]), 1);
    assert_eq!(block_on(purchase::revenuecat_status(&d1, APP, "2000000123")).unwrap().as_deref(), Some("completed"));
    assert!(wallets.verified(MIDGAR));
}

#[test]
fn the_fast_track_and_the_webhook_share_one_grant() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    d1.user(APP, "u1", 0);
    link(&d1, "u1", MIDGAR, "linked");
    let first = block_on(purchase::record_pending(&d1, APP, "u1", "small", 100, 299, "tx-9")).unwrap();
    let second = block_on(purchase::record_pending(&d1, APP, "u1", "small", 100, 299, "tx-9")).unwrap();
    assert_eq!(first, second);
    let account = wallet(MIDGAR, "u1");
    assert!(complete(&d1, &wallets, &first, &account).unwrap());
    assert!(!complete(&d1, &wallets, &second, &account).unwrap(), "already completed: nothing flipped");
    assert_eq!(wallets.balance(MIDGAR, APP), 100);
}

#[test]
fn a_purchase_completed_meanwhile_through_the_local_balance_is_not_granted_again() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    d1.user(APP, "u1", 0);
    link(&d1, "u1", MIDGAR, "linked");
    pending(&d1, "p-1", "u1", "revenuecat", "tx-1", 100);
    let stale = row(&d1, "p-1");
    d1.run("UPDATE credit_purchases SET status = 'completed' WHERE id = 'p-1'", &[]);
    let account = wallet(MIDGAR, "u1");
    let flipped = block_on(purchase::complete_in_wallet(&d1, &wallets, &account, &stale)).unwrap();
    assert!(!flipped);
    assert_eq!(wallets.balance(MIDGAR, APP), 0);
}

#[test]
fn a_claimed_purchase_can_be_released_for_a_retry_and_then_credits_once() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    d1.user(APP, "u1", 0);
    link(&d1, "u1", MIDGAR, "linked");
    pending(&d1, "p-1", "u1", "revenuecat", "tx-1", 100);
    d1.run("UPDATE credit_purchases SET status = 'completed' WHERE id = 'p-1'", &[]);
    block_on(purchase::release_claim(&d1, "p-1")).unwrap();
    assert_eq!(row(&d1, "p-1").status, "pending");
    complete(&d1, &wallets, "p-1", &wallet(MIDGAR, "u1")).unwrap();
    complete(&d1, &wallets, "p-1", &wallet(MIDGAR, "u1")).unwrap();
    assert_eq!(wallets.balance(MIDGAR, APP), 100);
    d1.run("UPDATE credit_purchases SET status = 'refunded' WHERE id = 'p-1'", &[]);
    block_on(purchase::release_claim(&d1, "p-1")).unwrap();
    assert_eq!(row(&d1, "p-1").status, "refunded", "only a completed row is released");
}

#[test]
fn a_crash_between_the_grant_and_the_flip_loses_and_doubles_nothing() {
    let mut crashes = 0;
    for ticks in 0..14 {
        let d1 = LocalD1::migrated();
        let wallets = FakeWallets::new();
        d1.user(APP, "u1", 0);
        link(&d1, "u1", MIDGAR, "linked");
        pending(&d1, "p-1", "u1", "revenuecat", "tx-1", 100);
        let account = wallet(MIDGAR, "u1");
        let budget = Budget::of(ticks);
        let result = complete(&Flaky { inner: &d1, budget: &budget }, &Flaky { inner: &wallets, budget: &budget }, "p-1", &account);
        crashes += result.is_err() as usize;
        budget.lift();
        complete(&d1, &wallets, "p-1", &account).unwrap();
        assert_eq!(row(&d1, "p-1").status, "completed", "ticks {ticks}");
        assert_eq!(wallets.balance(MIDGAR, APP), 100, "ticks {ticks}");
        assert!(wallets.verified(MIDGAR));
    }
    assert!(crashes >= 3);
}

#[test]
fn stripe_and_crypto_grants_are_keyed_by_the_purchase_id() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    d1.user(APP, "u1", 0);
    link(&d1, "u1", MIDGAR, "linked");
    pending(&d1, "p-stripe", "u1", "stripe", "cs_test_1", 50);
    pending(&d1, "p-crypto", "u1", "nowpayments", "np-1", 70);
    let account = wallet(MIDGAR, "u1");
    for id in ["p-stripe", "p-stripe", "p-crypto", "p-crypto"] {
        let _ = complete(&d1, &wallets, id, &account);
    }
    assert_eq!(wallets.balance(MIDGAR, APP), 120);
    let keys = wallets.call(MIDGAR, "GET", &format!("/entries?tenant={APP}"), None).1["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["key"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(keys.contains(&"stripe:p-stripe".to_string()) && keys.contains(&"crypto:p-crypto".to_string()), "{keys:?}");
}

#[test]
fn a_purchase_in_a_terminal_state_cannot_be_completed() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    d1.user(APP, "u1", 0);
    pending(&d1, "p-1", "u1", "stripe", "cs", 50);
    d1.run("UPDATE credit_purchases SET status = 'failed' WHERE id = 'p-1'", &[]);
    assert!(matches!(complete(&d1, &wallets, "p-1", &wallet(MIDGAR, "u1")), Err(AppError::NotFound(_))));
    assert_eq!(wallets.balance(MIDGAR, APP), 0);
}

#[test]
fn the_cron_lists_only_stale_pending_revenuecat_purchases() {
    let d1 = LocalD1::migrated();
    d1.user(APP, "u1", 0);
    pending(&d1, "old-rc", "u1", "revenuecat", "t1", 10);
    pending(&d1, "old-stripe", "u1", "stripe", "s1", 10);
    pending(&d1, "done-rc", "u1", "revenuecat", "t2", 10);
    d1.run("UPDATE credit_purchases SET status = 'completed' WHERE id = 'done-rc'", &[]);
    pending(&d1, "fresh-rc", "u1", "revenuecat", "t3", 10);
    d1.run("UPDATE credit_purchases SET created_at = '2099-01-01T00:00:00+00:00' WHERE id = 'fresh-rc'", &[]);
    assert_eq!(block_on(purchase::stale_revenuecat(&d1, 50)).unwrap(), ["old-rc"]);
    assert_eq!(block_on(purchase::stale_revenuecat(&d1, 0)).unwrap().len(), 0);
}

#[test]
fn a_user_created_for_a_purchase_has_the_row_a_purchase_needs() {
    let d1 = LocalD1::migrated();
    let now = "2026-10-17T00:00:00+00:00";
    let first = block_on(links::midgar_user(&d1, APP, MIDGAR, now)).unwrap();
    let second = block_on(links::midgar_user(&d1, APP, MIDGAR, now)).unwrap();
    assert_eq!(first, second);
    let id = block_on(purchase::record_pending(&d1, APP, &first, "small", 5, 299, "tx-1")).unwrap();
    assert_eq!(row(&d1, &id).user_id, first, "the foreign key to users holds");
}
