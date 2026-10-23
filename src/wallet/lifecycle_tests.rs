use futures::executor::block_on;
use serde_json::json;

use super::lifecycle::{erase_all, holds_wallet, merge, Erasure, Merged};
use super::link::Linker;
use super::links::{self, LinkState};
use super::testing::{Budget, FakeWallets, Flaky, LocalD1};
use crate::error::AppError;

const APP: &str = "dreameater";
const OTHER_APP: &str = "payday";
const MIDGAR: &str = "0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d47";
const OTHER: &str = "0195aaaa-7b1c-4d2e-9a60-3c5e8f1b2d47";
const THIRD: &str = "0196bbbb-7b1c-4d2e-9a60-3c5e8f1b2d47";

struct World {
    d1: LocalD1,
    wallets: FakeWallets,
}

impl World {
    fn new() -> Self {
        Self { d1: LocalD1::migrated(), wallets: FakeWallets::new() }
    }

    fn link(&self, app: &str, user: &str, midgar: &str) {
        self.d1.user(app, user, 0);
        self.d1.run(
            "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, ?2, ?3, 'linked', '2026-10-01T00:00:00+00:00')",
            &[app.into(), user.into(), midgar.into()],
        );
        self.d1.run(
            "INSERT INTO wallet_summary (app_id, midgar_id, balance, purchased, spent, updated_at) VALUES (?1, ?2, 0, 0, 0, '2026-10-01T00:00:00+00:00')
             ON CONFLICT(app_id, midgar_id) DO NOTHING",
            &[app.into(), midgar.into()],
        );
    }

    fn credit(&self, midgar: &str, tenant: &str, key: &str, amount: i64) {
        let body = json!({"key": key, "tenant": tenant, "kind": "purchase", "amount": amount, "source": "test", "description": "Pack"});
        assert_eq!(self.wallets.call(midgar, "POST", "/grant", Some(&body)).0, 200);
    }

    fn erase(&self, midgar: &str) -> Result<Erasure, AppError> {
        block_on(erase_all(&self.d1, &self.wallets, midgar))
    }

    fn rows(&self, table: &str, midgar: &str) -> i64 {
        self.d1.int(&format!("SELECT COUNT(*) FROM {table} WHERE midgar_id = ?1"), &[midgar.into()])
    }

    fn two_tenants() -> Self {
        let world = Self::new();
        world.link(APP, "u1", MIDGAR);
        world.link(OTHER_APP, "u2", MIDGAR);
        world.credit(MIDGAR, APP, "tx:1", 40);
        world.credit(MIDGAR, OTHER_APP, "tx:2", 7);
        world
    }
}

#[test]
fn erasing_a_midgar_id_empties_every_tenant_and_removes_its_rows() {
    let world = World::two_tenants();
    world.link(APP, "u3", OTHER);
    world.credit(OTHER, APP, "tx:3", 9);
    let erasure = world.erase(MIDGAR).unwrap();
    assert_eq!(erasure, Erasure::Erased { entries: 2, links: 2 });
    assert_eq!((world.wallets.balance(MIDGAR, APP), world.wallets.balance(MIDGAR, OTHER_APP)), (0, 0));
    assert_eq!((world.rows("wallet_links", MIDGAR), world.rows("wallet_summary", MIDGAR)), (0, 0));
    assert_eq!(world.wallets.balance(OTHER, APP), 9, "another Midgar ID is untouched");
    assert_eq!((world.rows("wallet_links", OTHER), world.rows("wallet_summary", OTHER)), (1, 1));
    assert!(world.wallets.verified(MIDGAR));
}

#[test]
fn erasing_twice_is_a_no_op_the_second_time() {
    let world = World::two_tenants();
    assert!(matches!(world.erase(MIDGAR).unwrap(), Erasure::Erased { .. }));
    assert_eq!(world.erase(MIDGAR).unwrap(), Erasure::NoWallet);
    assert_eq!(world.wallets.balance(MIDGAR, APP), 0);
}

#[test]
fn an_id_nothing_links_to_never_creates_an_object() {
    let world = World::new();
    assert_eq!(world.erase(MIDGAR).unwrap(), Erasure::NoWallet);
    assert!(world.wallets.calls.borrow().is_empty(), "no wallet object was asked for");
    assert!(!block_on(holds_wallet(&world.d1, MIDGAR)).unwrap());
}

#[test]
fn an_erased_wallet_refuses_every_later_write_and_still_reads_as_empty() {
    let world = World::two_tenants();
    world.erase(MIDGAR).unwrap();
    let grant = json!({"key": "tx:late", "tenant": APP, "kind": "purchase", "amount": 5, "source": "test", "description": "Pack"});
    let spend = json!({"key": "spend:late", "tenant": APP, "kind": "spend", "amount": -1, "source": "usage", "description": "Chat"});
    let reverse = json!({"key": "reverse:late", "tenant": APP, "kind": "reversal", "amount": -1, "source": "usage", "description": "Refund"});
    let migrate = json!({"tenant": APP, "key": "migrate:late", "moved": 3, "welcome_part": 0});
    for (path, body) in [("/grant", grant), ("/spend", spend), ("/reverse", reverse), ("/migrate", migrate)] {
        let (status, answer) = world.wallets.call(MIDGAR, "POST", path, Some(&body));
        assert_eq!((status, answer["error"].as_str()), (409, Some("erased")), "{path}");
    }
    assert_eq!(world.wallets.balance(MIDGAR, APP), 0);
    assert!(world.wallets.verified(MIDGAR));
}

#[test]
fn the_client_reads_an_erased_wallet_as_unavailable() {
    use super::client::Wallets;
    let world = World::two_tenants();
    world.erase(MIDGAR).unwrap();
    let entry = super::types::Entry {
        key: "tx:late".into(),
        tenant: APP.into(),
        kind: super::types::Kind::Purchase,
        amount: 5,
        source: "test".into(),
        reference: None,
        description: "Pack".into(),
    };
    let err = block_on(Wallets { transport: &world.wallets }.grant(MIDGAR, &entry)).unwrap_err();
    assert!(matches!(err, AppError::WalletUnavailable));
}

#[test]
fn a_crash_after_any_write_is_finished_by_the_retry() {
    let mut crashes = 0;
    let mut finished = false;
    for ticks in 0..20 {
        let world = World::two_tenants();
        let budget = Budget::of(ticks);
        let (d1, wallets) = (Flaky { inner: &world.d1, budget: &budget }, Flaky { inner: &world.wallets, budget: &budget });
        match block_on(erase_all(&d1, &wallets, MIDGAR)) {
            Ok(_) => finished = true,
            Err(_) => crashes += 1,
        }
        budget.lift();
        let retried = world.erase(MIDGAR).unwrap();
        assert!(matches!(retried, Erasure::Erased { .. } | Erasure::NoWallet), "ticks {ticks}");
        assert_eq!((world.wallets.balance(MIDGAR, APP), world.wallets.balance(MIDGAR, OTHER_APP)), (0, 0), "ticks {ticks}");
        assert_eq!((world.rows("wallet_links", MIDGAR), world.rows("wallet_summary", MIDGAR)), (0, 0), "ticks {ticks}");
        if finished {
            break;
        }
    }
    assert!(crashes >= 2, "the injector walked through the writes ({crashes})");
    assert!(finished);
}

#[test]
fn a_wallet_that_cannot_answer_fails_the_erasure_and_leaves_the_links_to_find_it_again() {
    let world = World::two_tenants();
    let budget = Budget::of(0);
    let wallets = Flaky { inner: &world.wallets, budget: &budget };
    assert!(matches!(block_on(erase_all(&world.d1, &wallets, MIDGAR)), Err(AppError::WalletUnavailable)));
    assert_eq!(world.rows("wallet_links", MIDGAR), 2);
    assert_eq!(world.wallets.balance(MIDGAR, APP), 40);
}

fn state(world: &World, midgar: &str) -> serde_json::Value {
    world.wallets.call(midgar, "GET", "/state", None).1
}

fn entries_of(world: &World, midgar: &str, tenant: &str, key: &str) -> usize {
    let (_, body) = world.wallets.call(midgar, "GET", &format!("/entries?tenant={tenant}&limit=200"), None);
    body["entries"].as_array().unwrap().iter().filter(|e| e["key"] == key).count()
}

fn link_of(world: &World, app: &str, user: &str) -> String {
    block_on(links::read(&world.d1, app, user)).unwrap().unwrap().midgar_id
}

impl World {
    fn merge(&self, from: &str, to: &str) -> Result<Merged, AppError> {
        block_on(merge(&self.d1, &self.wallets, from, to))
    }

    fn two_wallets() -> Self {
        let world = Self::two_tenants();
        world.link(APP, "u3", OTHER);
        world.credit(OTHER, APP, "tx:3", 10);
        world
    }

    fn assert_merged_into(&self, survivor: &str) {
        assert_eq!(self.wallets.balance(survivor, APP), 50, "40 from the merged-away wallet and 10 of its own");
        assert_eq!(self.wallets.balance(survivor, OTHER_APP), 7);
        assert_eq!(entries_of(self, survivor, APP, &format!("merge:{MIDGAR}:{APP}")), 1);
        assert_eq!(entries_of(self, survivor, OTHER_APP, &format!("merge:{MIDGAR}:{OTHER_APP}")), 1);
        assert!(self.wallets.verified(survivor));
        assert_eq!(self.rows("wallet_summary", MIDGAR), 0);
        assert_eq!(self.rows("wallet_links", MIDGAR), 0);
        assert_eq!(self.rows("wallet_links", survivor), 3, "u1, u2 and u3 are all linked to the survivor now");
    }
}

#[test]
fn a_merge_moves_every_tenant_into_the_survivor_and_repoints_the_links() {
    let world = World::two_wallets();
    let merged = world.merge(MIDGAR, OTHER).unwrap();
    assert_eq!(merged, Merged::Moved { target: OTHER.to_string(), tenants: 2, credits: 47 });
    world.assert_merged_into(OTHER);
    assert_eq!(state(&world, MIDGAR)["sealed_into"], OTHER);
    assert_eq!(state(&world, OTHER)["sources"], json!([MIDGAR]));
    assert_eq!(link_of(&world, APP, "u1"), OTHER);
    assert_eq!(world.d1.int("SELECT balance FROM wallet_summary WHERE app_id = ?1 AND midgar_id = ?2", &[APP.into(), OTHER.into()]), 50);
}

#[test]
fn the_merged_away_wallet_refuses_writes_and_a_spend_lands_on_the_survivor() {
    let world = World::two_wallets();
    world.merge(MIDGAR, OTHER).unwrap();
    let spend = json!({"key": "spend:late", "tenant": APP, "kind": "spend", "amount": -1, "source": "usage", "description": "Chat"});
    let (status, body) = world.wallets.call(MIDGAR, "POST", "/spend", Some(&spend));
    assert_eq!((status, body["error"].as_str(), body["sealed_into"].as_str()), (409, Some("sealed"), Some(OTHER)));
    assert_eq!(world.wallets.call(OTHER, "POST", "/spend", Some(&spend)).0, 200);
    assert_eq!(world.wallets.balance(OTHER, APP), 49);
}

#[test]
fn replaying_the_merge_changes_nothing_at_any_stage() {
    let world = World::two_wallets();
    world.merge(MIDGAR, OTHER).unwrap();
    assert_eq!(world.merge(MIDGAR, OTHER).unwrap(), Merged::NoWallet, "a finished merge is gated out");
    world.d1.run(
        "INSERT INTO wallet_summary (app_id, midgar_id, balance, purchased, spent, updated_at) VALUES (?1, ?2, 40, 0, 0, '2026-10-01T00:00:00+00:00')",
        &[APP.into(), MIDGAR.into()],
    );
    let again = world.merge(MIDGAR, OTHER).unwrap();
    assert!(matches!(again, Merged::Moved { .. }), "a stale mirror row sends it through every step again");
    world.assert_merged_into(OTHER);
    assert_eq!(world.wallets.count("POST /absorb"), 2);
    assert_eq!(world.merge(MIDGAR, MIDGAR).unwrap(), Merged::Same);
}

#[test]
fn an_id_with_no_wallet_is_a_no_op_and_touches_no_object() {
    let world = World::new();
    assert_eq!(world.merge(MIDGAR, OTHER).unwrap(), Merged::NoWallet);
    assert!(world.wallets.calls.borrow().is_empty());
}

#[test]
fn a_crash_after_any_write_is_finished_by_replaying_the_merge() {
    let mut crashes = 0;
    let mut finished = false;
    for ticks in 0..40 {
        let world = World::two_wallets();
        let budget = Budget::of(ticks);
        let (d1, wallets) = (Flaky { inner: &world.d1, budget: &budget }, Flaky { inner: &world.wallets, budget: &budget });
        match block_on(merge(&d1, &wallets, MIDGAR, OTHER)) {
            Ok(_) => finished = true,
            Err(_) => crashes += 1,
        }
        budget.lift();
        world.merge(MIDGAR, OTHER).unwrap();
        world.assert_merged_into(OTHER);
        if finished {
            break;
        }
    }
    assert!(crashes >= 4, "the injector walked through the writes ({crashes})");
    assert!(finished);
}

#[test]
fn a_merge_into_a_survivor_that_was_merged_away_lands_in_the_final_survivor() {
    let world = World::two_wallets();
    world.link(APP, "u4", THIRD);
    world.credit(THIRD, APP, "tx:4", 100);
    world.merge(OTHER, THIRD).unwrap();
    assert_eq!(world.merge(MIDGAR, OTHER).unwrap(), Merged::Moved { target: THIRD.to_string(), tenants: 2, credits: 47 });
    assert_eq!(world.wallets.balance(THIRD, APP), 150);
    assert_eq!(world.wallets.balance(THIRD, OTHER_APP), 7);
    assert_eq!(entries_of(&world, THIRD, APP, &format!("merge:{MIDGAR}:{APP}")), 1);
    assert_eq!(world.wallets.balance(OTHER, APP), 10, "the middle wallet is sealed and keeps its archive");
    assert_eq!((world.rows("wallet_links", MIDGAR), world.rows("wallet_links", OTHER), world.rows("wallet_links", THIRD)), (0, 0, 4));
    assert!(world.wallets.verified(THIRD));
}

#[test]
fn a_survivor_merged_away_after_it_took_the_credits_does_not_take_them_twice() {
    let world = World::two_wallets();
    world.link(APP, "u4", THIRD);
    world.credit(THIRD, APP, "tx:4", 100);
    world.merge(MIDGAR, OTHER).unwrap();
    world.merge(OTHER, THIRD).unwrap();
    world.d1.run(
        "INSERT INTO wallet_summary (app_id, midgar_id, balance, purchased, spent, updated_at) VALUES (?1, ?2, 40, 0, 0, '2026-10-01T00:00:00+00:00')",
        &[APP.into(), MIDGAR.into()],
    );
    let replay = world.merge(MIDGAR, OTHER).unwrap();
    assert_eq!(replay, Merged::Moved { target: THIRD.to_string(), tenants: 2, credits: 47 });
    assert_eq!(world.wallets.balance(THIRD, APP), 150, "100 of its own, 10 of the middle one, 40 of the first, once");
    assert_eq!(entries_of(&world, THIRD, APP, &format!("merge:{MIDGAR}:{APP}")), 0, "the first merge's entries stayed in the middle wallet");
    assert_eq!(entries_of(&world, OTHER, APP, &format!("merge:{MIDGAR}:{APP}")), 1);
    assert_eq!(world.rows("wallet_links", THIRD), 4);
}

#[test]
fn merges_delivered_in_the_wrong_order_end_in_the_same_place() {
    let in_order = World::two_wallets();
    in_order.link(APP, "u4", THIRD);
    in_order.credit(THIRD, APP, "tx:4", 100);
    in_order.merge(MIDGAR, OTHER).unwrap();
    in_order.merge(OTHER, THIRD).unwrap();

    let reversed = World::two_wallets();
    reversed.link(APP, "u4", THIRD);
    reversed.credit(THIRD, APP, "tx:4", 100);
    reversed.merge(OTHER, THIRD).unwrap();
    reversed.merge(MIDGAR, OTHER).unwrap();

    for world in [&in_order, &reversed] {
        assert_eq!(world.wallets.balance(THIRD, APP), 150);
        assert_eq!(world.wallets.balance(THIRD, OTHER_APP), 7);
        assert_eq!(world.rows("wallet_links", THIRD), 4);
        assert!(world.wallets.verified(THIRD));
    }
}

#[test]
fn a_merge_into_a_deleted_survivor_erases_the_merged_away_wallet() {
    let world = World::two_wallets();
    world.erase(OTHER).unwrap();
    assert_eq!(world.merge(MIDGAR, OTHER).unwrap(), Merged::Discarded);
    assert_eq!((world.wallets.balance(MIDGAR, APP), world.wallets.balance(OTHER, APP)), (0, 0));
    assert_eq!((world.rows("wallet_links", MIDGAR), world.rows("wallet_summary", MIDGAR)), (0, 0));
    assert_eq!(state(&world, MIDGAR)["erased"], true);
}

#[test]
fn a_survivor_deleted_in_the_middle_of_the_merge_discards_the_rest() {
    let world = World::two_wallets();
    world.wallets.call(MIDGAR, "POST", "/seal", Some(&json!({"into": OTHER})));
    world.erase(OTHER).unwrap();
    assert_eq!(world.merge(MIDGAR, OTHER).unwrap(), Merged::Discarded);
    assert_eq!(world.wallets.balance(MIDGAR, APP), 0);
}

#[test]
fn deleting_the_survivor_erases_the_wallets_merged_into_it_too() {
    let world = World::two_wallets();
    world.link(APP, "u4", THIRD);
    world.credit(THIRD, APP, "tx:4", 100);
    world.merge(MIDGAR, OTHER).unwrap();
    world.merge(OTHER, THIRD).unwrap();
    assert_eq!(world.wallets.balance(MIDGAR, APP), 40, "the archive of a sealed wallet is still in it");
    let erasure = world.erase(THIRD).unwrap();
    assert!(matches!(erasure, Erasure::Erased { .. }));
    for id in [MIDGAR, OTHER, THIRD] {
        assert_eq!(state(&world, id)["erased"], true, "{id}");
        assert_eq!(world.wallets.balance(id, APP), 0, "{id}");
    }
    assert_eq!(world.rows("wallet_links", THIRD), 0);
}

#[test]
fn a_crash_while_erasing_a_merged_tree_is_finished_by_the_retry() {
    let mut finished = false;
    for ticks in 0..30 {
        let world = World::two_wallets();
        world.merge(MIDGAR, OTHER).unwrap();
        let budget = Budget::of(ticks);
        let (d1, wallets) = (Flaky { inner: &world.d1, budget: &budget }, Flaky { inner: &world.wallets, budget: &budget });
        finished = block_on(erase_all(&d1, &wallets, OTHER)).is_ok();
        budget.lift();
        world.erase(OTHER).unwrap();
        for id in [MIDGAR, OTHER] {
            assert_eq!(state(&world, id)["erased"], true, "ticks {ticks} {id}");
            assert_eq!(world.wallets.balance(id, APP), 0, "ticks {ticks} {id}");
        }
        assert_eq!(world.rows("wallet_links", OTHER), 0, "ticks {ticks}");
        if finished {
            break;
        }
    }
    assert!(finished);
}

#[test]
fn a_link_whose_credits_were_already_in_the_merged_away_wallet_does_not_move_them_again() {
    let world = World::two_wallets();
    world.d1.user(APP, "u5", 0);
    world.d1.run(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, moved_credits, welcome_part, linked_at)
         VALUES (?1, 'u5', ?2, 'moving', 9, 0, '2026-10-01T00:00:00+00:00')",
        &[APP.into(), MIDGAR.into()],
    );
    let moved = json!({"tenant": APP, "key": links::migrate_key(APP, "u5"), "moved": 9, "welcome_part": 0});
    assert_eq!(world.wallets.call(MIDGAR, "POST", "/migrate", Some(&moved)).0, 200, "the saga got as far as the wallet");
    world.merge(MIDGAR, OTHER).unwrap();
    assert_eq!(world.wallets.balance(OTHER, APP), 59, "10 of its own, 40 and the 9 the saga moved");
    let link = block_on(links::read(&world.d1, APP, "u5")).unwrap().unwrap();
    assert_eq!((link.state, link.midgar_id.as_str()), (LinkState::Linked, OTHER));
    let linker = Linker { db: &world.d1, wallets: &world.wallets };
    block_on(linker.complete(link)).unwrap();
    assert_eq!(world.wallets.balance(OTHER, APP), 59, "nothing is moved a second time");
}

#[test]
fn a_link_whose_saga_had_not_reached_the_wallet_finishes_into_the_survivor() {
    let world = World::two_wallets();
    world.d1.user(APP, "u5", 0);
    world.d1.run(
        "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, moved_credits, welcome_part, linked_at)
         VALUES (?1, 'u5', ?2, 'moving', 9, 0, '2026-10-01T00:00:00+00:00')",
        &[APP.into(), MIDGAR.into()],
    );
    world.merge(MIDGAR, OTHER).unwrap();
    assert_eq!(world.wallets.balance(OTHER, APP), 50);
    let link = block_on(links::read(&world.d1, APP, "u5")).unwrap().unwrap();
    assert_eq!((link.state, link.midgar_id.as_str()), (LinkState::Moving, OTHER));
    let linker = Linker { db: &world.d1, wallets: &world.wallets };
    block_on(linker.complete(link)).unwrap();
    assert_eq!(world.wallets.balance(OTHER, APP), 59);
    assert_eq!(world.wallets.balance(MIDGAR, APP), 40, "the sealed wallet gained nothing");
}

#[test]
fn a_residue_move_already_in_the_merged_away_wallet_is_finished_not_repeated() {
    let world = World::two_wallets();
    world.d1.run("UPDATE wallet_links SET residue_pending = 3 WHERE user_id = 'u1'", &[]);
    let moved = json!({"tenant": APP, "key": links::residue_key(APP, "u1", 0), "moved": 3, "welcome_part": 0});
    assert_eq!(world.wallets.call(MIDGAR, "POST", "/migrate", Some(&moved)).0, 200);
    world.merge(MIDGAR, OTHER).unwrap();
    let link = block_on(links::read(&world.d1, APP, "u1")).unwrap().unwrap();
    assert_eq!((link.residue_pending, link.residue_moves), (None, 1));
    assert_eq!(world.wallets.balance(OTHER, APP), 53);
}

#[test]
fn two_app_users_of_one_id_both_follow_it() {
    let world = World::two_wallets();
    world.link(APP, "u9", MIDGAR);
    world.merge(MIDGAR, OTHER).unwrap();
    assert_eq!(link_of(&world, APP, "u9"), OTHER);
    assert_eq!(world.rows("wallet_links", OTHER), 4);
}
