use std::cell::Cell;

use futures::executor::block_on;
use serde_json::json;

use super::*;
use crate::ledgers::Account;
use crate::wallet::purchase;
use crate::wallet::testing::{FakeWallets, LocalD1};

const APP: &str = "dreameater";
const BUNDLE: &str = "com.marcusziade.DreamEater";
const PRODUCT: &str = "com.dreameater.credits.small";
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

fn expected() -> Expected<'static> {
    Expected { product_id: PRODUCT, midgar_id: MIDGAR, tenant_slug: "dream-eater", tenant_bundle: Some(BUNDLE), strict: true, production: true }
}

fn refused(transaction: &LedgerTransaction) -> Option<Refusal> {
    check(transaction, &expected()).err()
}

#[test]
fn a_consumable_made_by_the_caller_in_this_tenant_is_accepted() {
    assert_eq!(check(&transaction(), &expected()), Ok(()));
}

#[test]
fn the_token_is_compared_to_the_midgar_id_in_any_letter_case() {
    let upper = LedgerTransaction { app_account_token: Some(MIDGAR.to_uppercase()), ..transaction() };
    assert_eq!(refused(&upper), None);
}

#[test]
fn another_midgar_ids_transaction_is_refused() {
    let theirs = LedgerTransaction { app_account_token: Some(OTHER.to_string()), ..transaction() };
    assert_eq!(refused(&theirs), Some(Refusal::NotYours));
}

#[test]
fn a_transaction_without_an_account_token_is_refused() {
    let tokenless = LedgerTransaction { app_account_token: None, ..transaction() };
    assert_eq!(refused(&tokenless), Some(Refusal::NotYours));
}

#[test]
fn a_bundle_of_another_tenant_is_refused() {
    let foreign = LedgerTransaction { bundle_id: "com.guitaripod.payday".to_string(), registry_slug: Some("pay-day".to_string()), ..transaction() };
    assert_eq!(refused(&foreign), Some(Refusal::WrongApp));
    let unknown = LedgerTransaction { bundle_id: "com.example.stranger".to_string(), registry_slug: None, ..transaction() };
    assert_eq!(refused(&unknown), Some(Refusal::WrongApp));
}

#[test]
fn the_registry_or_the_apps_mapping_alone_is_enough_for_the_bundle() {
    let registry_only = LedgerTransaction { bundle_id: "com.marcusziade.DreamEater.mac".to_string(), ..transaction() };
    assert_eq!(refused(&registry_only), None);
    let mapping_only = LedgerTransaction { registry_slug: None, ..transaction() };
    assert_eq!(refused(&mapping_only), None);
    let no_mapping = Expected { tenant_bundle: None, ..expected() };
    assert_eq!(check(&mapping_only, &no_mapping), Err(Refusal::WrongApp));
}

#[test]
fn a_product_other_than_the_requests_is_refused() {
    let other = LedgerTransaction { product_id: "com.dreameater.credits.giga".to_string(), ..transaction() };
    assert_eq!(refused(&other), Some(Refusal::WrongProduct));
}

#[test]
fn only_a_consumable_is_credited() {
    for kind in ["Non-Consumable", "Auto-Renewable Subscription", "Non-Renewing Subscription"] {
        assert_eq!(refused(&LedgerTransaction { kind: kind.to_string(), ..transaction() }), Some(Refusal::NotConsumable));
    }
}

#[test]
fn a_revoked_transaction_is_refused() {
    assert_eq!(refused(&LedgerTransaction { revocation_date: Some(1_760_000_100_000), ..transaction() }), Some(Refusal::Revoked));
}

#[test]
fn a_family_shared_or_unlabelled_ownership_is_refused() {
    assert_eq!(refused(&LedgerTransaction { in_app_ownership_type: Some("FAMILY_SHARED".to_string()), ..transaction() }), Some(Refusal::NotPurchased));
    assert_eq!(refused(&LedgerTransaction { in_app_ownership_type: None, ..transaction() }), Some(Refusal::NotPurchased));
}

#[test]
fn only_one_pack_per_transaction_is_credited() {
    assert_eq!(refused(&LedgerTransaction { quantity: 2, ..transaction() }), Some(Refusal::Quantity));
    assert_eq!(refused(&LedgerTransaction { quantity: 0, ..transaction() }), Some(Refusal::Quantity));
}

#[test]
fn the_sandbox_rule_is_the_webhooks_rule() {
    let sandbox = LedgerTransaction { environment: "Sandbox".to_string(), ..transaction() };
    assert_eq!(refused(&sandbox), Some(Refusal::Sandbox));
    assert_eq!(check(&sandbox, &Expected { production: false, ..expected() }), Ok(()));
    assert_eq!(check(&sandbox, &Expected { strict: false, ..expected() }), Ok(()));
    assert_eq!(refused(&transaction()), None);
}

#[test]
fn refusals_map_to_codes_a_client_can_tell_apart() {
    let status = |r: Refusal| r.into_error().parts();
    assert_eq!((status(Refusal::NotYours).status, status(Refusal::NotYours).code), (403, "forbidden"));
    assert_eq!((status(Refusal::WrongApp).status, status(Refusal::WrongApp).code), (403, "forbidden"));
    for refusal in [Refusal::WrongProduct, Refusal::NotConsumable, Refusal::Revoked, Refusal::NotPurchased, Refusal::Quantity, Refusal::Sandbox] {
        assert_eq!((status(refusal).status, status(refusal).code), (400, "bad_request"));
    }
}

fn body(overrides: serde_json::Value) -> serde_json::Value {
    let mut base = json!({
        "ok": true, "transactionId": TX, "originalTransactionId": TX, "bundleId": BUNDLE, "productId": PRODUCT, "type": "Consumable",
        "appAccountToken": MIDGAR, "environment": "Production", "purchaseDate": 1_760_000_000_000_i64, "revocationDate": null,
        "inAppOwnershipType": "PURCHASED", "quantity": 1, "registrySlug": "dream-eater"
    });
    for (key, value) in overrides.as_object().unwrap() {
        base[key] = value.clone();
    }
    base
}

#[test]
fn the_ledgers_200_is_the_transaction_and_its_400_is_a_verdict() {
    assert_eq!(classify_answer(200, &body(json!({}))), Ok(transaction()));
    assert_eq!(classify_answer(200, &body(json!({ "revocationDate": 5, "appAccountToken": null }))).unwrap().revocation_date, Some(5));
    assert_eq!(classify_answer(400, &json!({ "error": { "code": "unverifiable_payload" } })), Err(VerifyFailure::Rejected));
}

#[test]
fn every_other_answer_is_the_ledger_being_unavailable() {
    for status in [401, 404, 429, 500, 502, 503] {
        assert!(matches!(classify_answer(status, &json!({})), Err(VerifyFailure::Unavailable(_))), "{status}");
    }
    assert!(matches!(classify_answer(200, &json!({ "ok": true })), Err(VerifyFailure::Unavailable(_))));
}

struct Ledger {
    answer: Result<LedgerTransaction, VerifyFailure>,
    asked: Cell<usize>,
}

impl TransactionVerifier for Ledger {
    async fn verify(&self, _: &str) -> Result<LedgerTransaction, VerifyFailure> {
        self.asked.set(self.asked.get() + 1);
        self.answer.clone()
    }
}

fn vetted(answer: Result<LedgerTransaction, VerifyFailure>) -> Result<LedgerTransaction, AppError> {
    block_on(vet(&Ledger { answer, asked: Cell::new(0) }, "eyJ.signed.jws", &expected()))
}

#[test]
fn a_verified_transaction_passes_vetting() {
    assert_eq!(vetted(Ok(transaction())).unwrap(), transaction());
}

#[test]
fn a_payload_that_does_not_verify_is_unverifiable() {
    let error = vetted(Err(VerifyFailure::Rejected)).unwrap_err();
    assert_eq!(error.parts().code, "unverifiable_purchase");
    assert_eq!(error.parts().status, 400);
    assert!(error.parts().message.contains("could not be verified"));
}

#[test]
fn a_ledger_that_cannot_be_asked_leaves_the_credit_pending() {
    let error = vetted(Err(VerifyFailure::Unavailable("down".to_string()))).unwrap_err();
    assert_eq!((error.parts().status, error.parts().code), (409, "credit_pending"));
    assert_eq!(error.parts().message, PENDING_MESSAGE);
}

#[test]
fn a_wrong_token_or_tenant_or_product_fails_vetting_with_its_own_error() {
    let theirs = vetted(Ok(LedgerTransaction { app_account_token: Some(OTHER.to_string()), ..transaction() })).unwrap_err();
    assert_eq!(theirs.parts().status, 403);
    let foreign = vetted(Ok(LedgerTransaction { bundle_id: "com.x".to_string(), registry_slug: Some("pay-day".to_string()), ..transaction() })).unwrap_err();
    assert_eq!(foreign.parts().status, 403);
    let product = vetted(Ok(LedgerTransaction { product_id: "com.dreameater.credits.giga".to_string(), ..transaction() })).unwrap_err();
    assert_eq!((product.parts().status, product.parts().code), (400, "bad_request"));
}

#[test]
fn a_tenants_bundle_and_pack_come_from_its_apps_row() {
    let d1 = LocalD1::migrated();
    assert_eq!(block_on(tenant_bundle(&d1, APP)).unwrap().as_deref(), Some(BUNDLE));
    assert_eq!(block_on(tenant_bundle(&d1, "nobody")).unwrap(), None);
    assert_eq!(block_on(pack_of_product(&d1, APP, PRODUCT)).unwrap().as_deref(), Some("small"));
    assert_eq!(block_on(pack_of_product(&d1, "pixie", PRODUCT)).unwrap(), None, "another tenant's product is no pack of this one");
    assert_eq!(block_on(pack_of_product(&d1, APP, "com.dreameater.credits.")).unwrap(), None);
    assert_eq!(block_on(pack_of_product(&d1, APP, "com.example.unknown")).unwrap(), None);
}

fn linked_wallet(d1: &LocalD1) -> Account {
    d1.user(APP, "u1", 0);
    d1.run(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, 'u1', ?2, 'linked', '2026-10-01T00:00:00+00:00')",
        &[APP.into(), MIDGAR.into()],
    );
    Account::Wallet { midgar_id: MIDGAR.into(), app_id: APP.into(), user_id: "u1".into() }
}

fn credit(d1: &LocalD1, wallets: &FakeWallets, account: &Account) -> Option<bool> {
    let status = block_on(purchase::revenuecat_status(d1, APP, TX)).unwrap();
    if already_settled(status.as_deref()) {
        return None;
    }
    let id = block_on(purchase::record_pending(d1, APP, account.user_id(), "small", 5, 299, TX)).unwrap();
    let row = block_on(purchase::read(d1, &id)).unwrap().unwrap();
    Some(block_on(purchase::complete_in_wallet(d1, wallets, account, &row)).unwrap())
}

#[test]
fn a_signed_purchase_grants_under_the_transaction_key_and_a_replay_credits_nothing() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    let account = linked_wallet(&d1);
    assert_eq!(credit(&d1, &wallets, &account), Some(true));
    assert_eq!(wallets.balance(MIDGAR, APP), 5);
    let (status, entry) = wallets.call(MIDGAR, "GET", &format!("/entry?tenant={APP}&key=tx:{TX}"), None);
    assert_eq!(status, 200, "the grant is stored under the key the RevenueCat webhook uses");
    assert_eq!(entry["amount"], 5);
    assert_eq!(credit(&d1, &wallets, &account), None);
    assert_eq!(wallets.balance(MIDGAR, APP), 5);
    assert!(wallets.verified(MIDGAR));
}

#[test]
fn the_webhook_after_a_signed_purchase_and_before_it_credit_one_pack_between_them() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    let account = linked_wallet(&d1);
    assert_eq!(credit(&d1, &wallets, &account), Some(true));
    assert_eq!(block_on(purchase::revenuecat_status(&d1, APP, TX)).unwrap().as_deref(), Some("completed"));
    let webhook = block_on(purchase::record_pending(&d1, APP, "u1", "small", 5, 299, TX)).unwrap();
    let row = block_on(purchase::read(&d1, &webhook)).unwrap().unwrap();
    assert!(!block_on(purchase::complete_in_wallet(&d1, &wallets, &account, &row)).unwrap(), "the webhook finds the purchase completed");
    assert_eq!(wallets.balance(MIDGAR, APP), 5);
    assert_eq!(d1.int("SELECT COUNT(*) FROM credit_purchases WHERE payment_id = ?1", &[TX.into()]), 1);
}

#[test]
fn a_signed_purchase_after_the_webhook_is_already_processed() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    let account = linked_wallet(&d1);
    let id = block_on(purchase::record_pending(&d1, APP, "u1", "small", 5, 299, TX)).unwrap();
    let row = block_on(purchase::read(&d1, &id)).unwrap().unwrap();
    assert_eq!(purchase::grant_key(&row), format!("tx:{TX}"));
    assert!(block_on(purchase::complete_in_wallet(&d1, &wallets, &account, &row)).unwrap());
    assert_eq!(credit(&d1, &wallets, &account), None);
    assert_eq!(wallets.balance(MIDGAR, APP), 5);
}

#[test]
fn a_pending_purchase_is_finished_by_the_signed_call_without_a_second_grant() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    let account = linked_wallet(&d1);
    block_on(purchase::record_pending(&d1, APP, "u1", "small", 5, 299, TX)).unwrap();
    assert!(!already_settled(Some("pending")));
    assert_eq!(credit(&d1, &wallets, &account), Some(true));
    assert_eq!(wallets.balance(MIDGAR, APP), 5);
}

#[test]
fn a_purchase_row_in_any_final_status_counts_as_settled() {
    for status in ["completed", "refunded", "failed"] {
        assert!(already_settled(Some(status)), "{status}");
    }
    assert!(!already_settled(None));
    assert!(!already_settled(Some("pending")));
}
