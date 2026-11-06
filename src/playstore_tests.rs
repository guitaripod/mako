use std::cell::RefCell;
use std::collections::VecDeque;

use futures::executor::block_on;
use serde_json::json;

use super::voided::{load_state, voided_window, Resume, VoidedPage, VoidedSweep, VoidedWindow};
use super::*;
use crate::wallet::pack_credit::{credit_pack, Credit, PurchaseLedger};
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
    for status in [400, 404, 410] {
        world.script.then(answer(status, json!({})));
        assert_eq!(block_on(world.api(NOW).acknowledge(PACKAGE, PRODUCT, TOKEN)), Err(VerifyFailure::Rejected), "{status}");
    }
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
fn only_a_purchase_through_the_billing_flow_is_charged() {
    assert!(purchase().paid());
    assert_eq!(purchase().charged_cents(299), 299);
    for kind in [0, 1, 2] {
        let free = ProductPurchase { purchase_type: Some(kind), ..purchase() };
        assert!(!free.paid(), "{kind}");
        assert_eq!(free.charged_cents(299), 0, "{kind}");
        assert_eq!(check(&free, &expected()), Ok(()), "a test or promo purchase is still credited");
    }
}

#[test]
fn a_test_purchase_is_credited_at_no_price_and_is_no_sale() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    let ledger = TestLedger { db: &d1, wallets: &wallets };
    d1.user("pixie", "u1", 0);
    let tester = ProductPurchase { purchase_type: Some(0), ..purchase() };
    let credited = block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, tester.charged_cents(299), &payment_id(&tester, TOKEN), None)).unwrap();
    let Credit::Credited { purchase_id } = credited else { panic!("{credited:?}") };
    let row = block_on(purchase::read(&d1, &purchase_id)).unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.amount_usd_cents), ("completed", 0));
    assert!(!purchase::is_sale(&row));
    assert_eq!(d1.int("SELECT balance FROM user_credits WHERE app_id = 'pixie' AND user_id = 'u1'", &[]), 150);
    assert_eq!(d1.int("SELECT COALESCE(SUM(amount_usd_cents), 0) FROM credit_purchases WHERE app_id = 'pixie' AND status = 'completed'", &[]), 0);

    let paid = block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, purchase().charged_cents(299), "gp:GPA.paid", None)).unwrap();
    let Credit::Credited { purchase_id } = paid else { panic!("{paid:?}") };
    assert!(purchase::is_sale(&block_on(purchase::read(&d1, &purchase_id)).unwrap().unwrap()));
}

/// A ledger whose completion always fails, as a wallet outage does: the purchase row stays `pending` for the sweep.
struct DownLedger;

impl PurchaseLedger for DownLedger {
    async fn complete(&self, _purchase_id: &str) -> Result<(), AppError> {
        Err(AppError::WalletUnavailable)
    }
}

fn pixie_tenant() -> PlayTenant {
    PlayTenant { app_id: "pixie".to_string(), package: PACKAGE.to_string(), product_prefix: "com.guitaripod.pixie.credits.".to_string() }
}

fn token_of(d1: &LocalD1, payment_id: &str) -> i64 {
    d1.int("SELECT COUNT(*) FROM credit_purchases WHERE payment_id = ?1 AND store_token IS NOT NULL", &[payment_id.into()])
}

#[test]
fn pixiepocket_is_the_one_play_tenant() {
    let d1 = LocalD1::migrated();
    assert_eq!(block_on(play_tenants(&d1)).unwrap(), [pixie_tenant()]);
}

#[test]
fn a_credit_keeps_the_token_before_it_completes_so_a_swept_purchase_can_be_acknowledged() {
    let d1 = LocalD1::migrated();
    d1.user("pixie", "u1", 0);
    let pending = block_on(credit_pack(&d1, &DownLedger, "pixie", "u1", "starter", 150, 299, "gp:GPA.swept", Some("swept-token")));
    assert!(matches!(pending, Err(AppError::WalletUnavailable)));
    assert_eq!(block_on(purchase::store_status(&d1, "pixie", "gp:GPA.swept")).unwrap().as_deref(), Some("pending"));
    assert_eq!(token_of(&d1, "gp:GPA.swept"), 1);

    let world = World::new(vec![token_answer("a"), answer(200, json!({}))]);
    let nothing = block_on(acknowledge_credited(&world.api(NOW), &d1, &pixie_tenant())).unwrap();
    assert_eq!(nothing, Acknowledgements::default(), "a purchase is acknowledged only once it is credited");
    assert!(world.script.calls().is_empty());

    let id = d1.int("SELECT COUNT(*) FROM credit_purchases WHERE payment_id = 'gp:GPA.swept'", &[]);
    assert_eq!(id, 1);
    let row_id: String = d1.0.query_row("SELECT id FROM credit_purchases WHERE payment_id = 'gp:GPA.swept'", [], |r| r.get(0)).unwrap();
    assert!(block_on(purchase::complete_locally(&d1, &row_id)).unwrap(), "the cron sweep finishes it");

    let report = block_on(acknowledge_credited(&world.api(NOW), &d1, &pixie_tenant())).unwrap();
    assert_eq!(report, Acknowledgements { acknowledged: 1, dropped: 0, kept: 0 });
    let calls = world.script.calls();
    assert_eq!(calls[1].url, format!("{}:acknowledge", purchase_url_of("swept-token")));
    assert_eq!(token_of(&d1, "gp:GPA.swept"), 0, "the token is dropped once Google has the acknowledgement");
}

#[test]
fn an_acknowledgement_google_refuses_drops_the_token_and_one_it_cannot_take_keeps_it() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    let ledger = TestLedger { db: &d1, wallets: &wallets };
    d1.user("pixie", "u1", 0);
    for (order, token) in [("gp:GPA.1", "token-1"), ("gp:GPA.2", "token-2"), ("gp:GPA.3", "token-3")] {
        block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, order, Some(token))).unwrap();
    }
    block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, "gp:GPA.acked", None)).unwrap();
    let world = World::new(vec![token_answer("a"), answer(200, json!({})), answer(400, json!({})), answer(503, json!({}))]);

    let report = block_on(acknowledge_credited(&world.api(NOW), &d1, &pixie_tenant())).unwrap();

    assert_eq!(report, Acknowledgements { acknowledged: 1, dropped: 1, kept: 1 });
    assert_eq!((token_of(&d1, "gp:GPA.1"), token_of(&d1, "gp:GPA.2"), token_of(&d1, "gp:GPA.3")), (0, 0, 1));
    assert_eq!(world.script.calls().len(), 4, "a purchase without a token is never asked about");

    world.script.then(answer(200, json!({})));
    let retried = block_on(acknowledge_credited(&world.api(NOW), &d1, &pixie_tenant())).unwrap();
    assert_eq!(retried, Acknowledgements { acknowledged: 1, dropped: 0, kept: 0 });
    assert_eq!(token_of(&d1, "gp:GPA.3"), 0);
}

#[test]
fn every_refund_drops_the_token_of_the_purchase_it_reverses() {
    let (d1, _) = credited_world(&[]);
    let wallets = FakeWallets::new();
    let ledger = TestLedger { db: &d1, wallets: &wallets };
    block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, "gp:GPA.local", Some("token-local"))).unwrap();
    assert_eq!(token_of(&d1, "gp:GPA.local"), 1);
    let reversed = block_on(crate::wallet::ledger_refund::reverse_play_order(&d1, None::<&FakeWallets>, "pixie", "gp:GPA.local")).unwrap();
    assert!(matches!(reversed, crate::wallet::ledger_refund::Outcome::Reversed { .. }), "{reversed:?}");
    assert_eq!(token_of(&d1, "gp:GPA.local"), 0, "the local flip to refunded drops it");

    d1.run(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES ('pixie', 'u1', ?1, 'linked', '2026-10-01T00:00:00+00:00')",
        &[MIDGAR.into()],
    );
    let ledger = TestLedger { db: &d1, wallets: &wallets };
    block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, "gp:GPA.wallet", Some("token-wallet"))).unwrap();
    assert_eq!(token_of(&d1, "gp:GPA.wallet"), 1);
    block_on(crate::wallet::ledger_refund::reverse_play_order(&d1, Some(&wallets), "pixie", "gp:GPA.wallet")).unwrap();
    assert_eq!(block_on(purchase::store_status(&d1, "pixie", "gp:GPA.wallet")).unwrap().as_deref(), Some("refunded"));
    assert_eq!(token_of(&d1, "gp:GPA.wallet"), 0, "the wallet path's mark_refunded drops it");
}

#[test]
fn a_refunded_purchase_is_not_acknowledged() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    let ledger = TestLedger { db: &d1, wallets: &wallets };
    d1.user("pixie", "u1", 0);
    block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, "gp:GPA.back", Some("token-back"))).unwrap();
    d1.run("UPDATE credit_purchases SET status = 'refunded' WHERE payment_id = 'gp:GPA.back'", &[]);
    assert!(block_on(purchase::unacknowledged_play_purchases(&d1, "pixie", 50)).unwrap().is_empty());
}

#[test]
fn the_token_is_kept_and_dropped_by_payment_id() {
    let d1 = LocalD1::migrated();
    d1.user("pixie", "u1", 0);
    block_on(purchase::record_pending(&d1, "pixie", "u1", "starter", 150, 299, "gp:GPA.k")).unwrap();
    block_on(purchase::remember_store_token(&d1, "pixie", "gp:GPA.k", "kept")).unwrap();
    assert_eq!(token_of(&d1, "gp:GPA.k"), 1);
    block_on(purchase::forget_store_token(&d1, "pixie", "gp:GPA.k")).unwrap();
    assert_eq!(token_of(&d1, "gp:GPA.k"), 0);
}

const DAY_MS: i64 = 24 * 3_600_000;
const NOW_MS: i64 = 1_790_000_000_000;
const MIDGAR: &str = "0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d47";

fn voided(order: &str) -> Value {
    json!({ "kind": "androidpublisher#voidedPurchase", "purchaseToken": format!("token-{order}"), "orderId": order, "voidedTimeMillis": "1789999000000", "voidedSource": 0, "voidedReason": 1 })
}

fn voided_answer(orders: &[&str], next: Option<&str>) -> Result<Answer, String> {
    let mut body = json!({ "voidedPurchases": orders.iter().map(|order| voided(order)).collect::<Vec<_>>() });
    if let Some(next) = next {
        body["tokenPagination"] = json!({ "nextPageToken": next });
    }
    answer(200, body)
}

fn balance(d1: &LocalD1) -> i64 {
    d1.int("SELECT balance FROM user_credits WHERE app_id = 'pixie' AND user_id = 'u1'", &[])
}

fn sweep(world: &World, d1: &LocalD1, wallets: Option<&FakeWallets>, now_ms: i64) -> VoidedSweep {
    block_on(sweep_voided(&world.api(NOW), d1, wallets, &pixie_tenant(), now_ms)).unwrap()
}

fn swept_until(d1: &LocalD1) -> i64 {
    d1.int("SELECT COALESCE(MAX(swept_until_ms), -1) FROM play_voided_sweeps WHERE package_name = ?1", &[PACKAGE.into()])
}

/// A pixie user with 10 credits who bought the starter pack (150 credits) in each of `orders`, credited locally.
fn credited_world(orders: &[&str]) -> (LocalD1, FakeWallets) {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    d1.user("pixie", "u1", 10);
    for order in orders {
        let ledger = TestLedger { db: &d1, wallets: &wallets };
        block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, &format!("gp:{order}"), None)).unwrap();
    }
    (d1, wallets)
}

#[test]
fn the_first_sweep_reads_the_30_days_google_keeps_and_later_ones_overlap_the_last() {
    let first = voided_window(None, NOW_MS);
    assert!(first.start_ms > NOW_MS - 30 * DAY_MS && first.start_ms < NOW_MS - 29 * DAY_MS, "{first:?}");
    assert_eq!(first.end_ms, NOW_MS - 60_000);
    let later = voided_window(Some(NOW_MS - 1_800_000), NOW_MS);
    assert_eq!(later.start_ms, NOW_MS - 1_800_000 - 2 * 3_600_000);
    assert_eq!(voided_window(Some(NOW_MS - 90 * DAY_MS), NOW_MS).start_ms, first.start_ms, "never older than Google lists");
    let future = voided_window(Some(NOW_MS + DAY_MS), NOW_MS);
    assert!(future.start_ms <= future.end_ms);
}

#[test]
fn a_page_of_voided_purchases_is_asked_for_one_time_products_in_the_window() {
    let world = World::new(vec![token_answer("a"), voided_answer(&["GPA.1"], Some("next page/1")), answer(200, json!({})), answer(500, json!({}))]);
    let window = VoidedWindow { start_ms: 1_000, end_ms: 2_000 };
    let page = block_on(world.api(NOW).voided_page(PACKAGE, &window, None)).unwrap();
    assert_eq!(page.purchases.len(), 1);
    assert_eq!(page.purchases[0].order_id.as_deref(), Some("GPA.1"));
    assert_eq!(page.next_page_token.as_deref(), Some("next page/1"));
    let empty = block_on(world.api(NOW).voided_page(PACKAGE, &window, Some("next page/1"))).unwrap();
    assert_eq!(empty, VoidedPage::default());
    assert!(matches!(block_on(world.api(NOW).voided_page(PACKAGE, &window, None)), Err(VerifyFailure::Unavailable(_))));
    let calls = world.script.calls();
    let base = format!("https://androidpublisher.googleapis.com/androidpublisher/v3/applications/{PACKAGE}/purchases/voidedpurchases");
    assert_eq!(calls[1].url, format!("{base}?type=0&maxResults=1000&startTime=1000&endTime=2000"));
    assert_eq!(calls[2].url, format!("{base}?type=0&maxResults=1000&startTime=1000&endTime=2000&token=next%20page%2F1"));
    assert_eq!((calls[1].verb, calls[1].bearer.as_deref()), (Verb::Get, Some("a")));
}

#[test]
fn a_voided_order_takes_its_credits_back_once_and_an_uncredited_one_is_refused_later() {
    let (d1, _wallets) = credited_world(&["GPA.1"]);
    assert_eq!(balance(&d1), 160);
    let world = World::new(vec![token_answer("a"), voided_answer(&["GPA.1", "GPA.later"], Some("p2")), voided_answer(&["GPA.1"], None)]);

    let report = sweep(&world, &d1, None, NOW_MS);

    assert_eq!(report, VoidedSweep { pages: 2, voided: 3, reversed: 1, recorded: 1, settled: 1, failed: 0, finished: true });
    assert_eq!(balance(&d1), 10, "the 150 credits of the refunded pack are gone");
    assert_eq!(block_on(purchase::store_status(&d1, "pixie", "gp:GPA.1")).unwrap().as_deref(), Some("refunded"));
    assert_eq!(
        d1.int("SELECT COUNT(*) FROM credit_transactions WHERE app_id = 'pixie' AND description = 'Google Play refund reversed: starter pack'", &[]),
        1
    );
    assert_eq!(swept_until(&d1), NOW_MS - 60_000);

    let wallets = FakeWallets::new();
    let ledger = TestLedger { db: &d1, wallets: &wallets };
    let late = block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, "gp:GPA.later", None)).unwrap();
    assert_eq!(late, Credit::Refunded, "an order Google voided before mako saw it is never credited");
    assert_eq!(balance(&d1), 10);
}

#[test]
fn the_next_sweep_starts_before_the_last_one_ended_and_settles_repeats_without_taking_twice() {
    let (d1, _wallets) = credited_world(&["GPA.1"]);
    let world = World::new(vec![token_answer("a"), voided_answer(&["GPA.1"], None)]);
    sweep(&world, &d1, None, NOW_MS);
    world.script.then(voided_answer(&["GPA.1"], None));

    let again = sweep(&world, &d1, None, NOW_MS + 1_800_000);

    assert_eq!(again, VoidedSweep { pages: 1, voided: 1, reversed: 0, recorded: 0, settled: 1, failed: 0, finished: true });
    assert_eq!(balance(&d1), 10);
    let second = &world.script.calls()[2].url;
    assert!(second.contains(&format!("startTime={}", NOW_MS - 60_000 - 2 * 3_600_000)), "{second}");
    assert_eq!(swept_until(&d1), NOW_MS + 1_800_000 - 60_000);
}

#[test]
fn a_sweep_google_cannot_finish_keeps_its_window_and_page_for_the_next_run() {
    let (d1, _wallets) = credited_world(&["GPA.1"]);
    let world = World::new(vec![token_answer("a"), voided_answer(&["GPA.1"], Some("p2")), answer(503, json!({}))]);
    let report = sweep(&world, &d1, None, NOW_MS);
    assert_eq!((report.reversed, report.finished), (1, false));
    assert_eq!(swept_until(&d1), -1, "the window does not move on");
    let window = voided_window(None, NOW_MS);
    assert_eq!(block_on(load_state(&d1, PACKAGE)).unwrap().resume, Some(Resume { window, page_token: "p2".to_string() }));

    world.script.then(voided_answer(&[], None));
    let resumed = sweep(&world, &d1, None, NOW_MS + 1_800_000);
    assert!(resumed.finished);
    let url = &world.script.calls()[3].url;
    assert!(url.contains("token=p2") && url.contains(&format!("startTime={}", window.start_ms)), "{url}");
    assert_eq!(swept_until(&d1), window.end_ms, "the window read through is the one recorded");
    assert_eq!(block_on(load_state(&d1, PACKAGE)).unwrap().resume, None);
}

#[test]
fn a_window_longer_than_one_run_goes_on_from_its_saved_page() {
    let (d1, _wallets) = credited_world(&["GPA.late"]);
    let busy = World::new(std::iter::once(token_answer("a")).chain((0..10).map(|i| voided_answer(&[], Some(&format!("p{i}"))))).collect());
    let capped = sweep(&busy, &d1, None, NOW_MS);
    assert_eq!((capped.pages, capped.finished), (10, false), "one run reads at most ten pages");
    assert_eq!(busy.script.calls().len(), 11);
    assert_eq!(swept_until(&d1), -1);

    busy.script.then(voided_answer(&["GPA.late"], None));
    let next = sweep(&busy, &d1, None, NOW_MS + 1_800_000);
    assert_eq!((next.pages, next.reversed, next.finished), (1, 1, true), "page 11 is read, not page 1 again");
    assert!(busy.script.calls()[11].url.ends_with("&token=p9"));
    assert_eq!(swept_until(&d1), NOW_MS - 60_000);
}

/// Wallets that can be switched off, as an outage of the wallet objects.
struct Switchable<'a> {
    wallets: &'a FakeWallets,
    down: std::cell::Cell<bool>,
}

impl crate::wallet::client::WalletTransport for Switchable<'_> {
    async fn send(&self, midgar_id: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), AppError> {
        if self.down.get() {
            return Err(AppError::WalletUnavailable);
        }
        self.wallets.send(midgar_id, method, path, body).await
    }
}

#[test]
fn a_page_with_an_order_that_could_not_be_settled_is_read_again() {
    let d1 = LocalD1::migrated();
    let fake = FakeWallets::new();
    d1.user("pixie", "u1", 0);
    d1.run(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES ('pixie', 'u1', ?1, 'linked', '2026-10-01T00:00:00+00:00')",
        &[MIDGAR.into()],
    );
    block_on(credit_pack(&d1, &TestLedger { db: &d1, wallets: &fake }, "pixie", "u1", "starter", 150, 299, "gp:GPA.w", None)).unwrap();
    let wallets = Switchable { wallets: &fake, down: std::cell::Cell::new(true) };
    let world = World::new(vec![token_answer("a"), voided_answer(&[], Some("p2")), voided_answer(&["GPA.w"], None)]);

    let failed = block_on(sweep_voided(&world.api(NOW), &d1, Some(&wallets), &pixie_tenant(), NOW_MS)).unwrap();

    assert_eq!((failed.failed, failed.finished), (1, false));
    assert_eq!(block_on(load_state(&d1, PACKAGE)).unwrap().resume.map(|resume| resume.page_token), Some("p2".to_string()));
    wallets.down.set(false);
    world.script.then(voided_answer(&["GPA.w"], None));
    let retried = block_on(sweep_voided(&world.api(NOW), &d1, Some(&wallets), &pixie_tenant(), NOW_MS + 1_800_000)).unwrap();
    assert_eq!((retried.reversed, retried.finished), (1, true));
    assert!(world.script.calls()[3].url.ends_with("&token=p2"), "the failed page is read again");
    assert_eq!(fake.balance(MIDGAR, "pixie"), 0);
}

#[test]
fn a_page_token_google_refuses_starts_the_window_over() {
    let (d1, _wallets) = credited_world(&[]);
    let world = World::new(vec![token_answer("a"), voided_answer(&[], Some("p2")), answer(503, json!({}))]);
    sweep(&world, &d1, None, NOW_MS);
    assert!(block_on(load_state(&d1, PACKAGE)).unwrap().resume.is_some());

    world.script.then(answer(400, json!({ "error": { "status": "INVALID_ARGUMENT" } })));
    let refused = sweep(&world, &d1, None, NOW_MS + 1_800_000);
    assert!(!refused.finished);
    assert_eq!(block_on(load_state(&d1, PACKAGE)).unwrap().resume, None);

    world.script.then(voided_answer(&[], None));
    assert!(sweep(&world, &d1, None, NOW_MS + 3_600_000).finished);
    assert!(!world.script.calls()[4].url.contains("token="), "the window starts over without a page token");
}

#[test]
fn a_voided_promo_order_is_found_by_its_token() {
    let (d1, _wallets) = credited_world(&[]);
    let wallets = FakeWallets::new();
    let ledger = TestLedger { db: &d1, wallets: &wallets };
    let promo = ProductPurchase { order_id: None, purchase_type: Some(1), ..purchase() };
    block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 0, &payment_id(&promo, "promo-token"), None)).unwrap();
    assert_eq!(balance(&d1), 160);
    let world = World::new(vec![token_answer("a"), answer(200, json!({ "voidedPurchases": [{ "purchaseToken": "promo-token" }, {}] }))]);

    let report = sweep(&world, &d1, None, NOW_MS);

    assert_eq!((report.voided, report.reversed), (1, 1), "a record without order or token is skipped");
    assert_eq!(balance(&d1), 10);
}

#[test]
fn a_voided_order_of_a_linked_buyer_is_taken_back_from_the_wallet() {
    let d1 = LocalD1::migrated();
    let wallets = FakeWallets::new();
    d1.user("pixie", "u1", 0);
    d1.run(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES ('pixie', 'u1', ?1, 'linked', '2026-10-01T00:00:00+00:00')",
        &[MIDGAR.into()],
    );
    let ledger = TestLedger { db: &d1, wallets: &wallets };
    block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, "gp:GPA.w", None)).unwrap();
    assert_eq!(wallets.balance(MIDGAR, "pixie"), 150);
    let world = World::new(vec![token_answer("a"), voided_answer(&["GPA.w"], None), voided_answer(&["GPA.w"], None)]);

    assert_eq!(sweep(&world, &d1, Some(&wallets), NOW_MS).reversed, 1);
    assert_eq!(wallets.balance(MIDGAR, "pixie"), 0);
    let (status, entry) = wallets.call(MIDGAR, "GET", "/entry?tenant=pixie&key=reverse:gp:GPA.w", None);
    assert_eq!((status, entry["source"].as_str()), (200, Some("play_store")));

    assert_eq!(sweep(&world, &d1, Some(&wallets), NOW_MS + 1_800_000).settled, 1);
    assert_eq!(wallets.balance(MIDGAR, "pixie"), 0);
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
    let credit = || block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, &play, None)).unwrap();

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
    let after = block_on(credit_pack(&d1, &ledger, "pixie", "u1", "starter", 150, 299, &refunded, None)).unwrap();
    assert_eq!(after, Credit::Refunded);
    assert_eq!(d1.int("SELECT balance FROM user_credits WHERE app_id = 'pixie' AND user_id = 'u1'", &[]), 160);
}
