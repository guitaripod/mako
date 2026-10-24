use futures::executor::block_on;

use super::ops::Reversal;
use super::testing::{Budget, Flaky, LocalD1};
use super::types::Kind;
use super::{pack_refund, purchase};
use crate::error::AppError;

const APP: &str = "dreameater";
const TX: &str = "2000000123";
const CREDITS: i64 = 5;

fn world(balance: i64) -> LocalD1 {
    let d1 = LocalD1::migrated();
    d1.user(APP, "u1", balance);
    d1
}

fn pending(d1: &LocalD1) -> String {
    block_on(purchase::record_pending(d1, APP, "u1", "small", CREDITS as u32, 299, TX)).unwrap()
}

fn reversal<'a>(description: &'a str, reference: &'a str) -> Reversal<'a> {
    Reversal { key: pack_refund::reversal_key(TX), kind: Kind::Reversal, source: "revenuecat", amount: CREDITS as u32, description, reference: Some(reference) }
}

fn reverse(d1: &LocalD1, id: &str) -> Option<u32> {
    block_on(purchase::reverse_locally(d1, APP, "u1", id, &reversal("App Store refund reversed: small pack", "refund:2000000123"))).unwrap()
}

fn status(d1: &LocalD1, id: &str) -> String {
    block_on(purchase::read(d1, id)).unwrap().unwrap().status
}

fn ledger_rows(d1: &LocalD1) -> i64 {
    d1.int("SELECT COUNT(*) FROM credit_transactions", &[])
}

#[test]
fn a_local_credit_adds_the_balance_the_ledger_row_and_the_status_together_once() {
    let d1 = world(2);
    let id = pending(&d1);
    assert!(block_on(purchase::complete_locally(&d1, &id)).unwrap());
    assert!(!block_on(purchase::complete_locally(&d1, &id)).unwrap(), "a repeat finds it completed");
    assert_eq!(d1.d1_balance(APP, "u1"), 2 + CREDITS);
    assert_eq!(d1.int("SELECT lifetime_purchased FROM user_credits", &[]), CREDITS);
    assert_eq!(ledger_rows(&d1), 1);
    assert_eq!(d1.int("SELECT balance_after FROM credit_transactions", &[]), 2 + CREDITS);
    assert_eq!(status(&d1, &id), "completed");
}

#[test]
fn a_local_credit_without_a_balance_row_changes_nothing_and_fails() {
    let d1 = LocalD1::migrated();
    d1.run(
        "INSERT INTO users (id, provider, provider_id, name, created_at, updated_at, app_id) VALUES ('u1', 'anonymous', 'u1', 'T', '2026-10-01T00:00:00+00:00', '2026-10-01T00:00:00+00:00', ?1)",
        &[APP.into()],
    );
    let id = pending(&d1);
    assert!(matches!(block_on(purchase::complete_locally(&d1, &id)), Err(AppError::InternalError(_))));
    assert_eq!(status(&d1, &id), "pending");
    assert_eq!(ledger_rows(&d1), 0);
}

#[test]
fn a_crash_around_the_local_credit_batch_never_leaves_a_status_without_its_credit() {
    let mut crashes = 0;
    for ticks in 0..4 {
        let d1 = world(0);
        let id = pending(&d1);
        let budget = Budget::of(ticks);
        let flaky = Flaky { inner: &d1, budget: &budget };
        if block_on(purchase::complete_locally(&flaky, &id)).is_err() {
            crashes += 1;
        }
        let credited = d1.d1_balance(APP, "u1");
        assert_eq!(status(&d1, &id) == "completed", credited == CREDITS, "ticks {ticks}: status and balance move together");
        assert_eq!(ledger_rows(&d1), credited / CREDITS, "ticks {ticks}");
        budget.lift();
        block_on(purchase::complete_locally(&d1, &id)).unwrap();
        assert_eq!(d1.d1_balance(APP, "u1"), CREDITS, "ticks {ticks}: the retry credits once");
        assert_eq!(ledger_rows(&d1), 1, "ticks {ticks}");
    }
    assert_eq!(crashes, 2);
}

#[test]
fn a_local_reversal_takes_the_pack_back_flips_the_status_and_writes_one_ledger_row() {
    let d1 = world(2);
    let id = pending(&d1);
    block_on(purchase::complete_locally(&d1, &id)).unwrap();
    assert_eq!(reverse(&d1, &id), Some(CREDITS as u32));
    assert_eq!(reverse(&d1, &id), None, "a repeat finds it refunded and changes nothing");
    assert_eq!(d1.d1_balance(APP, "u1"), 2);
    assert_eq!(d1.int("SELECT lifetime_spent FROM user_credits", &[]), CREDITS);
    assert_eq!(status(&d1, &id), "refunded");
    assert_eq!(d1.int("SELECT COUNT(*) FROM credit_transactions WHERE reference_id = 'refund:2000000123' AND amount = -5 AND balance_after = 2", &[]), 1);
}

#[test]
fn a_local_reversal_clamps_to_the_balance_and_never_goes_below_zero() {
    let d1 = world(0);
    let id = pending(&d1);
    block_on(purchase::complete_locally(&d1, &id)).unwrap();
    d1.run("UPDATE user_credits SET balance = 3", &[]);
    assert_eq!(reverse(&d1, &id), Some(3));
    assert_eq!(d1.d1_balance(APP, "u1"), 0);

    let spent = world(0);
    let id = pending(&spent);
    block_on(purchase::complete_locally(&spent, &id)).unwrap();
    spent.run("UPDATE user_credits SET balance = 0", &[]);
    assert_eq!(reverse(&spent, &id), Some(0));
    assert_eq!(status(&spent, &id), "refunded");
    assert_eq!(ledger_rows(&spent), 1, "only the credit wrote a ledger row, a zero reversal writes none");
}

#[test]
fn a_local_reversal_applies_to_a_completed_purchase_only() {
    let d1 = world(2);
    let id = pending(&d1);
    assert_eq!(reverse(&d1, &id), None, "a pending purchase has nothing to take back");
    assert_eq!(status(&d1, &id), "pending");
    assert_eq!(d1.d1_balance(APP, "u1"), 2);
    assert_eq!(ledger_rows(&d1), 0);
    assert!(block_on(purchase::complete_locally(&d1, &id)).unwrap(), "and the credit still happens");
    assert_eq!(d1.d1_balance(APP, "u1"), 2 + CREDITS);
}

#[test]
fn a_local_credit_after_the_refund_reversed_the_purchase_is_refused() {
    let d1 = world(2);
    let id = pending(&d1);
    block_on(purchase::complete_locally(&d1, &id)).unwrap();
    assert_eq!(reverse(&d1, &id), Some(CREDITS as u32));
    assert!(matches!(block_on(purchase::complete_locally(&d1, &id)), Err(AppError::Refunded(_))));
    assert_eq!(d1.d1_balance(APP, "u1"), 2);
}

#[test]
fn a_crash_around_the_local_reversal_batch_never_leaves_a_status_without_its_reversal() {
    let mut crashes = 0;
    for ticks in 0..4 {
        let d1 = world(2);
        let id = pending(&d1);
        block_on(purchase::complete_locally(&d1, &id)).unwrap();
        let budget = Budget::of(ticks);
        let flaky = Flaky { inner: &d1, budget: &budget };
        let outcome = block_on(purchase::reverse_locally(&flaky, APP, "u1", &id, &reversal("App Store refund reversed: small pack", "refund:2000000123")));
        if outcome.is_err() {
            crashes += 1;
        }
        assert_eq!(status(&d1, &id) == "refunded", d1.d1_balance(APP, "u1") == 2, "ticks {ticks}: status and balance move together");
        budget.lift();
        reverse(&d1, &id);
        assert_eq!(d1.d1_balance(APP, "u1"), 2, "ticks {ticks}: the retry reverses once");
        assert_eq!(d1.int("SELECT COUNT(*) FROM credit_transactions WHERE reference_id = 'refund:2000000123'", &[]), 1, "ticks {ticks}");
    }
    assert_eq!(crashes, 2);
}
