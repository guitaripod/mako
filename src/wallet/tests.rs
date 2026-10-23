use serde_json::{json, Value};

use super::books::Books;
use super::ledger::{apply, migrate, migrate_amount, reverse, verify, welcome_key};
use super::memory::MemoryBooks;
use super::route::handle;
use super::types::{Balance, Entry, Kind, Outcome, WalletError};

pub const NOW: i64 = 1_760_000_000;

/// Builds an entry with fixed source and description.
pub fn entry(key: &str, tenant: &str, kind: Kind, amount: i64) -> Entry {
    Entry {
        key: key.into(),
        tenant: tenant.into(),
        kind,
        amount,
        source: "test".into(),
        reference: Some(format!("ref:{key}")),
        description: "test entry".into(),
    }
}

/// Sends a JSON request through the route table.
pub fn call(books: &mut impl Books, method: &str, path: &str, body: Value) -> (u16, Value) {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    handle(books, method, path, query, body.to_string().as_bytes(), NOW)
}

/// A purchase grant is applied once and replays with the stored result.
pub fn replay_returns_the_stored_result(books: &mut impl Books) {
    let first = apply(books, &entry("tx:1", "app", Kind::Purchase, 50), NOW).unwrap();
    let Outcome::Applied { balance, seq } = first else { panic!("expected applied, got {first:?}") };
    assert_eq!(balance, 50);
    apply(books, &entry("spend:a", "app", Kind::Spend, -20), NOW).unwrap();
    let again = apply(books, &entry("tx:1", "app", Kind::Purchase, 50), NOW).unwrap();
    assert_eq!(again, Outcome::Replayed { balance_after: 50, seq });
    assert_eq!(books.balance("app").unwrap(), Balance { balance: 30, purchased: 50, spent: 20 });
    assert_eq!(books.chronological("app").unwrap().len(), 2);
}

/// An overdraw reports the current balance and appends nothing.
pub fn insufficient_funds_append_nothing(books: &mut impl Books) {
    apply(books, &entry("tx:1", "app", Kind::Purchase, 5), NOW).unwrap();
    let out = apply(books, &entry("spend:big", "app", Kind::Spend, -6), NOW).unwrap();
    assert_eq!(out, Outcome::Insufficient { balance: 5 });
    assert!(books.find("spend:big").unwrap().is_none());
    assert_eq!(books.chronological("app").unwrap().len(), 1);
    assert_eq!(books.balance("app").unwrap().balance, 5);
    let exact = apply(books, &entry("spend:all", "app", Kind::Spend, -5), NOW).unwrap();
    assert!(matches!(exact, Outcome::Applied { balance: 0, .. }));
    let empty = apply(books, &entry("spend:more", "app", Kind::Spend, -1), NOW).unwrap();
    assert_eq!(empty, Outcome::Insufficient { balance: 0 });
}

/// A reversal takes at most the balance and replays with the same `taken`.
pub fn reverse_clamps_and_replays(books: &mut impl Books) {
    apply(books, &entry("tx:1", "app", Kind::Purchase, 10), NOW).unwrap();
    apply(books, &entry("spend:a", "app", Kind::Spend, -4), NOW).unwrap();
    let out = reverse(books, &entry("reverse:tx:1", "app", Kind::Reversal, -10), NOW).unwrap();
    assert_eq!((out.taken, out.balance, out.replayed), (6, 0, false));
    let again = reverse(books, &entry("reverse:tx:1", "app", Kind::Reversal, -10), NOW).unwrap();
    assert_eq!((again.taken, again.balance, again.replayed, again.seq), (6, 0, true, out.seq));
    let none = reverse(books, &entry("reverse:tx:2", "app", Kind::Reversal, -3), NOW).unwrap();
    assert_eq!((none.taken, none.balance), (0, 0));
    let stored = books.find("reverse:tx:2").unwrap().unwrap();
    assert_eq!(stored.amount, 0);
    let full = reverse(books, &entry("admin:1", "app", Kind::Adjust, -1), NOW).unwrap();
    assert_eq!(full.taken, 0);
}

/// A reversal that fits is taken in full.
pub fn reverse_takes_the_full_amount_when_it_fits(books: &mut impl Books) {
    apply(books, &entry("tx:1", "app", Kind::Purchase, 10), NOW).unwrap();
    let out = reverse(books, &entry("admin:1", "app", Kind::Adjust, -3), NOW).unwrap();
    assert_eq!((out.taken, out.balance), (3, 7));
}

/// Tenants never see each other's balance, and the same key cannot cross tenants.
pub fn tenants_are_isolated(books: &mut impl Books) {
    apply(books, &entry("tx:1", "alpha", Kind::Purchase, 10), NOW).unwrap();
    apply(books, &entry("tx:2", "beta", Kind::Purchase, 3), NOW).unwrap();
    let out = apply(books, &entry("spend:a", "beta", Kind::Spend, -4), NOW).unwrap();
    assert_eq!(out, Outcome::Insufficient { balance: 3 });
    apply(books, &entry("spend:b", "alpha", Kind::Spend, -4), NOW).unwrap();
    assert_eq!(books.balance("alpha").unwrap().balance, 6);
    assert_eq!(books.balance("beta").unwrap().balance, 3);
    let cross = apply(books, &entry("tx:1", "beta", Kind::Purchase, 10), NOW);
    assert!(matches!(cross, Err(WalletError::Conflict(_))));
    assert_eq!(books.entries("alpha", None, 10).unwrap().len(), 2);
    assert_eq!(books.entries("beta", None, 10).unwrap().len(), 1);
}

/// The pool tenant and an empty tenant are refused for every operation.
pub fn pool_tenant_is_rejected(books: &mut impl Books) {
    for tenant in ["*", ""] {
        let err = apply(books, &entry("tx:1", tenant, Kind::Purchase, 10), NOW).unwrap_err();
        assert!(matches!(err, WalletError::BadRequest(_)), "{tenant:?}: {err:?}");
        assert!(migrate(books, tenant, "migrate:x", 1, 0, NOW).is_err());
    }
    assert_eq!(books.tenants().unwrap().len(), 0);
}

/// A reused key with another amount or kind is a conflict; a migrate key replays whatever it had.
pub fn idempotency_conflicts(books: &mut impl Books) {
    apply(books, &entry("tx:1", "app", Kind::Purchase, 10), NOW).unwrap();
    let amount = apply(books, &entry("tx:1", "app", Kind::Purchase, 11), NOW);
    assert!(matches!(amount, Err(WalletError::Conflict(_))));
    let kind = apply(books, &entry("tx:1", "app", Kind::Bonus, 10), NOW);
    assert!(matches!(kind, Err(WalletError::Conflict(_))));
    let flipped = apply(books, &entry("tx:1", "app", Kind::Spend, -10), NOW);
    assert!(matches!(flipped, Err(WalletError::Conflict(_))));
    assert_eq!(books.balance("app").unwrap().balance, 10);

    let first = migrate(books, "app", "migrate:app:u1", 7, 6, NOW).unwrap();
    assert_eq!((first.amount, first.replayed), (7, false));
    let again = migrate(books, "app", "migrate:app:u1", 99, 0, NOW).unwrap();
    assert_eq!((again.amount, again.replayed, again.seq), (7, true, first.seq));
    let other_kind = migrate(books, "app", "tx:1", 1, 0, NOW);
    assert!(matches!(other_kind, Err(WalletError::Conflict(_))));
}

/// Entries that break the sign rules or the amount bound are refused.
pub fn invalid_entries_are_refused(books: &mut impl Books) {
    for (kind, amount) in [
        (Kind::Purchase, 0),
        (Kind::Purchase, -1),
        (Kind::Spend, 1),
        (Kind::Spend, 0),
        (Kind::Marker, 1),
        (Kind::Reversal, 1),
        (Kind::Migrate, -1),
        (Kind::Purchase, 2_000_000_000),
    ] {
        let err = apply(books, &entry("k", "app", kind, amount), NOW).unwrap_err();
        assert!(matches!(err, WalletError::BadRequest(_)), "{kind:?} {amount}: {err:?}");
    }
    let no_key = apply(books, &entry("", "app", Kind::Purchase, 1), NOW);
    assert!(matches!(no_key, Err(WalletError::BadRequest(_))));
    assert_eq!(books.tenants().unwrap().len(), 0);
}

/// The welcome bonus is counted once across two app users of one Midgar ID.
pub fn migrate_dedupes_the_welcome(books: &mut impl Books) {
    let first = migrate(books, "app", "migrate:app:a", 10, 6, NOW).unwrap();
    assert_eq!(first.amount, 10);
    assert!(books.find(&welcome_key("app")).unwrap().is_some());
    let second = migrate(books, "app", "migrate:app:b", 8, 6, NOW).unwrap();
    assert_eq!(second.amount, 2);
    let all_welcome = migrate(books, "app", "migrate:app:c", 4, 6, NOW).unwrap();
    assert_eq!(all_welcome.amount, 0);
    let residue = migrate(books, "app", "migrate:app:a:0", 5, 0, NOW).unwrap();
    assert_eq!(residue.amount, 5);
    assert_eq!(books.balance("app").unwrap().balance, 17);
    let elsewhere = migrate(books, "other", "migrate:other:a", 6, 6, NOW).unwrap();
    assert_eq!(elsewhere.amount, 6);
}

/// A welcome bonus grant under the welcome key counts as the claim.
pub fn a_welcome_grant_counts_as_the_claim(books: &mut impl Books) {
    apply(books, &entry(&welcome_key("app"), "app", Kind::Bonus, 6), NOW).unwrap();
    let moved = migrate(books, "app", "migrate:app:a", 9, 6, NOW).unwrap();
    assert_eq!(moved.amount, 3);
    assert_eq!(books.chronological("app").unwrap().len(), 2);
}

/// Verify passes on a clean ledger and reports every kind of drift.
pub fn verify_reports_corruption(books: &mut MemoryBooks) {
    apply(books, &entry("tx:1", "app", Kind::Purchase, 10), NOW).unwrap();
    apply(books, &entry("spend:a", "app", Kind::Spend, -3), NOW).unwrap();
    apply(books, &entry("tx:2", "other", Kind::Purchase, 4), NOW).unwrap();
    let clean = verify(books).unwrap();
    assert!(clean.ok, "{:?}", clean.problems);
    assert_eq!((clean.tenants, clean.entries), (2, 3));
    books.corrupt_balance("app", Balance { balance: 8, purchased: 10, spent: 3 });
    let broken = verify(books).unwrap();
    assert!(!broken.ok);
    assert_eq!(broken.problems.len(), 1);
    assert!(broken.problems[0].starts_with("app:"));
}

/// The route table answers the documented statuses and bodies.
pub fn routes_answer_the_documented_statuses(books: &mut impl Books) {
    let grant = json!({"key":"tx:1","tenant":"app","kind":"purchase","amount":10,"source":"revenuecat","description":"Pack"});
    let (status, body) = call(books, "POST", "/grant", grant.clone());
    assert_eq!((status, body["outcome"].as_str(), body["balance"].as_i64()), (200, Some("applied"), Some(10)));
    let (status, body) = call(books, "POST", "/grant", grant);
    assert_eq!((status, body["outcome"].as_str(), body["balance_after"].as_i64()), (200, Some("replayed"), Some(10)));
    let conflict = json!({"key":"tx:1","tenant":"app","kind":"purchase","amount":11,"source":"x","description":"Pack"});
    let (status, body) = call(books, "POST", "/grant", conflict);
    assert_eq!((status, body["error"].as_str()), (409, Some("idempotency_conflict")));
    let spend = json!({"key":"spend:chat:1","tenant":"app","kind":"spend","amount":-4,"source":"usage","reference":"chat:1","description":"Chat"});
    let (status, body) = call(books, "POST", "/spend", spend);
    assert_eq!((status, body["balance"].as_i64()), (200, Some(6)));
    let big = json!({"key":"spend:chat:2","tenant":"app","kind":"spend","amount":-7,"source":"usage","description":"Chat"});
    let (status, body) = call(books, "POST", "/spend", big);
    assert_eq!((status, body["error"].as_str(), body["balance"].as_i64()), (402, Some("insufficient_funds"), Some(6)));
    let bad_spend = json!({"key":"k","tenant":"app","kind":"spend","amount":4,"source":"usage","description":"Chat"});
    assert_eq!(call(books, "POST", "/spend", bad_spend).0, 400);
    let bad_grant = json!({"key":"k","tenant":"app","kind":"spend","amount":-4,"source":"usage","description":"Chat"});
    assert_eq!(call(books, "POST", "/grant", bad_grant).0, 400);
    let pool = json!({"key":"k","tenant":"*","kind":"purchase","amount":4,"source":"x","description":"Chat"});
    assert_eq!(call(books, "POST", "/grant", pool).0, 400);
    let rev = json!({"key":"reverse:tx:1","tenant":"app","kind":"reversal","amount":-10,"source":"revenuecat","description":"Refund"});
    let (status, body) = call(books, "POST", "/reverse", rev);
    assert_eq!((status, body["taken"].as_i64(), body["balance"].as_i64()), (200, Some(6), Some(0)));
    let (status, body) = call(books, "GET", "/balance?tenant=app", json!({}));
    assert_eq!((status, body), (200, json!({"balance":0,"purchased":10,"spent":10})));
    assert_eq!(call(books, "GET", "/balance?tenant=*", json!({})).0, 400);
    assert_eq!(call(books, "GET", "/balance", json!({})).0, 400);
    let (_, body) = call(books, "GET", "/balance?tenant=unseen", json!({}));
    assert_eq!(body, json!({"balance":0,"purchased":0,"spent":0}));
    let (status, body) = call(books, "GET", "/verify", json!({}));
    assert_eq!((status, body["ok"].as_bool()), (200, Some(true)));
    assert_eq!(call(books, "GET", "/grant", json!({})).0, 405);
    assert_eq!(call(books, "POST", "/nope", json!({})).0, 404);
    assert_eq!(handle(books, "POST", "/grant", "", b"{", NOW).0, 400);
}

/// Entries come newest first with a cursor, and erase removes only one tenant.
pub fn entries_page_and_erase(books: &mut impl Books) {
    for n in 1..=5 {
        apply(books, &entry(&format!("tx:{n}"), "app", Kind::Purchase, n), NOW).unwrap();
    }
    apply(books, &entry("tx:o", "other", Kind::Purchase, 9), NOW).unwrap();
    let (_, page) = call(books, "GET", "/entries?tenant=app&limit=2", json!({}));
    let keys: Vec<&str> = page["entries"].as_array().unwrap().iter().map(|e| e["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["tx:5", "tx:4"]);
    let cursor = page["next_before"].as_i64().unwrap();
    let (_, page) = call(books, "GET", &format!("/entries?tenant=app&limit=2&before={cursor}"), json!({}));
    let keys: Vec<&str> = page["entries"].as_array().unwrap().iter().map(|e| e["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["tx:3", "tx:2"]);
    let (_, last) = call(books, "GET", &format!("/entries?tenant=app&before={}", page["next_before"]), json!({}));
    assert_eq!(last["entries"].as_array().unwrap().len(), 1);
    assert!(last["next_before"].is_null());
    assert_eq!(call(books, "GET", "/entries?tenant=app&limit=x", json!({})).0, 400);

    let (status, body) = call(books, "POST", "/erase", json!({"tenant":"app"}));
    assert_eq!((status, body), (200, json!({"erased":5})));
    assert_eq!(books.balance("app").unwrap(), Balance::default());
    assert_eq!(books.balance("other").unwrap().balance, 9);
    assert!(verify(books).unwrap().ok);
    assert_eq!(call(books, "POST", "/erase", json!({"tenant":"*"})).0, 400);
    let again = apply(books, &entry("tx:1", "app", Kind::Purchase, 1), NOW).unwrap();
    assert!(matches!(again, Outcome::Applied { balance: 1, .. }));
}

/// `erase_all` empties every tenant, is safe to repeat, and leaves a wallet that refuses writes but verifies clean.
pub fn erase_all_empties_the_wallet_for_good(books: &mut impl Books) {
    apply(books, &entry("tx:1", "alpha", Kind::Purchase, 10), NOW).unwrap();
    apply(books, &entry("tx:2", "beta", Kind::Purchase, 3), NOW).unwrap();
    apply(books, &entry("spend:a", "beta", Kind::Spend, -1), NOW).unwrap();
    let (status, body) = call(books, "POST", "/erase_all", json!({}));
    assert_eq!((status, body), (200, json!({"erased": 3, "tenants": 2})));
    let (status, body) = call(books, "POST", "/erase_all", json!({}));
    assert_eq!((status, body), (200, json!({"erased": 0, "tenants": 0})));
    assert_eq!(books.tenants().unwrap().len(), 0);
    let grant = json!({"key":"tx:9","tenant":"alpha","kind":"purchase","amount":5,"source":"x","description":"Pack"});
    let (status, body) = call(books, "POST", "/grant", grant);
    assert_eq!((status, body["error"].as_str()), (409, Some("erased")));
    let (status, body) = call(books, "GET", "/balance?tenant=alpha", json!({}));
    assert_eq!((status, body), (200, json!({"balance":0,"purchased":0,"spent":0})));
    assert!(verify(books).unwrap().ok);
    assert_eq!(call(books, "GET", "/erase_all", json!({})).0, 405);
}

/// A sealed wallet refuses every write with the survivor's id, still reads its stored totals, and keeps its first target.
pub fn a_sealed_wallet_refuses_writes_and_keeps_its_first_target(books: &mut impl Books) {
    apply(books, &entry("tx:1", "app", Kind::Purchase, 10), NOW).unwrap();
    let (status, body) = call(books, "POST", "/seal", json!({"into": "survivor"}));
    assert_eq!((status, body["sealed_into"].as_str(), body["replayed"].as_bool()), (200, Some("survivor"), Some(false)));
    let (_, again) = call(books, "POST", "/seal", json!({"into": "someone-else"}));
    assert_eq!((again["sealed_into"].as_str(), again["replayed"].as_bool()), (Some("survivor"), Some(true)));
    let grant = json!({"key":"tx:2","tenant":"app","kind":"purchase","amount":5,"source":"x","description":"Pack"});
    let spend = json!({"key":"spend:a","tenant":"app","kind":"spend","amount":-1,"source":"usage","description":"Chat"});
    let reverse = json!({"key":"reverse:a","tenant":"app","kind":"reversal","amount":-1,"source":"usage","description":"Refund"});
    let migrate = json!({"tenant":"app","key":"migrate:app:u","moved":3,"welcome_part":0});
    for (path, body) in [("/grant", grant), ("/spend", spend), ("/reverse", reverse), ("/migrate", migrate)] {
        let (status, answer) = call(books, "POST", path, body);
        assert_eq!((status, answer["error"].as_str(), answer["sealed_into"].as_str()), (409, Some("sealed"), Some("survivor")), "{path}");
    }
    let (_, balance) = call(books, "GET", "/balance?tenant=app", json!({}));
    assert_eq!(balance["balance"], 10);
    let (_, state) = call(books, "GET", "/state", json!({}));
    assert_eq!((state["sealed_into"].as_str(), state["erased"].as_bool()), (Some("survivor"), Some(false)));
    assert_eq!(state["tenants"], json!([{"tenant":"app","balance":10,"purchased":10,"spent":0}]));
    assert!(verify(books).unwrap().ok);
    assert_eq!(call(books, "POST", "/seal", json!({"into": ""})).0, 400);
}

/// An absorb takes every tenant or none, once per merged-away ID, and records where the credits came from.
pub fn absorb_takes_all_tenants_once_and_records_the_source(books: &mut impl Books) {
    apply(books, &entry("tx:own", "alpha", Kind::Purchase, 5), NOW).unwrap();
    let items = json!({"from": "old", "items": [{"tenant": "alpha", "amount": 7}, {"tenant": "beta", "amount": 3}]});
    let (status, body) = call(books, "POST", "/absorb", items.clone());
    assert_eq!((status, body["replayed"].as_bool()), (200, Some(false)));
    assert_eq!(
        body["totals"],
        json!([{"tenant":"alpha","balance":12,"purchased":5,"spent":0},{"tenant":"beta","balance":3,"purchased":0,"spent":0}])
    );
    let (status, body) = call(books, "POST", "/absorb", items);
    assert_eq!((status, body["replayed"].as_bool()), (200, Some(true)));
    assert_eq!(books.balance("alpha").unwrap().balance, 12);
    assert_eq!(books.balance("beta").unwrap().balance, 3);
    let stored = books.find("merge:old:alpha").unwrap().unwrap();
    assert_eq!((stored.kind, stored.amount, stored.source.as_str(), stored.reference.as_deref()), (Kind::Adjust, 7, "merge", Some("old")));
    assert_eq!(books.entries("alpha", None, 10).unwrap().len(), 2);
    let (_, state) = call(books, "GET", "/state", json!({}));
    assert_eq!(state["sources"], json!(["old"]));
    let other = json!({"from": "older", "items": [{"tenant": "alpha", "amount": 1}]});
    assert_eq!(call(books, "POST", "/absorb", other).1["totals"][0]["balance"], 13);
    let (_, state) = call(books, "GET", "/state", json!({}));
    assert_eq!(state["sources"], json!(["old", "older"]));
    assert!(verify(books).unwrap().ok);
}

/// A malformed absorb writes nothing: not even the tenants that were fine.
pub fn a_bad_absorb_writes_nothing(books: &mut impl Books) {
    for items in [
        json!([{"tenant": "alpha", "amount": 2}, {"tenant": "*", "amount": 1}]),
        json!([{"tenant": "alpha", "amount": 2}, {"tenant": "beta", "amount": 0}]),
        json!([{"tenant": "alpha", "amount": 2}, {"tenant": "beta", "amount": -1}]),
        json!([{"tenant": "alpha", "amount": 2}, {"tenant": "alpha", "amount": 1}]),
    ] {
        let (status, _) = call(books, "POST", "/absorb", json!({"from": "old", "items": items}));
        assert!(status == 400 || status == 409, "{status}");
        assert_eq!(books.tenants().unwrap().len(), 0, "{items}");
        assert!(books.meta("absorbed:old").unwrap().is_none());
    }
    assert_eq!(call(books, "POST", "/absorb", json!({"from": "", "items": []})).0, 400);
}

/// A wallet that was merged away itself still answers for what it took before, and points on for anything new.
pub fn a_sealed_survivor_answers_for_what_it_took_and_points_on(books: &mut impl Books) {
    let first = json!({"from": "old", "items": [{"tenant": "alpha", "amount": 4}]});
    assert_eq!(call(books, "POST", "/absorb", first.clone()).0, 200);
    call(books, "POST", "/seal", json!({"into": "newer"}));
    let (status, body) = call(books, "POST", "/absorb", first);
    assert_eq!((status, body["replayed"].as_bool()), (200, Some(true)));
    let new = json!({"from": "another", "items": [{"tenant": "alpha", "amount": 1}]});
    let (status, body) = call(books, "POST", "/absorb", new);
    assert_eq!((status, body["error"].as_str(), body["sealed_into"].as_str()), (409, Some("sealed"), Some("newer")));
    assert_eq!(books.balance("alpha").unwrap().balance, 4);
}

/// Erasing clears the seal and the record of absorbed IDs, and an erased wallet refuses an absorb.
pub fn erase_all_clears_the_seal_and_the_sources(books: &mut impl Books) {
    call(books, "POST", "/absorb", json!({"from": "old", "items": [{"tenant": "alpha", "amount": 4}]}));
    call(books, "POST", "/seal", json!({"into": "newer"}));
    assert_eq!(call(books, "POST", "/erase_all", json!({})).0, 200);
    let (_, state) = call(books, "GET", "/state", json!({}));
    assert_eq!((state["sealed_into"].is_null(), state["erased"].as_bool(), state["sources"].clone(), state["tenants"].clone()), (true, Some(true), json!([]), json!([])));
    let (status, body) = call(books, "POST", "/absorb", json!({"from": "again", "items": [{"tenant": "alpha", "amount": 1}]}));
    assert_eq!((status, body["error"].as_str()), (409, Some("erased")));
    let (_, sealed) = call(books, "POST", "/seal", json!({"into": "x"}));
    assert_eq!((sealed["erased"].as_bool(), sealed["sealed_into"].is_null()), (Some(true), true));
}

#[test]
fn the_merge_objects_behave_in_memory() {
    a_sealed_wallet_refuses_writes_and_keeps_its_first_target(&mut MemoryBooks::default());
    absorb_takes_all_tenants_once_and_records_the_source(&mut MemoryBooks::default());
    a_bad_absorb_writes_nothing(&mut MemoryBooks::default());
    a_sealed_survivor_answers_for_what_it_took_and_points_on(&mut MemoryBooks::default());
    erase_all_clears_the_seal_and_the_sources(&mut MemoryBooks::default());
}

#[test]
fn erase_all_empties_the_wallet_for_good_in_memory() {
    erase_all_empties_the_wallet_for_good(&mut MemoryBooks::default());
}

#[test]
fn replay_returns_the_stored_result_in_memory() {
    replay_returns_the_stored_result(&mut MemoryBooks::default());
}

#[test]
fn insufficient_funds_append_nothing_in_memory() {
    insufficient_funds_append_nothing(&mut MemoryBooks::default());
}

#[test]
fn reverse_clamps_and_replays_in_memory() {
    reverse_clamps_and_replays(&mut MemoryBooks::default());
    reverse_takes_the_full_amount_when_it_fits(&mut MemoryBooks::default());
}

#[test]
fn tenants_are_isolated_in_memory() {
    tenants_are_isolated(&mut MemoryBooks::default());
    pool_tenant_is_rejected(&mut MemoryBooks::default());
}

#[test]
fn idempotency_conflicts_in_memory() {
    idempotency_conflicts(&mut MemoryBooks::default());
    invalid_entries_are_refused(&mut MemoryBooks::default());
}

#[test]
fn migrate_dedupes_the_welcome_in_memory() {
    migrate_dedupes_the_welcome(&mut MemoryBooks::default());
    a_welcome_grant_counts_as_the_claim(&mut MemoryBooks::default());
}

#[test]
fn verify_reports_corruption_in_memory() {
    verify_reports_corruption(&mut MemoryBooks::default());
}

#[test]
fn routes_answer_the_documented_statuses_in_memory() {
    routes_answer_the_documented_statuses(&mut MemoryBooks::default());
}

#[test]
fn entries_page_and_erase_in_memory() {
    entries_page_and_erase(&mut MemoryBooks::default());
}

#[test]
fn migrate_amount_follows_the_rule() {
    assert_eq!(migrate_amount(10, 6, false), 10);
    assert_eq!(migrate_amount(10, 6, true), 4);
    assert_eq!(migrate_amount(4, 6, true), 0);
    assert_eq!(migrate_amount(0, 6, true), 0);
    assert_eq!(migrate_amount(10, 0, true), 10);
    assert_eq!(migrate_amount(-5, 0, false), 0);
    assert_eq!(migrate_amount(5, -3, true), 5);
}

#[test]
fn the_sharing_pool_is_read_nowhere() {
    let source = [
        include_str!("books.rs"),
        include_str!("ledger.rs"),
        include_str!("route.rs"),
        include_str!("sql.rs"),
        include_str!("object.rs"),
    ];
    assert!(source.iter().all(|file| !file.contains("sharing")));
}
