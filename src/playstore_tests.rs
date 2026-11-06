use std::cell::RefCell;
use std::collections::VecDeque;

use futures::executor::block_on;
use serde_json::json;

use super::*;
use crate::wallet::pack_credit::{credit_pack, Credit};
use crate::wallet::pack_refund;
use crate::wallet::purchase;
use crate::wallet::testing::{FakeWallets, LocalD1, TestLedger};

const PACKAGE: &str = "com.guitaripod.pixie";
const PRODUCT: &str = "com.guitaripod.pixie.credits.starter";
const TOKEN: &str = "opaque-token.AO-J1Oz_example";
const USER: &str = "6f1c2a9e-1b7d-4c55-9a0e-2f4b8d3c7e11";
const ORDER: &str = "GPA.3301-2468-1357-90123";
const EMAIL: &str = "play-verifier@midgar.iam.gserviceaccount.com";
const KEY_DER: &[u8] = b"not a real pkcs8 key, only bytes for the stand-in signer";
const NOW: u64 = 1_790_000_000;

fn pem() -> String {
    let body = STANDARD.encode(KEY_DER);
    let lines: Vec<&str> = body.as_bytes().chunks(64).map(|line| std::str::from_utf8(line).unwrap()).collect();
    format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", lines.join("\n"))
}

fn key_file(overrides: Value) -> String {
    let mut file = json!({
        "type": "service_account",
        "project_id": "midgar-play",
        "private_key_id": "kid-1",
        "private_key": pem(),
        "client_email": EMAIL,
        "client_id": "1234567890",
        "token_uri": "https://oauth2.googleapis.com/token",
    });
    for (field, value) in overrides.as_object().unwrap() {
        file[field] = value.clone();
    }
    file.to_string()
}

fn account() -> ServiceAccount {
    ServiceAccount::parse(&key_file(json!({}))).unwrap()
}

/// Answers calls from a script, in order, and records every call.
#[derive(Default)]
struct Script {
    answers: RefCell<VecDeque<Result<Answer, String>>>,
    calls: RefCell<Vec<Call>>,
}

impl Script {
    fn new(answers: Vec<Result<Answer, String>>) -> Self {
        Self { answers: RefCell::new(answers.into()), calls: RefCell::new(Vec::new()) }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    fn then(&self, answer: Result<Answer, String>) {
        self.answers.borrow_mut().push_back(answer);
    }
}

impl Transport for Script {
    async fn send(&self, call: &Call) -> Result<Answer, String> {
        self.calls.borrow_mut().push(call.clone());
        self.answers.borrow_mut().pop_front().unwrap_or_else(|| Err("nothing scripted".to_string()))
    }
}

/// Signs with a fixed signature and records what it signed.
#[derive(Default)]
struct StandIn {
    inputs: RefCell<Vec<Vec<u8>>>,
    broken: bool,
}

impl Signer for StandIn {
    async fn sign(&self, pkcs8: &[u8], input: &[u8]) -> Result<Vec<u8>, String> {
        assert_eq!(pkcs8, KEY_DER, "the signer gets the DER inside the key file's PEM");
        self.inputs.borrow_mut().push(input.to_vec());
        if self.broken {
            return Err("the key does not import".to_string());
        }
        Ok(b"signature".to_vec())
    }
}

fn answer(status: u16, body: Value) -> Result<Answer, String> {
    Ok(Answer { status, body })
}

fn token_answer(token: &str) -> Result<Answer, String> {
    answer(200, json!({ "access_token": token, "expires_in": 3599, "token_type": "Bearer" }))
}

fn bought() -> Value {
    json!({
        "kind": "androidpublisher#productPurchase",
        "purchaseTimeMillis": "1790000000000",
        "purchaseState": 0,
        "consumptionState": 0,
        "orderId": ORDER,
        "acknowledgementState": 0,
        "productId": PRODUCT,
        "regionCode": "FI",
        "obfuscatedExternalAccountId": USER,
    })
}

fn purchase() -> ProductPurchase {
    serde_json::from_value(bought()).unwrap()
}

struct World {
    account: ServiceAccount,
    script: Script,
    signer: StandIn,
    tokens: RefCell<TokenCache>,
}

impl World {
    fn new(answers: Vec<Result<Answer, String>>) -> Self {
        Self { account: account(), script: Script::new(answers), signer: StandIn::default(), tokens: RefCell::new(TokenCache::new()) }
    }

    fn api(&self, now: u64) -> PlayApi<'_, Script, StandIn> {
        PlayApi { account: &self.account, transport: &self.script, signer: &self.signer, tokens: &self.tokens, now }
    }

    fn lookup(&self, now: u64) -> Result<ProductPurchase, VerifyFailure> {
        block_on(self.api(now).product_purchase(PACKAGE, PRODUCT, TOKEN))
    }

    fn vet(&self, token: &str, expected: &Expected<'_>) -> Result<ProductPurchase, AppError> {
        block_on(vet(&self.api(NOW), PACKAGE, token, expected))
    }
}

fn expected() -> Expected<'static> {
    Expected { product_id: PRODUCT, user_id: USER, order_id: Some(ORDER) }
}

fn decode(segment: &str) -> Value {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segment).unwrap()).unwrap()
}

fn purchase_url_of(token: &str) -> String {
    format!("https://androidpublisher.googleapis.com/androidpublisher/v3/applications/{PACKAGE}/purchases/products/{PRODUCT}/tokens/{token}")
}

#[test]
fn a_key_file_parses_into_the_parts_that_sign_in() {
    let account = account();
    assert_eq!(account.client_email, EMAIL);
    assert_eq!(account.key_id.as_deref(), Some("kid-1"));
    assert_eq!(account.token_uri, "https://oauth2.googleapis.com/token");
    assert_eq!(account.private_key, KEY_DER);
}

#[test]
fn a_key_file_without_a_token_uri_or_key_id_signs_in_at_google() {
    let account = ServiceAccount::parse(&key_file(json!({ "token_uri": null, "private_key_id": "" }))).unwrap();
    assert_eq!(account.token_uri, "https://oauth2.googleapis.com/token");
    assert_eq!(account.key_id, None);
}

#[test]
fn anything_but_a_service_account_key_file_is_no_account() {
    let pkcs1 = pem().replace("BEGIN PRIVATE KEY", "BEGIN RSA PRIVATE KEY").replace("END PRIVATE KEY", "END RSA PRIVATE KEY");
    let cases = [
        String::new(),
        "{}".to_string(),
        "not json".to_string(),
        key_file(json!({ "private_key": null })),
        key_file(json!({ "client_email": "  " })),
        key_file(json!({ "private_key": pkcs1 })),
        key_file(json!({ "private_key": "-----BEGIN PRIVATE KEY-----\n!!!\n-----END PRIVATE KEY-----" })),
        key_file(json!({ "private_key": "-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----" })),
    ];
    for case in cases {
        assert!(ServiceAccount::parse(&case).is_none(), "{case}");
    }
}

#[test]
fn the_assertion_is_an_rs256_jwt_for_the_play_scope_signed_over_its_first_two_segments() {
    let world = World::new(vec![]);
    let assertion = block_on(world.api(NOW).assertion()).unwrap();
    let parts: Vec<&str> = assertion.split('.').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(decode(parts[0]), json!({ "alg": "RS256", "typ": "JWT", "kid": "kid-1" }));
    assert_eq!(
        decode(parts[1]),
        json!({
            "iss": EMAIL,
            "scope": "https://www.googleapis.com/auth/androidpublisher",
            "aud": "https://oauth2.googleapis.com/token",
            "iat": NOW,
            "exp": NOW + 3600,
        })
    );
    assert_eq!(URL_SAFE_NO_PAD.decode(parts[2]).unwrap(), b"signature");
    assert_eq!(world.signer.inputs.borrow().as_slice(), [format!("{}.{}", parts[0], parts[1]).into_bytes()]);
}

#[test]
fn a_purchase_is_read_with_the_access_token_the_signed_assertion_bought() {
    let world = World::new(vec![token_answer("ya29.first"), answer(200, bought())]);
    assert_eq!(world.lookup(NOW).unwrap(), purchase());
    let calls = world.script.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!((calls[0].verb, calls[0].url.as_str()), (Verb::Post, "https://oauth2.googleapis.com/token"));
    assert_eq!(calls[0].content_type, Some("application/x-www-form-urlencoded"));
    assert_eq!(calls[0].bearer, None);
    let form = calls[0].body.as_deref().unwrap();
    assert!(form.starts_with("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion="), "{form}");
    assert_eq!(form.matches('.').count(), 2, "the assertion's dots are unreserved and travel as they are");
    assert_eq!((calls[1].verb, calls[1].url.clone()), (Verb::Get, purchase_url_of(TOKEN)));
    assert_eq!(calls[1].bearer.as_deref(), Some("ya29.first"));
}

#[test]
fn the_access_token_is_reused_until_five_minutes_before_it_expires() {
    let world = World::new(vec![token_answer("ya29.first"), answer(200, bought()), answer(200, bought())]);
    world.lookup(NOW).unwrap();
    world.lookup(NOW + 3599 - 301).unwrap();
    assert_eq!(world.script.calls().len(), 3, "one sign-in for both lookups");
    assert_eq!(world.signer.inputs.borrow().len(), 1);

    world.script.then(token_answer("ya29.second"));
    world.script.then(answer(200, bought()));
    world.lookup(NOW + 3599 - 300).unwrap();
    let calls = world.script.calls();
    assert_eq!(calls.len(), 5);
    assert_eq!(calls[4].bearer.as_deref(), Some("ya29.second"));
}

#[test]
fn a_token_answer_without_a_lifetime_is_kept_for_an_hour() {
    let world = World::new(vec![answer(200, json!({ "access_token": "ya29.plain" })), answer(200, bought()), answer(200, bought())]);
    world.lookup(NOW).unwrap();
    world.lookup(NOW + 3600 - 301).unwrap();
    assert_eq!(world.script.calls().len(), 3);
}

#[test]
fn another_service_account_signs_in_for_itself() {
    let world = World::new(vec![token_answer("ya29.first"), answer(200, bought())]);
    world.lookup(NOW).unwrap();
    let other = ServiceAccount::parse(&key_file(json!({ "client_email": "rotated@midgar.iam.gserviceaccount.com" }))).unwrap();
    world.script.then(token_answer("ya29.rotated"));
    world.script.then(answer(200, bought()));
    let api = PlayApi { account: &other, transport: &world.script, signer: &world.signer, tokens: &world.tokens, now: NOW };
    block_on(api.product_purchase(PACKAGE, PRODUCT, TOKEN)).unwrap();
    let calls = world.script.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[3].bearer.as_deref(), Some("ya29.rotated"));
}

#[test]
fn an_access_token_google_refuses_is_replaced_once() {
    let world = World::new(vec![token_answer("ya29.stale"), answer(401, json!({})), token_answer("ya29.fresh"), answer(200, bought())]);
    assert_eq!(world.lookup(NOW).unwrap(), purchase());
    let calls = world.script.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[3].bearer.as_deref(), Some("ya29.fresh"));

    world.script.then(answer(200, bought()));
    world.lookup(NOW + 60).unwrap();
    assert_eq!(world.script.calls()[4].bearer.as_deref(), Some("ya29.fresh"), "the fresh token is the cached one now");
}

#[test]
fn a_second_refusal_is_google_being_unavailable_not_a_verdict() {
    let world = World::new(vec![token_answer("a"), answer(401, json!({})), token_answer("b"), answer(401, json!({}))]);
    assert!(matches!(world.lookup(NOW), Err(VerifyFailure::Unavailable(_))));
    assert_eq!(world.script.calls().len(), 4, "only one retry");
}

#[test]
fn a_token_google_does_not_know_is_rejected() {
    for status in [400, 404, 410] {
        let world = World::new(vec![token_answer("a"), answer(status, json!({ "error": { "code": status, "status": "INVALID_ARGUMENT" } }))]);
        assert_eq!(world.lookup(NOW), Err(VerifyFailure::Rejected), "{status}");
    }
}

#[test]
fn anything_else_google_answers_is_unavailable() {
    for status in [403, 429, 500, 503] {
        let world = World::new(vec![token_answer("a"), answer(status, json!({ "error": { "status": "PERMISSION_DENIED" } }))]);
        match world.lookup(NOW) {
            Err(VerifyFailure::Unavailable(reason)) => assert!(reason.contains(&status.to_string()) && reason.contains("PERMISSION_DENIED"), "{reason}"),
            other => panic!("{status}: {other:?}"),
        }
    }
    let unreachable = World::new(vec![token_answer("a"), Err("connection reset".to_string())]);
    assert!(matches!(unreachable.lookup(NOW), Err(VerifyFailure::Unavailable(reason)) if reason.contains("connection reset")));
    let unreadable = World::new(vec![token_answer("a"), answer(200, json!("not a purchase"))]);
    assert!(matches!(unreadable.lookup(NOW), Err(VerifyFailure::Unavailable(_))));
}

#[test]
fn a_refused_sign_in_is_unavailable_and_leaves_nothing_cached() {
    let world = World::new(vec![answer(400, json!({ "error": "invalid_grant", "error_description": "Invalid JWT Signature." }))]);
    match world.lookup(NOW) {
        Err(VerifyFailure::Unavailable(reason)) => assert!(reason.contains("400") && reason.contains("invalid_grant"), "{reason}"),
        other => panic!("{other:?}"),
    }
    world.script.then(answer(200, json!({ "token_type": "Bearer" })));
    assert!(matches!(world.lookup(NOW), Err(VerifyFailure::Unavailable(_))), "a 200 without a token is no token");
    world.script.then(token_answer("ya29.late"));
    world.script.then(answer(200, bought()));
    world.lookup(NOW).unwrap();
    assert_eq!(world.script.calls().len(), 4, "every lookup signed in again until one worked");
}

#[test]
fn a_key_that_cannot_sign_is_unavailable_and_asks_google_nothing() {
    let world = World { signer: StandIn { broken: true, ..StandIn::default() }, ..World::new(vec![]) };
    assert!(matches!(world.lookup(NOW), Err(VerifyFailure::Unavailable(reason)) if reason.contains("could not sign")));
    assert!(world.script.calls().is_empty());
}

#[test]
fn the_acknowledgement_posts_to_the_purchase() {
    let world = World::new(vec![token_answer("a"), answer(200, json!({})), answer(204, Value::Null), answer(500, json!({}))]);
    block_on(world.api(NOW).acknowledge(PACKAGE, PRODUCT, TOKEN)).unwrap();
    block_on(world.api(NOW).acknowledge(PACKAGE, PRODUCT, TOKEN)).unwrap();
    assert!(matches!(block_on(world.api(NOW).acknowledge(PACKAGE, PRODUCT, TOKEN)), Err(VerifyFailure::Unavailable(_))));
    let calls = world.script.calls();
    assert_eq!((calls[1].verb, calls[1].url.clone()), (Verb::Post, format!("{}:acknowledge", purchase_url_of(TOKEN))));
    assert_eq!(calls[1].bearer.as_deref(), Some("a"));
    assert_eq!(calls[1].body.as_deref(), Some("{}"));
}

#[test]
fn path_segments_and_form_values_are_percent_encoded() {
    assert_eq!(encode("a b/c?d#e&f=g%"), "a%20b%2Fc%3Fd%23e%26f%3Dg%25");
    assert_eq!(encode("Az09-._~"), "Az09-._~");
    assert_eq!(encode("ä"), "%C3%A4");
    assert!(purchase_url("p", "x/y", "t").ends_with("/applications/p/purchases/products/x%2Fy/tokens/t"));
}

#[test]
fn only_a_token_shaped_like_plays_is_sent_to_google() {
    assert!(plausible_token(TOKEN));
    assert!(plausible_token(&"a".repeat(4096)));
    for token in ["", ".", "..", "-._", "a/b", "a?b", "a b", "a%2Fb", "ä"] {
        assert!(!plausible_token(token), "{token:?}");
    }
    assert!(!plausible_token(&"a".repeat(4097)));
}

#[test]
fn a_paid_pack_bought_by_the_caller_is_accepted() {
    assert_eq!(check(&purchase(), &expected()), Ok(()));
}

#[test]
fn a_purchase_still_being_paid_is_pending() {
    let waiting = ProductPurchase { purchase_state: Some(2), ..purchase() };
    assert_eq!(check(&waiting, &expected()), Err(Refusal::Pending));
}

#[test]
fn a_canceled_or_refunded_purchase_is_void() {
    for state in [Some(1), Some(7), None] {
        let canceled = ProductPurchase { purchase_state: state, ..purchase() };
        assert_eq!(check(&canceled, &expected()), Err(Refusal::Voided), "{state:?}");
    }
    let refunded = ProductPurchase { refundable_quantity: Some(0), ..purchase() };
    assert_eq!(check(&refunded, &expected()), Err(Refusal::Voided));
    let whole = ProductPurchase { refundable_quantity: Some(1), ..purchase() };
    assert_eq!(check(&whole, &expected()), Ok(()));
}

#[test]
fn another_product_or_more_than_one_pack_is_refused() {
    let other = ProductPurchase { product_id: Some("com.guitaripod.pixie.credits.enterprise".to_string()), ..purchase() };
    assert_eq!(check(&other, &expected()), Err(Refusal::WrongProduct));
    let unnamed = ProductPurchase { product_id: None, ..purchase() };
    assert_eq!(check(&unnamed, &expected()), Ok(()), "Google may leave the product out; the URL already named it");
    let three = ProductPurchase { quantity: Some(3), ..purchase() };
    assert_eq!(check(&three, &expected()), Err(Refusal::Quantity));
    let one = ProductPurchase { quantity: Some(1), ..purchase() };
    assert_eq!(check(&one, &expected()), Ok(()));
}

#[test]
fn a_purchase_the_app_made_for_another_account_is_not_yours() {
    let theirs = ProductPurchase { obfuscated_external_account_id: Some("someone-else".to_string()), ..purchase() };
    assert_eq!(check(&theirs, &expected()), Err(Refusal::NotYours));
    let upper = ProductPurchase { obfuscated_external_account_id: Some(USER.to_uppercase()), ..purchase() };
    assert_eq!(check(&upper, &expected()), Ok(()));
    let unnamed = ProductPurchase { obfuscated_external_account_id: None, ..purchase() };
    assert_eq!(check(&unnamed, &expected()), Ok(()));
}

#[test]
fn the_order_the_app_saw_must_be_googles() {
    let claimed = Expected { order_id: Some("GPA.0000-0000-0000-00000"), ..expected() };
    assert_eq!(check(&purchase(), &claimed), Err(Refusal::WrongOrder));
    for order_id in [None, Some("")] {
        assert_eq!(check(&purchase(), &Expected { order_id, ..expected() }), Ok(()));
    }
    let promo = ProductPurchase { order_id: None, purchase_type: Some(1), ..purchase() };
    assert_eq!(check(&promo, &expected()), Ok(()));
}

#[test]
fn refusals_answer_with_the_credit_codes_the_app_reads() {
    let cases = [
        (Refusal::Pending, 409, "credit_pending"),
        (Refusal::Voided, 400, "refunded"),
        (Refusal::WrongProduct, 400, "bad_request"),
        (Refusal::WrongOrder, 400, "bad_request"),
        (Refusal::NotYours, 403, "forbidden"),
        (Refusal::Quantity, 400, "bad_request"),
    ];
    for (refusal, status, code) in cases {
        let parts = refusal.into_error().parts();
        assert_eq!((parts.status, parts.code), (status, code), "{refusal:?}");
    }
}

#[test]
fn a_purchase_is_keyed_by_its_order_or_else_by_a_hash_of_its_token() {
    assert_eq!(payment_id(&purchase(), TOKEN), format!("gp:{ORDER}"));
    let promo = ProductPurchase { order_id: None, ..purchase() };
    let keyed = payment_id(&promo, TOKEN);
    assert!(keyed.starts_with("gp:token:") && keyed.len() == "gp:token:".len() + 64, "{keyed}");
    assert!(!keyed.contains(TOKEN));
    assert_eq!(keyed, payment_id(&ProductPurchase { order_id: Some("  ".to_string()), ..purchase() }, TOKEN));
    assert_ne!(keyed, payment_id(&promo, "another-token"));
}

#[test]
fn vetting_turns_googles_answer_into_the_credit_codes() {
    let world = World::new(vec![token_answer("a"), answer(200, bought())]);
    assert_eq!(world.vet(TOKEN, &expected()).unwrap(), purchase());

    let unknown = World::new(vec![token_answer("a"), answer(400, json!({}))]);
    let error = unknown.vet(TOKEN, &expected()).unwrap_err().parts();
    assert_eq!((error.status, error.code), (400, "unverifiable_purchase"));
    assert!(error.message.contains("could not be verified"));

    let down = World::new(vec![token_answer("a"), answer(503, json!({}))]);
    let error = down.vet(TOKEN, &expected()).unwrap_err().parts();
    assert_eq!((error.status, error.code, error.message.as_str()), (409, "credit_pending", PENDING_MESSAGE));

    let canceled = World::new(vec![token_answer("a"), answer(200, json!({ "purchaseState": 1, "orderId": ORDER }))]);
    assert_eq!(canceled.vet(TOKEN, &expected()).unwrap_err().parts().code, "refunded");
}

#[test]
fn a_token_that_cannot_be_a_purchase_is_unverifiable_without_asking_google() {
    let world = World::new(vec![]);
    assert_eq!(world.vet("../../edits", &expected()).unwrap_err().parts().code, "unverifiable_purchase");
    assert!(world.script.calls().is_empty());
}

#[test]
fn pixiepocket_is_the_tenant_with_a_play_package() {
    let d1 = LocalD1::migrated();
    assert_eq!(block_on(package_of(&d1, "pixie")).unwrap().as_deref(), Some(PACKAGE));
    assert_eq!(block_on(package_of(&d1, "dreameater")).unwrap(), None);
    assert_eq!(block_on(package_of(&d1, "nobody")).unwrap(), None);
    assert_eq!(block_on(crate::storekit::pack_of_product(&d1, "pixie", PRODUCT)).unwrap().as_deref(), Some("starter"));
}

#[test]
fn a_play_purchase_row_is_a_store_pack_of_its_own_provider_and_grant_key() {
    let d1 = LocalD1::migrated();
    d1.user("pixie", "u1", 0);
    let play = format!("gp:{ORDER}");
    let id = block_on(purchase::record_pending(&d1, "pixie", "u1", "starter", 150, 299, &play)).unwrap();
    let row = block_on(purchase::read(&d1, &id)).unwrap().unwrap();
    assert_eq!(row.provider, purchase::PLAY_STORE);
    assert_eq!(purchase::grant_key(&row), format!("tx:gp:{ORDER}"));
    assert_eq!(block_on(purchase::store_status(&d1, "pixie", &play)).unwrap().as_deref(), Some("pending"));
    assert_eq!(block_on(purchase::record_pending(&d1, "pixie", "u1", "starter", 150, 299, &play)).unwrap(), id, "one row per order");

    let apple = block_on(purchase::record_pending(&d1, "pixie", "u1", "starter", 150, 299, "2000000999")).unwrap();
    assert_eq!(block_on(purchase::read(&d1, &apple)).unwrap().unwrap().provider, purchase::APP_STORE);
    assert_eq!(purchase::store_provider("gp:token:abc"), purchase::PLAY_STORE);
    assert_eq!(purchase::store_provider("2000000999"), purchase::APP_STORE);

    d1.run("UPDATE credit_purchases SET created_at = '2026-01-01T00:00:00+00:00'", &[]);
    let stale = block_on(purchase::stale_store_purchases(&d1, 50)).unwrap();
    assert!(stale.contains(&id) && stale.contains(&apple), "the cron finishes Play rows too");
}

#[test]
fn a_play_pack_credits_the_local_balance_once_and_never_after_its_refund_claim() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    let ledger = TestLedger { db: &d1, wallets: &wallets };
    d1.user("pixie", "u1", 10);
    let play = format!("gp:{ORDER}");
    let credit = || block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, &play)).unwrap();

    assert!(matches!(credit(), Credit::Credited { .. }));
    assert_eq!(d1.int("SELECT balance FROM user_credits WHERE app_id = 'pixie' AND user_id = 'u1'", &[]), 160);
    assert_eq!(
        d1.int("SELECT COUNT(*) FROM credit_transactions WHERE app_id = 'pixie' AND description = 'Google Play purchase: starter pack'", &[]),
        1
    );
    assert_eq!(credit(), Credit::Duplicate);
    assert_eq!(d1.int("SELECT balance FROM user_credits WHERE app_id = 'pixie' AND user_id = 'u1'", &[]), 160);

    let refunded = "gp:GPA.refunded".to_string();
    assert!(block_on(pack_refund::claim(&d1, "pixie", &refunded, "u1", 150)).unwrap());
    let after = block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, &refunded)).unwrap();
    assert_eq!(after, Credit::Refunded);
    assert_eq!(d1.int("SELECT balance FROM user_credits WHERE app_id = 'pixie' AND user_id = 'u1'", &[]), 160);
}
