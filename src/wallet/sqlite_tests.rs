use rusqlite::Connection;
use serde_json::json;

use super::books::Books;
use super::ledger::apply;
use super::sql::{self, SqlBooks, SqlExec, Val};
use super::tests::{self as scenarios, call, entry, NOW};
use super::testing::Rusqlite;
use super::types::{Kind, WalletError};

/// A fresh object database with the real schema.
fn fresh() -> SqlBooks<Rusqlite> {
    let sql = Rusqlite(Connection::open_in_memory().unwrap());
    sql::init(&sql).unwrap();
    SqlBooks::new(sql)
}

/// Runs raw SQL against the books' connection.
fn raw(books: &SqlBooks<Rusqlite>, sql: &str, params: &[Val]) -> Result<Vec<Vec<Val>>, WalletError> {
    books_sql(books).run(sql, params)
}

/// The executor inside the books, reached through a second statement-free handle.
fn books_sql(books: &SqlBooks<Rusqlite>) -> &Rusqlite {
    books.executor()
}

#[test]
fn replay_returns_the_stored_result_on_sqlite() {
    scenarios::replay_returns_the_stored_result(&mut fresh());
}

#[test]
fn insufficient_funds_append_nothing_on_sqlite() {
    scenarios::insufficient_funds_append_nothing(&mut fresh());
}

#[test]
fn reverse_clamps_and_replays_on_sqlite() {
    scenarios::reverse_clamps_and_replays(&mut fresh());
    scenarios::reverse_takes_the_full_amount_when_it_fits(&mut fresh());
}

#[test]
fn tenants_are_isolated_on_sqlite() {
    scenarios::tenants_are_isolated(&mut fresh());
    scenarios::pool_tenant_is_rejected(&mut fresh());
}

#[test]
fn idempotency_conflicts_on_sqlite() {
    scenarios::idempotency_conflicts(&mut fresh());
    scenarios::invalid_entries_are_refused(&mut fresh());
}

#[test]
fn migrate_dedupes_the_welcome_on_sqlite() {
    scenarios::migrate_dedupes_the_welcome(&mut fresh());
    scenarios::a_welcome_grant_counts_as_the_claim(&mut fresh());
}

#[test]
fn routes_answer_the_documented_statuses_on_sqlite() {
    scenarios::routes_answer_the_documented_statuses(&mut fresh());
}

#[test]
fn entries_page_and_erase_on_sqlite() {
    scenarios::entries_page_and_erase(&mut fresh());
}

#[test]
fn the_merge_objects_behave_on_sqlite() {
    scenarios::a_sealed_wallet_refuses_writes_and_keeps_its_first_target(&mut fresh());
    scenarios::absorb_takes_all_tenants_once_and_records_the_source(&mut fresh());
    scenarios::a_bad_absorb_writes_nothing(&mut fresh());
    scenarios::a_sealed_survivor_answers_for_what_it_took_and_points_on(&mut fresh());
    scenarios::erase_all_clears_the_seal_and_the_sources(&mut fresh());
}

#[test]
fn erase_all_empties_the_wallet_for_good_on_sqlite() {
    scenarios::erase_all_empties_the_wallet_for_good(&mut fresh());
}

#[test]
fn verify_finds_a_tampered_balance_on_sqlite() {
    let mut books = fresh();
    apply(&mut books, &entry("tx:1", "app", Kind::Purchase, 10), NOW).unwrap();
    assert!(super::ledger::verify(&books).unwrap().ok);
    raw(&books, "UPDATE balances SET balance = 9 WHERE tenant = 'app'", &[]).unwrap();
    let report = super::ledger::verify(&books).unwrap();
    assert!(!report.ok);
    raw(&books, "UPDATE balances SET balance = 10 WHERE tenant = 'app'", &[]).unwrap();
    raw(&books, "UPDATE entries SET balance_after = 7 WHERE idem_key = 'tx:1'", &[]).unwrap();
    assert!(!super::ledger::verify(&books).unwrap().ok);
}

#[test]
fn the_schema_is_idempotent_and_stores_sharing_off() {
    let books = fresh();
    sql::init(books_sql(&books)).unwrap();
    let meta = raw(&books, "SELECT key, value FROM meta ORDER BY key", &[]).unwrap();
    assert_eq!(
        meta,
        vec![
            vec![Val::Text("schema".into()), Val::Text("1".into())],
            vec![Val::Text("sharing".into()), Val::Text("off".into())],
        ]
    );
}

#[test]
fn the_database_enforces_the_invariants_on_its_own() {
    let books = fresh();
    let insert = "INSERT INTO entries (idem_key, tenant, kind, amount, balance_after, source, description, at) VALUES (?, 'app', ?, 1, ?, 's', 'd', 1)";
    assert!(raw(&books, insert, &["k1".into(), "purchase".into(), 1.into()]).is_ok());
    assert!(raw(&books, insert, &["k1".into(), "purchase".into(), 1.into()]).is_err());
    assert!(raw(&books, insert, &["k2".into(), "gift".into(), 1.into()]).is_err());
    assert!(raw(&books, insert, &["k3".into(), "spend".into(), (-1).into()]).is_err());
    let bad_balance = "INSERT INTO balances (tenant, balance, updated_at) VALUES ('x', -1, 1)";
    assert!(raw(&books, bad_balance, &[]).is_err());
}

#[test]
fn a_failed_overdraw_leaves_the_database_untouched() {
    let mut books = fresh();
    apply(&mut books, &entry("tx:1", "app", Kind::Purchase, 3), NOW).unwrap();
    let rows_before = raw(&books, "SELECT COUNT(*) FROM entries", &[]).unwrap();
    let (status, _) = call(&mut books, "POST", "/spend", json!({"key":"s","tenant":"app","kind":"spend","amount":-4,"source":"u","description":"d"}));
    assert_eq!(status, 402);
    assert_eq!(raw(&books, "SELECT COUNT(*) FROM entries", &[]).unwrap(), rows_before);
    assert!(books.find("s").unwrap().is_none());
}

#[test]
fn two_spends_for_one_balance_are_strictly_ordered() {
    let mut books = fresh();
    apply(&mut books, &entry("tx:1", "app", Kind::Purchase, 5), NOW).unwrap();
    let results: Vec<u16> = (0..3)
        .map(|n| call(&mut books, "POST", "/spend", json!({"key":format!("s{n}"),"tenant":"app","kind":"spend","amount":-2,"source":"u","description":"d"})).0)
        .collect();
    assert_eq!(results, [200, 200, 402]);
    assert_eq!(books.balance("app").unwrap().balance, 1);
}

mod migration_030 {
    use super::*;

    const FIRST_WALLET_MIGRATION: &str = "030_wallet_links.sql";

    /// Replays every migration in order on SQLite, optionally stopping before 030.
    fn replay(include_wallet: bool) -> Connection {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".sql") && (include_wallet || n != FIRST_WALLET_MIGRATION))
            .collect();
        names.sort();
        let db = Connection::open_in_memory().unwrap();
        for name in names {
            let sql = std::fs::read_to_string(dir.join(&name)).unwrap();
            db.execute_batch(&sql).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        db
    }

    fn wallet_migration() -> String {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations").join(FIRST_WALLET_MIGRATION)).unwrap()
    }

    fn slugs(db: &Connection) -> Vec<(String, Option<String>)> {
        let mut statement = db.prepare("SELECT app_id, registry_slug FROM apps ORDER BY app_id").unwrap();
        statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    }

    #[test]
    fn it_backfills_the_registry_slug_for_every_app() {
        let db = replay(false);
        let expected = [
            ("doublekick", "double-kick"),
            ("dreameater", "dream-eater"),
            ("livingdex", "living-dex"),
            ("payday", "pay-day"),
            ("pixie", "pixiepocket"),
            ("helia", "helia"),
            ("psywave", "psywave"),
            ("newapp", "newapp"),
        ];
        for (app_id, _) in expected {
            db.execute("INSERT OR IGNORE INTO apps (app_id, name) VALUES (?1, ?1)", [app_id]).unwrap();
        }
        db.execute_batch(&wallet_migration()).unwrap();
        let apps = slugs(&db);
        for (app_id, slug) in expected {
            let found = apps.iter().find(|(id, _)| id == app_id).unwrap_or_else(|| panic!("{app_id} missing"));
            assert_eq!(found.1.as_deref(), Some(slug), "{app_id}");
        }
        assert!(apps.iter().all(|(_, slug)| slug.is_some()), "{apps:?}");
    }

    #[test]
    fn it_creates_the_link_table_with_its_constraints() {
        let db = replay(true);
        let insert = "INSERT INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, ?2, 'mid', ?3, 'now')";
        db.execute(insert, ["pixie", "u1", "moving"]).unwrap();
        assert!(db.execute(insert, ["pixie", "u1", "linked"]).is_err());
        assert!(db.execute(insert, ["pixie", "u2", "done"]).is_err());
        let row: (i64, Option<i64>, Option<i64>) = db
            .query_row("SELECT residue_moves, residue_pending, moved_credits FROM wallet_links", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!(row, (0, None, None));
        let index: String = db
            .query_row("SELECT sql FROM sqlite_master WHERE name = 'idx_wallet_links_midgar'", [], |r| r.get(0))
            .unwrap();
        assert!(index.contains("midgar_id, app_id, linked_at"));
    }

    #[test]
    fn it_creates_the_summary_mirror() {
        let db = replay(true);
        let insert = "INSERT INTO wallet_summary (app_id, midgar_id, balance, purchased, spent, updated_at) VALUES ('pixie', 'mid', 1, 2, 3, 'now')";
        db.execute(insert, []).unwrap();
        assert!(db.execute(insert, []).is_err());
    }

    #[test]
    fn it_preserves_existing_rows_and_applies_to_an_older_state() {
        let db = replay(false);
        let before: i64 = db.query_row("SELECT COUNT(*) FROM apps", [], |r| r.get(0)).unwrap();
        db.execute_batch(&wallet_migration()).unwrap();
        let after: i64 = db.query_row("SELECT COUNT(*) FROM apps WHERE registry_slug IS NOT NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(before, after);
    }
}
