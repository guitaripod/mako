use futures::executor::block_on;
use serde_json::json;

use super::ledger_grant::{self, Reason as GrantReason};
use super::ledger_refund::{reverse, Outcome, Reason};
use super::pack_credit::refund_pack;
use super::pack_refund::Settlement;
use super::testing::{Budget, FakeWallets, Flaky, LocalD1, WalletLedger};
use super::{pack_refund, purchase};
use crate::error::AppError;
use crate::storekit::{LedgerTransaction, REFUNDED_MESSAGE};

const APP: &str = "dreameater";
const BUNDLE: &str = "com.marcusziade.DreamEater";
const PRODUCT: &str = "com.dreameater.credits.small";
const PACK_CREDITS: i64 = 5;
const MIDGAR: &str = "0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d47";
const TX: &str = "2000000123";
const SECOND_TX: &str = "2000000124";

fn transaction() -> LedgerTransaction {
    LedgerTransaction {
        transaction_id: TX.to_string(),
        original_transaction_id: TX.to_string(),
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
    LedgerTransaction { transaction_id: id.to_string(), original_transaction_id: id.to_string(), revocation_date: Some(1_760_000_100_000), ..transaction() }
}

struct World {
    d1: LocalD1,
    wallets: FakeWallets,
}

impl World {
    fn linked() -> Self {
        let world = Self { d1: LocalD1::migrated(), wallets: FakeWallets::new() };
        world.d1.user(APP, "u1", 0);
        world.d1.run(
            "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, 'u1', ?2, 'linked', '2026-10-01T00:00:00+00:00')",
            &[APP.into(), MIDGAR.into()],
        );
        world
    }

    fn bought(&self, id: &str) {
        let paid = LedgerTransaction { transaction_id: id.to_string(), original_transaction_id: id.to_string(), ..transaction() };
        let outcome = block_on(ledger_grant::credit(&self.d1, &self.wallets, true, &paid)).unwrap();
        assert!(matches!(outcome, ledger_grant::Outcome::Credited { .. }), "{outcome:?}");
    }

    fn refund(&self, transaction: &LedgerTransaction) -> Result<Outcome, AppError> {
        block_on(reverse(&self.d1, &self.wallets, transaction))
    }

    fn spend(&self, amount: i64, key: &str) {
        let body = json!({"key": key, "tenant": APP, "kind": "spend", "amount": -amount, "source": "usage", "description": "Chat"});
        assert_eq!(self.wallets.call(MIDGAR, "POST", "/spend", Some(&body)).0, 200);
    }

    fn balance(&self) -> i64 {
        self.wallets.balance(MIDGAR, APP)
    }

    fn reversals(&self, id: &str) -> usize {
        let (_, body) = self.wallets.call(MIDGAR, "GET", &format!("/entries?tenant={APP}&limit=200"), None);
        body["entries"].as_array().unwrap().iter().filter(|e| e["key"] == pack_refund::reversal_key(id)).count()
    }

    fn claims(&self) -> i64 {
        self.d1.int("SELECT COUNT(*) FROM purchase_refunds", &[])
    }

    fn status(&self, id: &str) -> Option<String> {
        block_on(purchase::revenuecat_status(&self.d1, APP, id)).unwrap()
    }

    fn d1_status(&self, id: &str) -> String {
        block_on(purchase::read(&self.d1, id)).unwrap().unwrap().status
    }

    fn revenuecat_refund(&self, id: &str) -> bool {
        let ledger = WalletLedger { db: &self.d1, wallets: &self.wallets };
        matches!(block_on(refund_pack(&self.d1, &ledger, APP, "small", id)).unwrap(), Settlement::Reversed { .. })
    }
}

#[test]
fn a_refund_after_the_ledger_grant_takes_the_pack_back_once() {
    let world = World::linked();
    world.bought(TX);
    assert_eq!(world.balance(), PACK_CREDITS);
    let outcome = world.refund(&refunded(TX)).unwrap();
    let Outcome::Reversed { credits, taken, .. } = outcome else { panic!("expected a reversal, got {outcome:?}") };
    assert_eq!((credits as i64, taken as i64), (PACK_CREDITS, PACK_CREDITS));
    assert_eq!(world.balance(), 0);
    assert_eq!(world.reversals(TX), 1);
    assert_eq!(world.status(TX).as_deref(), Some("refunded"));
    assert_eq!(world.claims(), 1);
    assert!(world.wallets.verified(MIDGAR));
}

#[test]
fn only_the_refunded_transaction_is_reversed() {
    let world = World::linked();
    world.bought(TX);
    world.bought(SECOND_TX);
    world.refund(&refunded(TX)).unwrap();
    assert_eq!(world.balance(), PACK_CREDITS);
    assert_eq!(world.status(SECOND_TX).as_deref(), Some("completed"));
}

#[test]
fn a_repeated_refund_is_a_no_op() {
    let world = World::linked();
    world.bought(TX);
    world.bought(SECOND_TX);
    assert!(matches!(world.refund(&refunded(TX)).unwrap(), Outcome::Reversed { .. }));
    for _ in 0..2 {
        assert!(matches!(world.refund(&refunded(TX)).unwrap(), Outcome::Duplicate { .. }));
    }
    assert_eq!(world.balance(), PACK_CREDITS);
    assert_eq!(world.reversals(TX), 1);
    assert_eq!(world.wallets.count("POST /reverse"), 1, "a repeat never reaches the wallet");
    assert_eq!(world.claims(), 1);
}

#[test]
fn a_refund_after_the_revenuecat_path_reversed_it_is_a_no_op() {
    let world = World::linked();
    world.bought(TX);
    world.bought(SECOND_TX);
    assert!(world.revenuecat_refund(TX));
    assert_eq!(world.balance(), PACK_CREDITS);
    assert!(matches!(world.refund(&refunded(TX)).unwrap(), Outcome::Duplicate { .. }));
    assert_eq!(world.balance(), PACK_CREDITS);
    assert_eq!(world.reversals(TX), 1);
    assert_eq!(world.claims(), 1);
}

#[test]
fn a_refund_the_revenuecat_path_sees_after_the_ledger_reversed_it_is_a_no_op() {
    let world = World::linked();
    world.bought(TX);
    world.bought(SECOND_TX);
    assert!(matches!(world.refund(&refunded(TX)).unwrap(), Outcome::Reversed { .. }));
    assert!(!world.revenuecat_refund(TX), "the claim is already written");
    assert_eq!(world.balance(), PACK_CREDITS);
    assert_eq!(world.reversals(TX), 1);
}

#[test]
fn a_claim_left_by_a_crashed_revenuecat_reversal_is_finished_once_by_the_ledger() {
    let world = World::linked();
    world.bought(TX);
    world.bought(SECOND_TX);
    let purchase = block_on(pack_refund::refundable_purchase(&world.d1, APP, TX)).unwrap().unwrap();
    assert!(block_on(pack_refund::claim(&world.d1, APP, TX, &purchase.user_id, purchase.credits)).unwrap());
    assert!(matches!(world.refund(&refunded(TX)).unwrap(), Outcome::Reversed { taken: 5, .. }));
    assert_eq!(world.balance(), PACK_CREDITS);
    assert_eq!(world.status(TX).as_deref(), Some("refunded"));
    assert!(matches!(world.refund(&refunded(TX)).unwrap(), Outcome::Duplicate { .. }));
    assert_eq!(world.reversals(TX), 1);
}

#[test]
fn a_balance_already_spent_reverses_down_to_zero() {
    let world = World::linked();
    world.bought(TX);
    world.spend(3, "spend:1");
    assert!(matches!(world.refund(&refunded(TX)).unwrap(), Outcome::Reversed { credits: 5, taken: 2, .. }));
    assert_eq!(world.balance(), 0);
    assert!(world.wallets.verified(MIDGAR));

    let spent = World::linked();
    spent.bought(TX);
    spent.spend(PACK_CREDITS, "spend:1");
    assert!(matches!(spent.refund(&refunded(TX)).unwrap(), Outcome::Reversed { credits: 5, taken: 0, .. }));
    assert_eq!(spent.balance(), 0);
    assert_eq!(spent.status(TX).as_deref(), Some("refunded"));
    assert!(matches!(spent.refund(&refunded(TX)).unwrap(), Outcome::Duplicate { .. }));
}

#[test]
fn a_subscription_or_any_product_that_is_not_a_pack_is_refused_and_nothing_changes() {
    let world = World::linked();
    world.bought(TX);
    let subscription = LedgerTransaction { kind: "Auto-Renewable Subscription".to_string(), ..refunded(TX) };
    assert_eq!(world.refund(&subscription).unwrap(), Outcome::Refused(Reason::NotConsumable));
    let premium = LedgerTransaction { product_id: "com.dreameater.premium.monthly".to_string(), kind: "Auto-Renewable Subscription".to_string(), ..refunded(TX) };
    assert_eq!(world.refund(&premium).unwrap(), Outcome::Refused(Reason::UnknownProduct));
    let stranger = LedgerTransaction { product_id: "com.example.stranger".to_string(), ..refunded(TX) };
    assert_eq!(world.refund(&stranger).unwrap(), Outcome::Refused(Reason::UnknownProduct));
    let bare = LedgerTransaction { product_id: "com.dreameater.credits.".to_string(), ..refunded(TX) };
    assert_eq!(world.refund(&bare).unwrap(), Outcome::Refused(Reason::UnknownProduct));
    let colossal = LedgerTransaction { product_id: "com.dreameater.credits.colossal".to_string(), ..refunded(TX) };
    assert_eq!(world.refund(&colossal).unwrap(), Outcome::Refused(Reason::UnknownPack));
    assert_eq!(world.balance(), PACK_CREDITS);
    assert_eq!(world.claims(), 0);
    assert_eq!(world.wallets.count("POST /reverse"), 0);
    assert_eq!(world.status(TX).as_deref(), Some("completed"));
}

#[test]
fn a_transaction_from_another_app_is_refused() {
    let world = World::linked();
    world.bought(TX);
    let foreign = LedgerTransaction { bundle_id: "com.guitaripod.payday".to_string(), registry_slug: Some("pay-day".to_string()), ..refunded(TX) };
    assert_eq!(world.refund(&foreign).unwrap(), Outcome::Refused(Reason::WrongApp));
    assert_eq!(world.balance(), PACK_CREDITS);
    assert_eq!(world.claims(), 0);
}

#[test]
fn a_refund_of_a_transaction_nothing_has_credited_is_recorded_and_reverses_nothing() {
    let world = World::linked();
    world.bought(SECOND_TX);
    assert_eq!(world.refund(&refunded(TX)).unwrap(), Outcome::Recorded);
    assert_eq!(world.refund(&refunded(TX)).unwrap(), Outcome::Recorded, "a repeat records nothing new");
    assert_eq!(world.claims(), 1);
    assert_eq!(world.balance(), PACK_CREDITS);
    assert_eq!(world.wallets.count("POST /reverse"), 0);
    assert!(block_on(pack_refund::is_claimed(&world.d1, APP, TX)).unwrap());
    assert!(!block_on(pack_refund::is_claimed(&world.d1, APP, SECOND_TX)).unwrap());
}

#[test]
fn a_refund_first_means_the_grant_that_follows_credits_nothing() {
    let world = World::linked();
    assert_eq!(world.refund(&refunded(TX)).unwrap(), Outcome::Recorded);
    let outcome = block_on(ledger_grant::credit(&world.d1, &world.wallets, true, &transaction())).unwrap();
    assert_eq!(outcome, ledger_grant::Outcome::Refused(GrantReason::Refunded));
    assert_eq!(world.balance(), 0);
    assert_eq!(world.status(TX), None);
    assert_eq!(world.wallets.count("POST /grant"), 0);
    assert_eq!(GrantReason::Refunded.as_str(), "refunded");
    let other = LedgerTransaction { transaction_id: SECOND_TX.to_string(), original_transaction_id: SECOND_TX.to_string(), ..transaction() };
    assert!(matches!(block_on(ledger_grant::credit(&world.d1, &world.wallets, true, &other)).unwrap(), ledger_grant::Outcome::Credited { .. }));
}

#[test]
fn a_refund_first_means_the_validate_call_answers_refunded_and_final() {
    let world = World::linked();
    assert!(block_on(pack_refund::refunded_error(&world.d1, APP, TX)).unwrap().is_none());
    assert_eq!(world.refund(&refunded(TX)).unwrap(), Outcome::Recorded);
    let error = block_on(pack_refund::refunded_error(&world.d1, APP, TX)).unwrap().expect("a claimed refund is an error");
    let parts = error.parts();
    assert_eq!((parts.status, parts.code), (400, "refunded"));
    assert_eq!(parts.message, REFUNDED_MESSAGE);
    assert!(!parts.message.contains("already been processed") && !parts.message.contains("could not be verified"));
    assert!(block_on(pack_refund::refunded_error(&world.d1, APP, SECOND_TX)).unwrap().is_none());
}

#[test]
fn a_refund_first_means_the_revenuecat_purchase_event_finds_the_claim() {
    let world = World::linked();
    assert_eq!(world.refund(&refunded(TX)).unwrap(), Outcome::Recorded);
    assert!(block_on(pack_refund::is_claimed(&world.d1, APP, TX)).unwrap(), "credit_pack_from_event stops here");
    assert!(block_on(pack_refund::refundable_purchase(&world.d1, APP, TX)).unwrap().is_none());
    assert_eq!(world.balance(), 0);
}

#[test]
fn a_refund_after_a_pending_purchase_finishes_it_then_reverses_it() {
    let world = World::linked();
    block_on(purchase::record_pending(&world.d1, APP, "u1", "small", PACK_CREDITS as u32, 299, TX)).unwrap();
    world.bought(SECOND_TX);
    assert!(matches!(world.refund(&refunded(TX)).unwrap(), Outcome::Reversed { credits: 5, taken: 5, .. }));
    assert_eq!(world.balance(), PACK_CREDITS);
    assert_eq!(world.status(TX).as_deref(), Some("refunded"));
    assert_eq!(world.reversals(TX), 1);
}

#[test]
fn a_purchase_that_belongs_to_an_unlinked_user_is_left_to_the_revenuecat_path() {
    let world = World::linked();
    world.d1.user(APP, "local", 7);
    world.d1.run(
        "INSERT INTO credit_purchases (id, app_id, user_id, pack_id, credits, amount_usd_cents, payment_provider, payment_id, status, created_at)
         VALUES ('p-local', ?1, 'local', 'small', 5, 299, 'revenuecat', ?2, 'completed', '2026-10-01T00:00:00+00:00')",
        &[APP.into(), TX.into()],
    );
    assert_eq!(world.refund(&refunded(TX)).unwrap(), Outcome::Refused(Reason::NotLinked));
    assert_eq!(world.claims(), 0);
    assert_eq!(world.d1_status("p-local"), "completed");
    assert_eq!(world.d1.d1_balance(APP, "local"), 7);
}

#[test]
fn a_crash_after_any_write_reverses_exactly_once_on_the_retry() {
    let mut crashes = 0;
    let mut finished = false;
    for ticks in 0..40 {
        let world = World::linked();
        world.bought(TX);
        world.bought(SECOND_TX);
        let budget = Budget::of(ticks);
        let (d1, wallets) = (Flaky { inner: &world.d1, budget: &budget }, Flaky { inner: &world.wallets, budget: &budget });
        match block_on(reverse(&d1, &wallets, &refunded(TX))) {
            Ok(_) => finished = true,
            Err(_) => crashes += 1,
        }
        budget.lift();
        let retried = world.refund(&refunded(TX)).unwrap();
        assert!(matches!(retried, Outcome::Reversed { .. } | Outcome::Duplicate { .. }), "ticks {ticks}: {retried:?}");
        assert_eq!(world.reversals(TX), 1, "ticks {ticks}");
        assert_eq!(world.balance(), PACK_CREDITS, "ticks {ticks}");
        assert_eq!(world.status(TX).as_deref(), Some("refunded"), "ticks {ticks}");
        assert_eq!(world.claims(), 1, "ticks {ticks}");
        assert!(world.wallets.verified(MIDGAR));
        if finished {
            break;
        }
    }
    assert!(crashes >= 3, "the injector walked through the writes ({crashes})");
    assert!(finished);
}

#[test]
fn a_purchase_row_is_never_created_for_a_transaction_whose_refund_was_claimed() {
    let world = World::linked();
    assert_eq!(world.refund(&refunded(TX)).unwrap(), Outcome::Recorded);
    let error = block_on(purchase::record_pending(&world.d1, APP, "u1", "small", PACK_CREDITS as u32, 299, TX)).unwrap_err();
    assert!(matches!(error, AppError::Refunded(message) if message == REFUNDED_MESSAGE));
    assert_eq!(world.status(TX), None);
    assert!(block_on(purchase::record_pending(&world.d1, APP, "u1", "small", PACK_CREDITS as u32, 299, SECOND_TX)).is_ok());
}

#[test]
fn a_claim_that_comes_after_the_purchase_row_finds_the_row() {
    let world = World::linked();
    let id = block_on(purchase::record_pending(&world.d1, APP, "u1", "small", PACK_CREDITS as u32, 299, TX)).unwrap();
    assert!(block_on(pack_refund::claim(&world.d1, APP, TX, "", 0)).unwrap());
    assert_eq!(block_on(purchase::record_pending(&world.d1, APP, "u1", "small", PACK_CREDITS as u32, 299, TX)).unwrap(), id, "the row that exists is the one returned");
    assert!(matches!(world.refund(&refunded(TX)).unwrap(), Outcome::Reversed { credits: 5, taken: 5, .. }));
    assert_eq!(world.status(TX).as_deref(), Some("refunded"));
}

#[test]
fn a_credit_that_finishes_after_the_refund_reversed_the_purchase_changes_nothing() {
    let world = World::linked();
    world.bought(SECOND_TX);
    let id = block_on(purchase::record_pending(&world.d1, APP, "u1", "small", PACK_CREDITS as u32, 299, TX)).unwrap();
    let stale = block_on(purchase::read(&world.d1, &id)).unwrap().unwrap();
    assert!(matches!(world.refund(&refunded(TX)).unwrap(), Outcome::Reversed { .. }));
    let account = crate::ledgers::Account::Wallet { midgar_id: MIDGAR.to_string(), app_id: APP.to_string(), user_id: "u1".to_string() };
    let late = block_on(purchase::complete_in_wallet(&world.d1, &world.wallets, &account, &stale)).unwrap_err();
    assert!(matches!(late, AppError::Refunded(_)));
    assert_eq!(world.d1_status(&id), "refunded");
    assert_eq!(world.balance(), PACK_CREDITS);
    assert_eq!(world.reversals(TX), 1);
}
