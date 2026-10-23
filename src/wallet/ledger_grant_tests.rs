use futures::executor::block_on;
use serde_json::json;

use super::ledger_grant::{credit, Outcome, Reason};
use super::purchase;
use super::testing::{Budget, FakeWallets, Flaky, LocalD1};
use crate::error::AppError;
use crate::ledgers::Account;
use crate::storekit::{LedgerTransaction, Refusal};

const APP: &str = "dreameater";
const BUNDLE: &str = "com.marcusziade.DreamEater";
const PRODUCT: &str = "com.dreameater.credits.small";
const PACK_CREDITS: i64 = 5;
const MIDGAR: &str = "0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d47";
const OTHER: &str = "0195aaaa-7b1c-4d2e-9a60-3c5e8f1b2d47";
const TX: &str = "2000000123";

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

struct World {
    d1: LocalD1,
    wallets: FakeWallets,
}

impl World {
    fn linked() -> Self {
        let world = Self { d1: LocalD1::migrated(), wallets: FakeWallets::new() };
        world.d1.user(APP, "u1", 0);
        world.link("u1", MIDGAR, "linked");
        world
    }

    fn link(&self, user: &str, midgar: &str, state: &str) {
        self.d1.run(
            "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, ?2, ?3, ?4, '2026-10-01T00:00:00+00:00')",
            &[APP.into(), user.into(), midgar.into(), state.into()],
        );
    }

    fn credit(&self, transaction: &LedgerTransaction) -> Result<Outcome, AppError> {
        block_on(credit(&self.d1, &self.wallets, true, transaction))
    }

    fn entries(&self, key: &str) -> usize {
        let (_, body) = self.wallets.call(MIDGAR, "GET", &format!("/entries?tenant={APP}&limit=200"), None);
        body["entries"].as_array().unwrap().iter().filter(|e| e["key"] == key).count()
    }

    fn purchases(&self) -> i64 {
        self.d1.int("SELECT COUNT(*) FROM credit_purchases", &[])
    }
}

#[test]
fn a_linked_buyers_consumable_is_credited_once_under_the_transaction_key() {
    let world = World::linked();
    let outcome = world.credit(&transaction()).unwrap();
    let Outcome::Credited { app_id, user_id, pack_id, credits, .. } = outcome else { panic!("expected a credit, got {outcome:?}") };
    assert_eq!((app_id.as_str(), user_id.as_str(), pack_id.as_str(), credits as i64), (APP, "u1", "small", PACK_CREDITS));
    assert_eq!(world.wallets.balance(MIDGAR, APP), PACK_CREDITS);
    assert_eq!(world.entries(&format!("tx:{TX}")), 1);
    assert_eq!(block_on(purchase::revenuecat_status(&world.d1, APP, TX)).unwrap().as_deref(), Some("completed"));
    assert!(world.wallets.verified(MIDGAR));
}

#[test]
fn the_same_transaction_replayed_twice_gives_one_wallet_entry() {
    let world = World::linked();
    assert!(matches!(world.credit(&transaction()).unwrap(), Outcome::Credited { .. }));
    for _ in 0..2 {
        assert!(matches!(world.credit(&transaction()).unwrap(), Outcome::Duplicate { .. }));
    }
    assert_eq!(world.entries(&format!("tx:{TX}")), 1);
    assert_eq!(world.wallets.balance(MIDGAR, APP), PACK_CREDITS);
    assert_eq!(world.purchases(), 1);
    assert_eq!(world.wallets.count("POST /grant"), 1, "a replay never reaches the wallet");
}

#[test]
fn a_transaction_the_revenuecat_webhook_credited_first_is_not_credited_again() {
    let world = World::linked();
    let id = block_on(purchase::record_pending(&world.d1, APP, "u1", "small", PACK_CREDITS as u32, 299, TX)).unwrap();
    let row = block_on(purchase::read(&world.d1, &id)).unwrap().unwrap();
    let account = Account::Wallet { midgar_id: MIDGAR.into(), app_id: APP.into(), user_id: "u1".into() };
    assert!(block_on(purchase::complete_in_wallet(&world.d1, &world.wallets, &account, &row)).unwrap());
    assert!(matches!(world.credit(&transaction()).unwrap(), Outcome::Duplicate { purchase_id } if purchase_id == id));
    assert_eq!(world.entries(&format!("tx:{TX}")), 1);
    assert_eq!(world.wallets.balance(MIDGAR, APP), PACK_CREDITS);
}

#[test]
fn the_webhook_after_the_ledger_finds_the_grant_and_adds_nothing() {
    let world = World::linked();
    world.credit(&transaction()).unwrap();
    let id = block_on(purchase::record_pending(&world.d1, APP, "u1", "small", PACK_CREDITS as u32, 299, TX)).unwrap();
    let row = block_on(purchase::read(&world.d1, &id)).unwrap().unwrap();
    assert_eq!(row.status, "completed");
    assert_eq!(world.wallets.balance(MIDGAR, APP), PACK_CREDITS);
    assert_eq!(world.purchases(), 1);
}

#[test]
fn two_transactions_are_two_credits() {
    let world = World::linked();
    world.credit(&transaction()).unwrap();
    let second = LedgerTransaction { transaction_id: "2000000124".to_string(), original_transaction_id: "2000000124".to_string(), ..transaction() };
    assert!(matches!(world.credit(&second).unwrap(), Outcome::Credited { .. }));
    assert_eq!(world.wallets.balance(MIDGAR, APP), 2 * PACK_CREDITS);
    assert_eq!(world.entries("tx:2000000124"), 1);
}

#[test]
fn a_midgar_id_nobody_is_linked_to_is_refused_and_nothing_is_written() {
    let world = World { d1: LocalD1::migrated(), wallets: FakeWallets::new() };
    assert_eq!(world.credit(&transaction()).unwrap(), Outcome::Refused(Reason::NotLinked));
    assert_eq!(world.purchases(), 0);
    assert_eq!(world.wallets.count("POST /grant"), 0);
    world.d1.user(APP, "u2", 0);
    world.link("u2", OTHER, "linked");
    assert_eq!(world.credit(&transaction()).unwrap(), Outcome::Refused(Reason::NotLinked), "another ID's link is not this buyer's");
}

#[test]
fn a_link_still_moving_is_credited_because_the_grant_is_additive() {
    let world = World { d1: LocalD1::migrated(), wallets: FakeWallets::new() };
    world.d1.user(APP, "u1", 0);
    world.link("u1", MIDGAR, "moving");
    assert!(matches!(world.credit(&transaction()).unwrap(), Outcome::Credited { .. }));
    assert_eq!(world.wallets.balance(MIDGAR, APP), PACK_CREDITS);
}

#[test]
fn a_purchase_row_that_belongs_to_an_unlinked_local_user_is_left_to_its_own_path() {
    let world = World::linked();
    world.d1.user(APP, "local", 0);
    world.d1.run(
        "INSERT INTO credit_purchases (id, app_id, user_id, pack_id, credits, amount_usd_cents, payment_provider, payment_id, status, created_at)
         VALUES ('p-local', ?1, 'local', 'small', 5, 299, 'revenuecat', ?2, 'pending', '2026-10-01T00:00:00+00:00')",
        &[APP.into(), TX.into()],
    );
    assert_eq!(world.credit(&transaction()).unwrap(), Outcome::Refused(Reason::Elsewhere));
    assert_eq!(world.wallets.count("POST /grant"), 0);
    assert_eq!(world.purchases(), 1);
}

#[test]
fn a_transaction_without_a_midgar_id_for_a_token_is_refused() {
    let world = World::linked();
    for token in [None, Some("not-a-uuid".to_string()), Some(MIDGAR.replace('-', ""))] {
        let tokenless = LedgerTransaction { app_account_token: token, ..transaction() };
        assert_eq!(world.credit(&tokenless).unwrap(), Outcome::Refused(Reason::NotAMidgarId));
    }
    let shouted = LedgerTransaction { app_account_token: Some(MIDGAR.to_uppercase()), ..transaction() };
    assert!(matches!(world.credit(&shouted).unwrap(), Outcome::Credited { .. }), "a Midgar ID is the same in any letter case");
}

#[test]
fn a_product_no_tenant_sells_or_no_pack_of_the_tenant_is_refused() {
    let world = World::linked();
    let stranger = LedgerTransaction { product_id: "com.example.stranger".to_string(), ..transaction() };
    assert_eq!(world.credit(&stranger).unwrap(), Outcome::Refused(Reason::UnknownProduct));
    let bare = LedgerTransaction { product_id: "com.dreameater.credits.".to_string(), ..transaction() };
    assert_eq!(world.credit(&bare).unwrap(), Outcome::Refused(Reason::UnknownProduct));
    let no_such_pack = LedgerTransaction { product_id: "com.dreameater.credits.colossal".to_string(), ..transaction() };
    assert_eq!(world.credit(&no_such_pack).unwrap(), Outcome::Refused(Reason::UnknownPack));
    assert_eq!(world.purchases(), 0);
}

#[test]
fn the_validate_calls_rules_apply_to_the_forwarded_transaction() {
    let world = World::linked();
    let cases = [
        (LedgerTransaction { bundle_id: "com.guitaripod.payday".to_string(), registry_slug: Some("pay-day".to_string()), ..transaction() }, Refusal::WrongApp),
        (LedgerTransaction { kind: "Non-Consumable".to_string(), ..transaction() }, Refusal::NotConsumable),
        (LedgerTransaction { revocation_date: Some(1_760_000_100_000), ..transaction() }, Refusal::Revoked),
        (LedgerTransaction { in_app_ownership_type: Some("FAMILY_SHARED".to_string()), ..transaction() }, Refusal::NotPurchased),
        (LedgerTransaction { quantity: 2, ..transaction() }, Refusal::Quantity),
        (LedgerTransaction { environment: "Sandbox".to_string(), ..transaction() }, Refusal::Sandbox),
    ];
    for (refused, why) in cases {
        assert_eq!(world.credit(&refused).unwrap(), Outcome::Refused(Reason::Transaction(why)), "{why:?}");
    }
    assert_eq!(world.purchases(), 0);
    assert_eq!(world.wallets.count("POST /grant"), 0);
}

#[test]
fn a_sandbox_purchase_credits_a_tenant_that_is_not_strict_and_off_production() {
    let world = World::linked();
    let sandbox = LedgerTransaction { environment: "Sandbox".to_string(), ..transaction() };
    assert!(matches!(block_on(credit(&world.d1, &world.wallets, false, &sandbox)).unwrap(), Outcome::Credited { .. }));
    let lax = World::linked();
    lax.d1.run("UPDATE apps SET strict_purchases = 0 WHERE app_id = ?1", &[APP.into()]);
    assert!(matches!(lax.credit(&sandbox).unwrap(), Outcome::Credited { .. }));
}

#[test]
fn the_tenants_registry_slug_comes_from_its_row_not_from_a_header() {
    let world = World::linked();
    world.d1.run("UPDATE apps SET apple_app_bundle_id = NULL WHERE app_id = ?1", &[APP.into()]);
    assert!(matches!(world.credit(&transaction()).unwrap(), Outcome::Credited { .. }), "the registry slug alone vouches for the bundle");
    let foreign = LedgerTransaction { registry_slug: Some("pay-day".to_string()), ..transaction() };
    let fresh = World::linked();
    fresh.d1.run("UPDATE apps SET apple_app_bundle_id = NULL WHERE app_id = ?1", &[APP.into()]);
    assert_eq!(fresh.credit(&foreign).unwrap(), Outcome::Refused(Reason::Transaction(Refusal::WrongApp)));
}

#[test]
fn a_crash_after_any_write_loses_and_doubles_nothing() {
    let mut crashes = 0;
    let mut finished = false;
    for ticks in 0..40 {
        let world = World::linked();
        let budget = Budget::of(ticks);
        let (d1, wallets) = (Flaky { inner: &world.d1, budget: &budget }, Flaky { inner: &world.wallets, budget: &budget });
        match block_on(credit(&d1, &wallets, true, &transaction())) {
            Ok(_) => finished = true,
            Err(_) => crashes += 1,
        }
        budget.lift();
        let retried = world.credit(&transaction()).unwrap();
        assert!(matches!(retried, Outcome::Credited { .. } | Outcome::Duplicate { .. }), "ticks {ticks}: {retried:?}");
        assert_eq!(world.entries(&format!("tx:{TX}")), 1, "ticks {ticks}");
        assert_eq!(world.wallets.balance(MIDGAR, APP), PACK_CREDITS, "ticks {ticks}");
        assert_eq!(world.purchases(), 1, "ticks {ticks}");
        assert_eq!(block_on(purchase::revenuecat_status(&world.d1, APP, TX)).unwrap().as_deref(), Some("completed"), "ticks {ticks}");
        assert!(world.wallets.verified(MIDGAR));
        if finished {
            break;
        }
    }
    assert!(crashes >= 3, "the injector walked through the writes ({crashes})");
    assert!(finished);
}

#[test]
fn an_answer_lost_after_the_wallet_granted_is_finished_by_the_retry_without_a_second_grant() {
    let world = World::linked();
    let budget = Budget::of(4);
    let (d1, wallets) = (Flaky { inner: &world.d1, budget: &budget }, Flaky { inner: &world.wallets, budget: &budget });
    let first = block_on(credit(&d1, &wallets, true, &transaction()));
    assert!(first.is_err());
    budget.lift();
    world.credit(&transaction()).unwrap();
    assert_eq!(world.wallets.balance(MIDGAR, APP), PACK_CREDITS);
    assert_eq!(world.entries(&format!("tx:{TX}")), 1);
}

#[test]
fn the_answer_body_the_ledger_forwards_deserialises_into_a_transaction() {
    let body = json!({
        "ok": true, "transactionId": TX, "originalTransactionId": TX, "bundleId": BUNDLE, "productId": PRODUCT, "type": "Consumable",
        "appAccountToken": MIDGAR, "environment": "Production", "purchaseDate": 1_760_000_000_000_i64, "revocationDate": null,
        "inAppOwnershipType": "PURCHASED", "quantity": 1, "registrySlug": "dream-eater"
    });
    let parsed: LedgerTransaction = serde_json::from_value(body).unwrap();
    assert_eq!(parsed, transaction());
}
