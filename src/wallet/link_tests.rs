use futures::executor::block_on;

use super::link::Linker;
use super::links::{self, LinkState};
use super::testing::{Budget, FakeWallets, Flaky, LocalD1};
use crate::error::AppError;
use crate::ledgers::Account;
use crate::midgar_token::{MidgarCaller, Tier};

const APP: &str = "dreameater";
const MIDGAR: &str = "0194f3a2-7b1c-4d2e-9a60-3c5e8f1b2d47";
const OTHER_MIDGAR: &str = "0195aaaa-7b1c-4d2e-9a60-3c5e8f1b2d47";
const WELCOME: i64 = 6;

fn caller(id: &str, tier: Tier) -> MidgarCaller {
    MidgarCaller { midgar_id: id.to_string(), tier }
}

fn attested() -> MidgarCaller {
    caller(MIDGAR, Tier::AppAttest)
}

struct World {
    d1: LocalD1,
    wallets: FakeWallets,
}

impl World {
    fn new() -> Self {
        let d1 = LocalD1::migrated();
        d1.run("UPDATE apps SET new_user_free_credits = ?1 WHERE app_id = ?2", &[WELCOME.into(), APP.into()]);
        Self { d1, wallets: FakeWallets::new() }
    }

    fn with_user(user: &str, balance: i64, welcome: i64) -> Self {
        let world = Self::new();
        world.add_user(user, balance, welcome);
        world
    }

    fn add_user(&self, user: &str, balance: i64, welcome: i64) {
        self.d1.user(APP, user, balance);
        if welcome > 0 {
            self.d1.transaction(APP, user, "bonus", welcome, "Welcome bonus");
        }
    }

    fn linker(&self) -> Linker<'_, LocalD1, FakeWallets> {
        Linker { db: &self.d1, wallets: &self.wallets }
    }

    fn link(&self, user: &str) -> links::LinkRow {
        block_on(self.linker().lookup(APP, user)).unwrap().expect("link row")
    }

    fn migrate_entries(&self, midgar: &str) -> usize {
        let (_, body) = self.wallets.call(midgar, "GET", &format!("/entries?tenant={APP}&limit=200"), None);
        body["entries"].as_array().unwrap().iter().filter(|e| e["kind"] == "migrate").count()
    }

    fn start_moving(&self, user: &str, midgar: &str) {
        block_on(links::insert_moving(&self.d1, APP, user, midgar, "2099-01-01T00:00:00+00:00")).unwrap();
    }

    fn age_link(&self, user: &str, linked_at: &str) {
        self.d1.run("UPDATE wallet_links SET linked_at = ?1 WHERE user_id = ?2", &[linked_at.into(), user.into()]);
    }
}

fn wallet_account(user: &str) -> Account {
    Account::Wallet { midgar_id: MIDGAR.into(), app_id: APP.into(), user_id: user.into() }
}

#[test]
fn a_legacy_balance_moves_into_the_wallet_exactly_once() {
    let world = World::with_user("u1", 40, WELCOME);
    let account = block_on(world.linker().ensure_linked(APP, "u1", &attested())).unwrap();
    assert_eq!(account, wallet_account("u1"));
    assert_eq!(world.wallets.balance(MIDGAR, APP), 40);
    assert_eq!(world.d1.d1_balance(APP, "u1"), 0);
    let link = world.link("u1");
    assert_eq!((link.state, link.moved_credits, link.welcome_part), (LinkState::Linked, Some(40), Some(WELCOME)));
    assert_eq!(world.d1.int("SELECT balance FROM wallet_summary WHERE app_id = ?1 AND midgar_id = ?2", &[APP.into(), MIDGAR.into()]), 40);

    let again = block_on(world.linker().ensure_linked(APP, "u1", &attested())).unwrap();
    assert_eq!(again, account);
    assert_eq!(world.wallets.count("POST /migrate"), 1);
    assert_eq!(world.wallets.balance(MIDGAR, APP), 40);
    assert!(world.wallets.verified(MIDGAR));
}

fn assert_moved_once(world: &World, user: &str, expected_wallet: i64) {
    let link = world.link(user);
    assert_eq!(link.state, LinkState::Linked, "the link finished");
    assert_eq!(world.d1.d1_balance(APP, user), 0, "nothing is left in D1");
    assert_eq!(world.wallets.balance(MIDGAR, APP), expected_wallet, "the wallet holds the balance once");
    assert_eq!(world.migrate_entries(MIDGAR), 1, "one migrate entry");
    assert!(world.wallets.verified(MIDGAR));
}

#[test]
fn a_crash_after_every_write_of_the_saga_still_moves_the_balance_exactly_once() {
    let mut crashes_seen = 0;
    let mut finished_clean = false;
    for ticks in 0..80 {
        let world = World::with_user("u1", 40, WELCOME);
        let budget = Budget::of(ticks);
        let (d1, wallets) = (Flaky { inner: &world.d1, budget: &budget }, Flaky { inner: &world.wallets, budget: &budget });
        let first = block_on(Linker { db: &d1, wallets: &wallets }.ensure_linked(APP, "u1", &attested()));
        if first.is_err() {
            crashes_seen += 1;
        } else {
            finished_clean = true;
        }
        budget.lift();
        let recovered = block_on(world.linker().ensure_linked(APP, "u1", &attested())).unwrap();
        assert_eq!(recovered, wallet_account("u1"), "ticks {ticks}");
        assert_moved_once(&world, "u1", 40);
        if finished_clean {
            break;
        }
    }
    assert!(crashes_seen >= 8, "the injector walked through the saga's writes ({crashes_seen})");
    assert!(finished_clean);
}

#[test]
fn the_cron_finishes_a_saga_that_crashed_at_any_point() {
    for ticks in 0..14 {
        let world = World::with_user("u1", 40, WELCOME);
        let budget = Budget::of(ticks);
        let (d1, wallets) = (Flaky { inner: &world.d1, budget: &budget }, Flaky { inner: &world.wallets, budget: &budget });
        let _ = block_on(Linker { db: &d1, wallets: &wallets }.ensure_linked(APP, "u1", &attested()));
        budget.lift();
        if block_on(world.linker().lookup(APP, "u1")).unwrap().is_none() {
            continue;
        }
        world.age_link("u1", "2026-01-01T00:00:00+00:00");
        let resumed = block_on(world.linker().resume_stale(20)).unwrap();
        let already_done = world.link("u1").state == LinkState::Linked;
        assert!(already_done, "ticks {ticks}");
        assert!(resumed <= 1);
        assert_moved_once(&world, "u1", 40);
    }
}

#[test]
fn a_request_without_a_token_finishes_a_moving_link_from_the_row_alone() {
    let world = World::with_user("u1", 12, 0);
    world.start_moving("u1", MIDGAR);
    let account = block_on(world.linker().account_for_request(APP, "u1", None)).unwrap();
    assert_eq!(account, wallet_account("u1"));
    assert_moved_once(&world, "u1", 12);
}

#[test]
fn a_user_without_a_link_and_without_a_token_stays_local() {
    let world = World::with_user("u1", 12, 0);
    let account = block_on(world.linker().account_for_request(APP, "u1", None)).unwrap();
    assert_eq!(account, Account::local(APP, "u1"));
    assert_eq!(world.d1.d1_balance(APP, "u1"), 12);
    assert_eq!(world.wallets.calls.borrow().len(), 0);
    assert!(block_on(world.linker().relink(APP, "u1")).unwrap().is_none());
}

#[test]
fn the_cron_resumes_only_stale_moving_rows_and_only_up_to_the_limit() {
    let world = World::new();
    for n in 0..3 {
        let user = format!("old{n}");
        world.add_user(&user, 5, 0);
        world.start_moving(&user, &format!("{MIDGAR}-{n}"));
        world.age_link(&user, "2026-01-01T00:00:00+00:00");
    }
    world.add_user("fresh", 5, 0);
    world.start_moving("fresh", OTHER_MIDGAR);
    assert_eq!(block_on(world.linker().resume_stale(2)).unwrap(), 2);
    assert_eq!(block_on(world.linker().resume_stale(20)).unwrap(), 1);
    assert_eq!(world.link("fresh").state, LinkState::Moving);
    assert_eq!(block_on(world.linker().resume_stale(20)).unwrap(), 0);
}

#[test]
fn a_reinstall_counts_the_welcome_bonus_once() {
    let world = World::with_user("device-a", 6, WELCOME);
    block_on(world.linker().ensure_linked(APP, "device-a", &attested())).unwrap();
    assert_eq!(world.wallets.balance(MIDGAR, APP), 6);

    world.add_user("device-b", 6, WELCOME);
    block_on(world.linker().ensure_linked(APP, "device-b", &attested())).unwrap();
    assert_eq!(world.wallets.balance(MIDGAR, APP), 6, "the second welcome bonus is not counted");
    assert_eq!(world.d1.d1_balance(APP, "device-b"), 0);
    let second = world.link("device-b");
    assert_eq!((second.moved_credits, second.welcome_part), (Some(6), Some(WELCOME)));

    world.add_user("device-c", 6 + 20, WELCOME);
    block_on(world.linker().ensure_linked(APP, "device-c", &attested())).unwrap();
    assert_eq!(world.wallets.balance(MIDGAR, APP), 6 + 20, "real purchased credits of every user are kept");
    assert_eq!(world.migrate_entries(MIDGAR), 3);
    assert!(world.wallets.verified(MIDGAR));
}

#[test]
fn a_welcome_bonus_larger_than_the_balance_is_clamped_at_the_balance() {
    let world = World::with_user("device-a", 6, WELCOME);
    block_on(world.linker().ensure_linked(APP, "device-a", &attested())).unwrap();
    world.add_user("device-b", 2, WELCOME);
    block_on(world.linker().ensure_linked(APP, "device-b", &attested())).unwrap();
    assert_eq!(world.wallets.balance(MIDGAR, APP), 6);
}

#[test]
fn a_native_welcome_then_a_legacy_link_does_not_grant_the_bonus_twice() {
    let world = World::new();
    let native = block_on(world.linker().native_account(APP, &attested())).unwrap();
    assert_eq!(world.wallets.balance(MIDGAR, APP), WELCOME);
    let Account::Wallet { user_id: native_user, .. } = native else { panic!("expected a wallet account") };

    world.add_user("legacy", WELCOME, WELCOME);
    block_on(world.linker().ensure_linked(APP, "legacy", &attested())).unwrap();
    assert_eq!(world.wallets.balance(MIDGAR, APP), WELCOME, "the legacy welcome bonus was already claimed");
    assert_eq!(world.d1.d1_balance(APP, "legacy"), 0);
    assert_ne!(native_user, "legacy");
}

#[test]
fn a_legacy_link_then_a_native_request_finds_the_legacy_user_and_grants_no_welcome() {
    let world = World::with_user("legacy", 9, WELCOME);
    block_on(world.linker().ensure_linked(APP, "legacy", &attested())).unwrap();
    let native = block_on(world.linker().native_account(APP, &attested())).unwrap();
    assert_eq!(native, wallet_account("legacy"));
    assert_eq!(world.wallets.balance(MIDGAR, APP), 9);
    assert_eq!(world.d1.int("SELECT COUNT(*) FROM users WHERE provider = 'midgar'", &[]), 0);
}

#[test]
fn a_native_request_creates_a_midgar_user_and_a_linked_row_with_the_welcome_for_an_attested_device() {
    let world = World::new();
    let account = block_on(world.linker().native_account(APP, &attested())).unwrap();
    let Account::Wallet { midgar_id, app_id, user_id } = account.clone() else { panic!("expected a wallet account") };
    assert_eq!((midgar_id.as_str(), app_id.as_str()), (MIDGAR, APP));
    assert_eq!(world.d1.int("SELECT COUNT(*) FROM users WHERE id = ?1 AND provider = 'midgar' AND provider_id = ?2 AND api_key_hash IS NULL", &[user_id.as_str().into(), MIDGAR.into()]), 1);
    let link = world.link(&user_id);
    assert_eq!((link.state, link.moved_credits), (LinkState::Linked, Some(0)));
    assert_eq!(world.wallets.balance(MIDGAR, APP), WELCOME);

    let again = block_on(world.linker().native_account(APP, &attested())).unwrap();
    assert_eq!(again, account);
    assert_eq!(world.d1.int("SELECT COUNT(*) FROM users WHERE provider = 'midgar'", &[]), 1);
    assert_eq!(world.wallets.balance(MIDGAR, APP), WELCOME, "a second request grants nothing");
}

#[test]
fn a_native_request_for_a_tenant_that_does_not_exist_creates_nothing() {
    let world = World::new();
    let result = block_on(world.linker().native_account("dream-eater", &attested()));
    assert!(matches!(result, Err(AppError::Unauthorized(_))));
    assert_eq!(world.d1.int("SELECT COUNT(*) FROM users WHERE provider = 'midgar'", &[]), 0);
    assert_eq!(world.d1.int("SELECT COUNT(*) FROM wallet_links", &[]), 0);
    assert_eq!(world.wallets.calls.borrow().len(), 0, "no welcome bonus either");
}

#[test]
fn only_an_attested_device_gets_the_welcome_bonus() {
    for tier in [Tier::Development, Tier::Unverified] {
        let world = World::new();
        block_on(world.linker().native_account(APP, &caller(MIDGAR, tier))).unwrap();
        assert_eq!(world.wallets.balance(MIDGAR, APP), 0, "{tier:?}");
        assert_eq!(world.d1.int("SELECT COUNT(*) FROM wallet_links WHERE midgar_id = ?1", &[MIDGAR.into()]), 1);
    }
}

#[test]
fn a_tenant_with_no_welcome_credits_grants_none() {
    let world = World::new();
    world.d1.run("UPDATE apps SET new_user_free_credits = 0 WHERE app_id = ?1", &[APP.into()]);
    block_on(world.linker().native_account(APP, &attested())).unwrap();
    assert_eq!(world.wallets.balance(MIDGAR, APP), 0);
}

#[test]
fn a_crash_while_creating_a_native_user_never_loses_or_doubles_the_welcome() {
    for ticks in 0..14 {
        let world = World::new();
        let budget = Budget::of(ticks);
        let (d1, wallets) = (Flaky { inner: &world.d1, budget: &budget }, Flaky { inner: &world.wallets, budget: &budget });
        let _ = block_on(Linker { db: &d1, wallets: &wallets }.native_account(APP, &attested()));
        budget.lift();
        let account = block_on(world.linker().native_account(APP, &attested())).unwrap();
        assert!(matches!(account, Account::Wallet { .. }), "ticks {ticks}");
        assert_eq!(world.wallets.balance(MIDGAR, APP), WELCOME, "ticks {ticks}");
        assert_eq!(world.d1.int("SELECT COUNT(*) FROM wallet_links WHERE midgar_id = ?1", &[MIDGAR.into()]), 1, "ticks {ticks}");
        assert!(world.wallets.verified(MIDGAR));
    }
}

#[test]
fn a_user_presenting_another_midgar_id_stays_on_its_link() {
    let world = World::with_user("u1", 10, 0);
    block_on(world.linker().ensure_linked(APP, "u1", &attested())).unwrap();
    let account = block_on(world.linker().ensure_linked(APP, "u1", &caller(OTHER_MIDGAR, Tier::AppAttest))).unwrap();
    assert_eq!(account, wallet_account("u1"));
    assert_eq!(world.link("u1").midgar_id, MIDGAR);
    assert_eq!(world.d1.int("SELECT COUNT(*) FROM wallet_links", &[]), 1);
    assert_eq!(world.wallets.balance(OTHER_MIDGAR, APP), 0);
    assert_eq!(world.wallets.balance(MIDGAR, APP), 10);
}

#[test]
fn a_mismatch_while_moving_finishes_the_saga_for_the_stored_midgar_id() {
    let world = World::with_user("u1", 10, 0);
    world.start_moving("u1", MIDGAR);
    let account = block_on(world.linker().account_for_request(APP, "u1", Some(&caller(OTHER_MIDGAR, Tier::AppAttest)))).unwrap();
    assert_eq!(account, wallet_account("u1"));
    assert_eq!(world.wallets.balance(MIDGAR, APP), 10);
    assert_eq!(world.wallets.balance(OTHER_MIDGAR, APP), 0);
}

#[test]
fn a_user_without_a_balance_row_links_with_nothing_to_move() {
    let world = World::new();
    world.d1.run(
        "INSERT INTO users (id, provider, provider_id, name, created_at, updated_at, app_id) VALUES ('bare', 'anonymous', 'bare', 'T', 'x', 'x', ?1)",
        &[APP.into()],
    );
    block_on(world.linker().ensure_linked(APP, "bare", &attested())).unwrap();
    let link = world.link("bare");
    assert_eq!((link.state, link.moved_credits, link.welcome_part), (LinkState::Linked, Some(0), Some(0)));
    assert_eq!(world.wallets.balance(MIDGAR, APP), 0);
}

#[test]
fn a_concurrent_second_freeze_cannot_swallow_a_credit_of_the_same_size() {
    let world = World::with_user("u1", 5, 0);
    block_on(world.linker().ensure_linked(APP, "u1", &attested())).unwrap();
    let stale_stamp = block_on(links::stamp_of(&world.d1, APP, "u1")).unwrap();
    world.d1.run("UPDATE user_credits SET balance = 5, updated_at = '2026-10-17T10:00:00+00:00' WHERE user_id = 'u1'", &[]);
    block_on(links::freeze_at(&world.d1, APP, "u1", "2026-10-17T10:00:01+00:00", &stale_stamp)).unwrap();
    assert_eq!(world.d1_balance("u1"), 5, "the late duplicate freeze changed nothing");
}

impl World {
    fn d1_balance(&self, user: &str) -> i64 {
        self.d1.d1_balance(APP, user)
    }

    fn residue_pending(&self, user: &str) -> Option<i64> {
        self.link(user).residue_pending
    }

    fn land_in_frozen_row(&self, user: &str, credits: i64) {
        let stamp = format!("2026-10-17T10:{:02}:00+00:00", self.d1.int("SELECT COUNT(*) FROM credit_transactions", &[]) % 59);
        self.d1.run(
            "UPDATE user_credits SET balance = balance + ?1, updated_at = ?2 WHERE user_id = ?3",
            &[credits.into(), stamp.as_str().into(), user.into()],
        );
        self.d1.transaction(APP, user, "refund", credits, "late refund");
    }
}

#[test]
fn credits_that_land_in_the_frozen_row_move_to_the_wallet_once_per_sweep() {
    let world = World::with_user("u1", 40, 0);
    block_on(world.linker().ensure_linked(APP, "u1", &attested())).unwrap();
    assert_eq!(block_on(world.linker().sweep_residue(20)).unwrap(), 0, "nothing to sweep yet");

    world.land_in_frozen_row("u1", 7);
    assert_eq!(block_on(world.linker().sweep_residue(20)).unwrap(), 1);
    assert_eq!(world.wallets.balance(MIDGAR, APP), 47);
    assert_eq!((world.d1_balance("u1"), world.residue_pending("u1"), world.link("u1").residue_moves), (0, None, 1));
    assert_eq!(block_on(world.linker().sweep_residue(20)).unwrap(), 0);
    assert_eq!(world.wallets.balance(MIDGAR, APP), 47);

    world.land_in_frozen_row("u1", 3);
    assert_eq!(block_on(world.linker().sweep_residue(20)).unwrap(), 1);
    assert_eq!(world.wallets.balance(MIDGAR, APP), 50);
    assert_eq!(world.link("u1").residue_moves, 2);
    assert_eq!(world.migrate_entries(MIDGAR), 3);
    assert!(world.wallets.verified(MIDGAR));
}

#[test]
fn the_residue_sweep_survives_a_crash_after_every_write() {
    let mut crashes = 0;
    for ticks in 0..40 {
        let world = World::with_user("u1", 40, 0);
        block_on(world.linker().ensure_linked(APP, "u1", &attested())).unwrap();
        world.land_in_frozen_row("u1", 7);
        let budget = Budget::of(ticks);
        let (d1, wallets) = (Flaky { inner: &world.d1, budget: &budget }, Flaky { inner: &world.wallets, budget: &budget });
        let _ = block_on(Linker { db: &d1, wallets: &wallets }.sweep_residue(20));
        crashes += budget.exhausted() as usize;
        budget.lift();
        block_on(world.linker().sweep_residue(20)).unwrap();
        block_on(world.linker().sweep_residue(20)).unwrap();
        assert_eq!(world.wallets.balance(MIDGAR, APP), 47, "ticks {ticks}");
        assert_eq!((world.d1_balance("u1"), world.residue_pending("u1"), world.link("u1").residue_moves), (0, None, 1), "ticks {ticks}");
        assert!(world.wallets.verified(MIDGAR));
        if !budget.exhausted() && ticks > 12 {
            break;
        }
    }
    assert!(crashes >= 4, "crashes injected: {crashes}");
}

#[test]
fn a_credit_that_lands_during_the_residue_freeze_is_kept_for_the_next_sweep() {
    let world = World::with_user("u1", 40, 0);
    block_on(world.linker().ensure_linked(APP, "u1", &attested())).unwrap();
    world.land_in_frozen_row("u1", 7);
    let candidates = block_on(links::residue_candidates(&world.d1, 20)).unwrap();
    assert_eq!(candidates.len(), 1);
    world.land_in_frozen_row("u1", 2);
    block_on(links::freeze_residue(&world.d1, APP, "u1", &candidates[0].stamp, "2026-10-17T11:00:00+00:00")).unwrap();
    assert_eq!((world.d1_balance("u1"), world.residue_pending("u1")), (9, None), "the freeze lost its race and did nothing");
    assert_eq!(block_on(world.linker().sweep_residue(20)).unwrap(), 1);
    assert_eq!(world.wallets.balance(MIDGAR, APP), 49);
}

#[test]
fn the_residue_sweep_is_limited_and_skips_users_without_residue() {
    let world = World::new();
    for n in 0..3 {
        let user = format!("u{n}");
        world.add_user(&user, 0, 0);
        block_on(world.linker().ensure_linked(APP, &user, &caller(&format!("{MIDGAR}-{n}"), Tier::AppAttest))).unwrap();
        world.land_in_frozen_row(&user, 4);
    }
    world.add_user("quiet", 0, 0);
    block_on(world.linker().ensure_linked(APP, "quiet", &caller(OTHER_MIDGAR, Tier::AppAttest))).unwrap();
    assert_eq!(block_on(world.linker().sweep_residue(2)).unwrap(), 2);
    assert_eq!(block_on(world.linker().sweep_residue(20)).unwrap(), 1);
    assert_eq!(block_on(world.linker().sweep_residue(20)).unwrap(), 0);
}

#[test]
fn a_wallet_that_is_down_fails_the_link_and_leaves_the_money_in_the_link_row() {
    struct Down;
    impl super::client::WalletTransport for Down {
        async fn send(&self, _: &str, _: &str, _: &str, _: Option<&serde_json::Value>) -> Result<(u16, serde_json::Value), AppError> {
            Err(AppError::WalletUnavailable)
        }
    }
    let world = World::with_user("u1", 40, 0);
    let result = block_on(Linker { db: &world.d1, wallets: &Down }.ensure_linked(APP, "u1", &attested()));
    assert!(matches!(result, Err(AppError::WalletUnavailable)));
    let link = world.link("u1");
    assert_eq!((link.state, link.moved_credits), (LinkState::Moving, Some(40)));
    assert_eq!(world.d1_balance("u1"), 0);
}

#[test]
fn rows_the_wallet_keeps_refusing_cannot_starve_the_links_behind_them() {
    struct Refuses<'a> {
        inner: &'a FakeWallets,
        midgar_prefix: &'static str,
    }
    impl super::client::WalletTransport for Refuses<'_> {
        async fn send(&self, midgar_id: &str, method: &str, path: &str, body: Option<&serde_json::Value>) -> Result<(u16, serde_json::Value), AppError> {
            if midgar_id.starts_with(self.midgar_prefix) {
                return Err(AppError::WalletUnavailable);
            }
            self.inner.send(midgar_id, method, path, body).await
        }
    }
    let world = World::new();
    for (n, user) in ["bad0", "bad1", "good0", "good1"].into_iter().enumerate() {
        world.add_user(user, 10, 0);
        let midgar = if user.starts_with("bad") { format!("bad-{n}") } else { format!("{MIDGAR}-{n}") };
        world.start_moving(user, &midgar);
        world.age_link(user, &format!("2026-01-01T00:00:0{n}+00:00"));
    }
    let refused = Linker { db: &world.d1, wallets: &Refuses { inner: &world.wallets, midgar_prefix: "bad-" } };
    for _ in 0..2 {
        block_on(refused.resume_stale(2)).unwrap();
    }
    assert_eq!(world.link("good0").state, LinkState::Linked);
    assert_eq!(world.link("good1").state, LinkState::Linked);
    for user in ["bad0", "bad1"] {
        let link = world.link(user);
        assert_eq!((link.state, link.moved_credits), (LinkState::Moving, Some(10)));
        assert_eq!(world.d1_balance(user), 0);
    }
    assert_eq!(world.wallets.balance(&format!("{MIDGAR}-2"), APP) + world.wallets.balance(&format!("{MIDGAR}-3"), APP), 20);
}

#[test]
fn a_link_the_wallet_refused_after_its_freeze_still_finishes_once_the_wallet_answers() {
    let world = World::new();
    for n in 0..3 {
        let user = format!("u{n}");
        world.add_user(&user, 10, 0);
        world.start_moving(&user, &format!("{MIDGAR}-{n}"));
        world.age_link(&user, &format!("2026-01-01T00:00:0{n}+00:00"));
    }
    for crash_after in 0..12 {
        let budget = Budget::of(crash_after);
        let flaky = Linker { db: &Flaky { inner: &world.d1, budget: &budget }, wallets: &Flaky { inner: &world.wallets, budget: &budget } };
        let _ = block_on(flaky.resume_stale(2));
    }
    while block_on(world.linker().resume_stale(2)).unwrap() > 0 {}
    for n in 0..3 {
        assert_eq!(world.link(&format!("u{n}")).state, LinkState::Linked);
        assert_eq!(world.wallets.balance(&format!("{MIDGAR}-{n}"), APP), 10);
        assert_eq!(world.d1_balance(&format!("u{n}")), 0);
        assert_eq!(world.migrate_entries(&format!("{MIDGAR}-{n}")), 1);
    }
}
