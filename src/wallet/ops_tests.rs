use futures::executor::block_on;
use serde_json::{json, Value};

use super::client::WalletTransport;
use super::ops::{Charge, Grant, Reversal, WalletOps};
use super::testing::{FakeWallets, LocalD1};
use super::types::Kind;
use crate::error::{is_wallet_unavailable, AppError};

const APP: &str = "dreameater";
const OTHER_APP: &str = "payday";
const MIDGAR: &str = "0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d47";

struct Down;

impl WalletTransport for Down {
    async fn send(&self, _: &str, _: &str, _: &str, _: Option<&Value>) -> Result<(u16, Value), AppError> {
        Err(AppError::WalletUnavailable)
    }
}

struct Sick(u16);

impl WalletTransport for Sick {
    async fn send(&self, _: &str, _: &str, _: &str, _: Option<&Value>) -> Result<(u16, Value), AppError> {
        Ok((self.0, json!({ "error": "storage_error" })))
    }
}

fn purchase(key: &str, amount: u32) -> Grant<'static> {
    Grant { key: key.into(), kind: Kind::Purchase, source: "revenuecat", amount, description: "Pack", reference: Some("p-1") }
}

fn charge(reference: &str, amount: u32) -> Charge<'static> {
    Charge { key: format!("spend:{reference}"), amount, description: "Chat", reference: Box::leak(reference.to_string().into_boxed_str()) }
}

fn world() -> (LocalD1, FakeWallets) {
    (LocalD1::migrated(), FakeWallets::new())
}

#[test]
fn a_grant_and_a_spend_move_the_balance_and_mirror_the_totals() {
    let (d1, wallets) = world();
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    assert_eq!(block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 50))).unwrap().balance, 50);
    assert_eq!(block_on(ops.spend(MIDGAR, APP, &charge("chat:a", 20))).unwrap(), 30);
    assert_eq!(block_on(ops.balance(MIDGAR, APP)).unwrap(), 30);
    let row = |column: &str| d1.int(&format!("SELECT {column} FROM wallet_summary WHERE app_id = ?1 AND midgar_id = ?2"), &[APP.into(), MIDGAR.into()]);
    assert_eq!((row("balance"), row("purchased"), row("spent")), (30, 50, 20));
}

#[test]
fn a_replayed_grant_and_a_replayed_spend_change_nothing() {
    let (d1, wallets) = world();
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 50))).unwrap();
    let replay = block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 50))).unwrap();
    assert_eq!((replay.balance, replay.replayed), (50, true));
    block_on(ops.spend(MIDGAR, APP, &charge("chat:a", 20))).unwrap();
    let again = block_on(ops.spend(MIDGAR, APP, &charge("chat:a", 20))).unwrap();
    assert_eq!(again, 30, "a repeat of the reference charges nothing");
    assert_eq!(wallets.balance(MIDGAR, APP), 30);
    assert!(wallets.verified(MIDGAR));
}

#[test]
fn a_key_reused_for_a_different_amount_is_a_conflict() {
    let (d1, wallets) = world();
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 50))).unwrap();
    assert!(matches!(block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 51))), Err(AppError::Conflict(_))));
}

#[test]
fn an_overdraw_is_a_402_and_writes_nothing() {
    let (d1, wallets) = world();
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 5))).unwrap();
    let denied = block_on(ops.spend(MIDGAR, APP, &charge("chat:big", 6)));
    assert!(matches!(denied, Err(AppError::PaymentRequired(m)) if m == "Insufficient credits. Need 6 credits, have 5"));
    assert_eq!(wallets.balance(MIDGAR, APP), 5);
    let check = block_on(ops.check(MIDGAR, APP, 6));
    assert!(matches!(check, Err(AppError::PaymentRequired(_))));
    assert!(block_on(ops.check(MIDGAR, APP, 5)).is_ok());
}

#[test]
fn spends_and_reads_fail_closed_when_the_wallet_is_down() {
    let d1 = LocalD1::migrated();
    d1.user(APP, "u1", 99);
    let ops = WalletOps { wallets: &Down, db: &d1 };
    assert!(matches!(block_on(ops.balance(MIDGAR, APP)), Err(AppError::WalletUnavailable)));
    assert!(matches!(block_on(ops.check(MIDGAR, APP, 1)), Err(AppError::WalletUnavailable)));
    assert!(matches!(block_on(ops.spend(MIDGAR, APP, &charge("chat:a", 1))), Err(AppError::WalletUnavailable)));
    assert!(matches!(block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 1))), Err(AppError::WalletUnavailable)));
    assert_eq!(d1.d1_balance(APP, "u1"), 99, "there is no fallback to the D1 balance");
    assert_eq!(d1.int("SELECT COUNT(*) FROM credit_transactions", &[]), 0);
}

#[test]
fn a_wallet_that_answers_with_a_server_error_is_unavailable() {
    let d1 = LocalD1::migrated();
    for status in [500, 502, 503] {
        let ops = WalletOps { wallets: &Sick(status), db: &d1 };
        assert!(matches!(block_on(ops.spend(MIDGAR, APP, &charge("chat:a", 1))), Err(AppError::WalletUnavailable)), "{status}");
        assert!(matches!(block_on(ops.balance(MIDGAR, APP)), Err(AppError::WalletUnavailable)), "{status}");
    }
}

#[test]
fn the_unavailable_error_survives_the_trip_through_a_worker_error() {
    let error: worker::Error = AppError::WalletUnavailable.into();
    assert!(is_wallet_unavailable(&error));
    assert!(matches!(AppError::from(error), AppError::WalletUnavailable));
    let other: worker::Error = AppError::InternalError("x".into()).into();
    assert!(!is_wallet_unavailable(&other));
}

#[test]
fn a_failing_mirror_never_fails_the_request() {
    let (d1, wallets) = world();
    d1.run("DROP TABLE wallet_summary", &[]);
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    assert_eq!(block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 8))).unwrap().balance, 8);
    assert_eq!(block_on(ops.spend(MIDGAR, APP, &charge("chat:a", 3))).unwrap(), 5);
}

#[test]
fn a_reversal_takes_at_most_the_balance_once_per_key() {
    let (d1, wallets) = world();
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 10))).unwrap();
    block_on(ops.spend(MIDGAR, APP, &charge("chat:a", 4))).unwrap();
    let reversal = Reversal { key: "reverse:tx:1".into(), kind: Kind::Reversal, source: "revenuecat", amount: 10, description: "Refund", reference: Some("r") };
    assert_eq!(block_on(ops.reverse(MIDGAR, APP, &reversal)).unwrap(), (0, 6));
    assert_eq!(block_on(ops.reverse(MIDGAR, APP, &reversal)).unwrap(), (0, 6), "a repeat reports the same");
    assert_eq!(wallets.balance(MIDGAR, APP), 0);
    assert_eq!(wallets.count("POST /reverse"), 2);
    assert!(wallets.verified(MIDGAR));
}

#[test]
fn an_admin_adjustment_credits_and_debits_under_its_own_key() {
    let (d1, wallets) = world();
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    let credit = Grant { key: "admin:one".into(), kind: Kind::Adjust, source: "admin", amount: 9, description: "Admin", reference: None };
    assert_eq!(block_on(ops.grant(MIDGAR, APP, &credit)).unwrap().balance, 9);
    let debit = Reversal { key: "admin:two".into(), kind: Kind::Adjust, source: "admin", amount: 100, description: "Admin", reference: None };
    assert_eq!(block_on(ops.reverse(MIDGAR, APP, &debit)).unwrap(), (0, 9));
}

#[test]
fn tenants_never_see_each_others_credits() {
    let (d1, wallets) = world();
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 10))).unwrap();
    assert_eq!(block_on(ops.balance(MIDGAR, OTHER_APP)).unwrap(), 0);
    assert!(matches!(block_on(ops.spend(MIDGAR, OTHER_APP, &charge("chat:a", 1))), Err(AppError::PaymentRequired(_))));
    assert_eq!(block_on(ops.balance(MIDGAR, APP)).unwrap(), 10);
}

#[test]
fn a_zero_amount_touches_nothing() {
    let (d1, wallets) = world();
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 10))).unwrap();
    assert_eq!(block_on(ops.spend(MIDGAR, APP, &charge("chat:z", 0))).unwrap(), 10);
    assert_eq!(wallets.count("POST /spend"), 0);
}

#[test]
fn history_lists_entries_newest_first_without_the_claim_markers() {
    let (d1, wallets) = world();
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    wallets.call(MIDGAR, "POST", "/migrate", Some(&json!({ "tenant": APP, "key": "migrate:a:u", "moved": 3, "welcome_part": 0 })));
    block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 10))).unwrap();
    block_on(ops.spend(MIDGAR, APP, &charge("chat:a", 4))).unwrap();
    let (rows, total) = block_on(ops.history(MIDGAR, APP, "u1", 0, 10)).unwrap();
    let types: Vec<&str> = rows.iter().map(|r| r["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["spend", "purchase", "migrate"]);
    assert_eq!(total, 3);
    assert_eq!(rows[0]["amount"], -4);
    assert_eq!(rows[0]["user_id"], "u1");
    assert_eq!(rows[0]["reference_id"], "chat:a");
    let (page, _) = block_on(ops.history(MIDGAR, APP, "u1", 1, 1)).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0]["type"], "purchase");
}

#[test]
fn a_spend_entry_is_found_by_its_reference_and_only_for_spends() {
    let (d1, wallets) = world();
    let ops = WalletOps { wallets: &wallets, db: &d1 };
    block_on(ops.grant(MIDGAR, APP, &purchase("tx:1", 10))).unwrap();
    block_on(ops.spend(MIDGAR, APP, &charge("chat:a", 4))).unwrap();
    let found = block_on(ops.spend_entry(MIDGAR, APP, "spend:chat:a")).unwrap().expect("the spend");
    assert_eq!(found.amount, -4);
    assert!(block_on(ops.spend_entry(MIDGAR, APP, "tx:1")).unwrap().is_none(), "a purchase is not a spend");
    assert!(block_on(ops.spend_entry(MIDGAR, OTHER_APP, "spend:chat:a")).unwrap().is_none(), "another tenant has none");
    assert!(block_on(ops.spend_entry(MIDGAR, APP, "spend:chat:missing")).unwrap().is_none());
}
