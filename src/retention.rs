use serde_json::Value;
use worker::{console_error, console_log, D1Database, Env};

use crate::error::AppError;
use crate::wallet::d1::{D1Exec, Stmt};
use crate::wallet::Val;

pub const PURGE_BATCH: i64 = 200;

const DELETE_CHUNK: usize = 50;

/// The oldest expired images first, only for tenants that opted in with `apps.purge_expired_images = 1`.
/// Timestamps are RFC 3339 in UTC, so comparing them as text orders them in time.
pub const SELECT_EXPIRED_SQL: &str = "SELECT id, r2_key FROM stored_images
     WHERE expires_at <= ?1
       AND app_id IN (SELECT app_id FROM apps WHERE purge_expired_images = 1)
     ORDER BY expires_at
     LIMIT ?2";

/// An expired image waiting to be purged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredImage {
    pub id: String,
    pub r2_key: String,
}

/// What one purge run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    pub selected: usize,
    pub rows_deleted: usize,
    pub failed: Vec<(String, String)>,
}

/// Where the image bytes live; production is the R2 bucket, the host tests use a stand-in that can fail per key.
#[allow(async_fn_in_trait)]
pub trait ObjectStore {
    /// Deletes one object. A missing object is a success.
    async fn delete_object(&self, key: &str) -> Result<(), AppError>;
}

impl ObjectStore for worker::Bucket {
    async fn delete_object(&self, key: &str) -> Result<(), AppError> {
        self.delete(key).await.map_err(AppError::from)
    }
}

/// Reads the id and key out of the selected rows, dropping any row that lacks either.
pub fn parse_expired(rows: &[Value]) -> Vec<ExpiredImage> {
    rows.iter()
        .filter_map(|row| {
            let id = row.get("id")?.as_str()?;
            let r2_key = row.get("r2_key")?.as_str()?;
            Some(ExpiredImage { id: id.to_string(), r2_key: r2_key.to_string() })
        })
        .collect()
}

/// The statements that remove the given images' report rows and then the images themselves, in chunks small
/// enough for D1's bound-parameter limit.
pub fn delete_statements(ids: &[String]) -> Vec<Stmt> {
    ids.chunks(DELETE_CHUNK)
        .flat_map(|chunk| {
            let marks = (1..=chunk.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(", ");
            let params: Vec<Val> = chunk.iter().map(|id| Val::from(id.as_str())).collect();
            [
                Stmt::new(&format!("DELETE FROM image_reports WHERE image_id IN ({marks})"), params.clone()),
                Stmt::new(&format!("DELETE FROM stored_images WHERE id IN ({marks})"), params),
            ]
        })
        .collect()
}

/// Deletes up to `limit` expired images as of `now` (an RFC 3339 UTC timestamp): the R2 object first, then
/// the report and image rows of every image whose object is gone. A failed object delete is recorded in the
/// report, leaves that image's rows for the next run and does not stop the others.
pub async fn purge_expired<D: D1Exec, S: ObjectStore>(db: &D, store: &S, now: &str, limit: i64) -> Result<PurgeReport, AppError> {
    let rows = db.rows(SELECT_EXPIRED_SQL, &[Val::from(now), Val::Int(limit)]).await?;
    let expired = parse_expired(&rows);
    let mut report = PurgeReport { selected: expired.len(), ..PurgeReport::default() };

    let mut removed = Vec::with_capacity(expired.len());
    for image in &expired {
        match store.delete_object(&image.r2_key).await {
            Ok(()) => removed.push(image.id.clone()),
            Err(e) => report.failed.push((image.r2_key.clone(), format!("{e:?}"))),
        }
    }

    if !removed.is_empty() {
        db.batch(&delete_statements(&removed)).await?;
        report.rows_deleted = removed.len();
    }
    Ok(report)
}

/// The scheduled entry point: purges one bounded batch of expired images and logs the outcome.
pub async fn run_scheduled(env: &Env, db: &D1Database) {
    let bucket = match env.bucket("IMAGES") {
        Ok(bucket) => bucket,
        Err(e) => {
            console_error!("image purge could not open the R2 bucket: {:?}", e);
            return;
        }
    };
    let now = chrono::Utc::now().to_rfc3339();
    match purge_expired(db, &bucket, &now, PURGE_BATCH).await {
        Ok(report) => {
            for (key, error) in &report.failed {
                console_error!("image purge could not delete R2 object {}: {}", key, error);
            }
            if report.selected > 0 {
                console_log!(
                    "image purge selected {} expired images, deleted {} rows, {} object deletes failed",
                    report.selected,
                    report.rows_deleted,
                    report.failed.len()
                );
            }
        }
        Err(e) => console_error!("image purge failed: {:?}", e),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use futures::executor::block_on;

    use super::*;
    use crate::wallet::testing::LocalD1;

    struct FakeBucket {
        deleted: RefCell<Vec<String>>,
        failing: HashSet<String>,
    }

    impl FakeBucket {
        fn new() -> Self {
            Self { deleted: RefCell::new(Vec::new()), failing: HashSet::new() }
        }

        fn failing_on(keys: &[&str]) -> Self {
            Self { deleted: RefCell::new(Vec::new()), failing: keys.iter().map(|k| k.to_string()).collect() }
        }
    }

    impl ObjectStore for FakeBucket {
        async fn delete_object(&self, key: &str) -> Result<(), AppError> {
            if self.failing.contains(key) {
                return Err(AppError::InternalError("r2 down".to_string()));
            }
            self.deleted.borrow_mut().push(key.to_string());
            Ok(())
        }
    }

    const NOW: &str = "2026-10-23T12:00:00+00:00";

    fn add_image(db: &LocalD1, id: &str, app: &str, expires_at: &str) {
        db.run(
            "INSERT INTO stored_images (id, app_id, user_id, r2_key, prompt, model, size, created_at, expires_at)
             VALUES (?1, ?2, 'u', ?3, 'p', 'm', '1024x1024', '2026-10-01T00:00:00+00:00', ?4)",
            &[id.into(), app.into(), format!("u/{id}.png").into(), expires_at.into()],
        );
    }

    fn images(db: &LocalD1) -> i64 {
        db.int("SELECT COUNT(*) FROM stored_images", &[])
    }

    fn database() -> LocalD1 {
        let db = LocalD1::migrated();
        db.run("INSERT INTO apps (app_id, name) VALUES ('scrubme', 'Scrub Me')", &[]);
        db.run("UPDATE apps SET purge_expired_images = 1 WHERE app_id = 'scrubme'", &[]);
        db.user("scrubme", "u", 0);
        db
    }

    fn selected(db: &LocalD1) -> usize {
        block_on(db.rows(SELECT_EXPIRED_SQL, &[Val::from(NOW), Val::Int(10)])).unwrap().len()
    }

    #[test]
    fn only_expired_images_are_selected_oldest_first() {
        let db = database();
        add_image(&db, "newer", "scrubme", "2026-10-22T00:00:00.123456789+00:00");
        add_image(&db, "older", "scrubme", "2026-10-20T00:00:00+00:00");
        add_image(&db, "fresh", "scrubme", "2026-10-30T00:00:00+00:00");
        let rows = block_on(db.rows(SELECT_EXPIRED_SQL, &[Val::from(NOW), Val::Int(10)])).unwrap();
        let ids: Vec<String> = parse_expired(&rows).into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec!["older", "newer"]);
    }

    #[test]
    fn an_image_expiring_in_the_same_second_with_a_fraction_is_after_now() {
        let db = database();
        add_image(&db, "later-this-second", "scrubme", "2026-10-23T12:00:00.5+00:00");
        add_image(&db, "exactly-now", "scrubme", NOW);
        let rows = block_on(db.rows(SELECT_EXPIRED_SQL, &[Val::from(NOW), Val::Int(10)])).unwrap();
        let ids: Vec<String> = parse_expired(&rows).into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec!["exactly-now"]);
    }

    #[test]
    fn no_tenant_is_opted_in_by_the_migration() {
        let db = LocalD1::migrated();
        assert_eq!(db.int("SELECT COUNT(*) FROM apps WHERE purge_expired_images != 0", &[]), 0);
        assert_eq!(db.int("SELECT purge_expired_images FROM apps WHERE app_id = 'pixie'", &[]), 0);
    }

    #[test]
    fn no_tenant_is_purged_by_default() {
        let db = LocalD1::migrated();
        db.user("pixie", "u", 0);
        for app in ["pixie", "dreameater", "psywave"] {
            add_image(&db, &format!("old-{app}"), app, "2026-01-01T00:00:00+00:00");
        }
        assert_eq!(selected(&db), 0);
        let bucket = FakeBucket::new();
        let report = block_on(purge_expired(&db, &bucket, NOW, PURGE_BATCH)).unwrap();
        assert_eq!(report, PurgeReport::default());
        assert!(bucket.deleted.borrow().is_empty());
        assert_eq!(images(&db), 3);
    }

    #[test]
    fn a_tenant_that_opted_in_is_purged_and_one_that_did_not_is_not() {
        let db = database();
        db.run("INSERT INTO apps (app_id, name) VALUES ('other', 'Other')", &[]);
        add_image(&db, "opted-in", "scrubme", "2026-01-01T00:00:00+00:00");
        add_image(&db, "not-opted-in", "other", "2026-01-01T00:00:00+00:00");
        let rows = block_on(db.rows(SELECT_EXPIRED_SQL, &[Val::from(NOW), Val::Int(10)])).unwrap();
        assert_eq!(parse_expired(&rows).into_iter().map(|i| i.id).collect::<Vec<_>>(), vec!["opted-in"]);
        db.run("UPDATE apps SET purge_expired_images = 0 WHERE app_id = 'scrubme'", &[]);
        assert_eq!(selected(&db), 0);
    }

    #[test]
    fn a_tenant_without_an_apps_row_is_never_purged() {
        let db = LocalD1::migrated();
        db.user("ghost", "u", 0);
        add_image(&db, "orphan", "ghost", "2026-01-01T00:00:00+00:00");
        assert_eq!(selected(&db), 0);
    }

    #[test]
    fn pixie_is_never_purged_while_it_is_not_opted_in() {
        let db = database();
        add_image(&db, "pixie-old", "pixie", "2026-01-01T00:00:00+00:00");
        add_image(&db, "scrub-old", "scrubme", "2026-01-01T00:00:00+00:00");
        let bucket = FakeBucket::new();
        block_on(purge_expired(&db, &bucket, NOW, PURGE_BATCH)).unwrap();
        assert_eq!(db.int("SELECT COUNT(*) FROM stored_images WHERE id = 'pixie-old'", &[]), 1);
        assert_eq!(bucket.deleted.borrow().len(), 1);
    }

    #[test]
    fn a_run_removes_expired_images_and_their_reports_and_keeps_the_rest() {
        let db = database();
        add_image(&db, "old", "scrubme", "2026-10-01T00:00:00+00:00");
        add_image(&db, "fresh", "scrubme", "2026-10-30T00:00:00+00:00");
        add_image(&db, "pixie-old", "pixie", "2026-10-01T00:00:00+00:00");
        for image in ["old", "fresh"] {
            db.run(
                "INSERT INTO image_reports (id, app_id, image_id, reporter_user_id, created_at) VALUES (?1, 'scrubme', ?2, 'r', ?3)",
                &[format!("rep-{image}").into(), image.into(), NOW.into()],
            );
        }
        let bucket = FakeBucket::new();
        let report = block_on(purge_expired(&db, &bucket, NOW, PURGE_BATCH)).unwrap();
        assert_eq!(report, PurgeReport { selected: 1, rows_deleted: 1, failed: vec![] });
        assert_eq!(*bucket.deleted.borrow(), vec!["u/old.png"]);
        assert_eq!(db.int("SELECT COUNT(*) FROM stored_images WHERE id = 'old'", &[]), 0);
        assert_eq!(db.int("SELECT COUNT(*) FROM stored_images WHERE id IN ('fresh', 'pixie-old')", &[]), 2);
        assert_eq!(db.int("SELECT COUNT(*) FROM image_reports WHERE image_id = 'old'", &[]), 0);
        assert_eq!(db.int("SELECT COUNT(*) FROM image_reports WHERE image_id = 'fresh'", &[]), 1);
    }

    #[test]
    fn a_failed_object_delete_keeps_that_row_and_the_batch_continues() {
        let db = database();
        for id in ["a", "b", "c"] {
            add_image(&db, id, "scrubme", "2026-10-01T00:00:00+00:00");
        }
        let bucket = FakeBucket::failing_on(&["u/b.png"]);
        let report = block_on(purge_expired(&db, &bucket, NOW, PURGE_BATCH)).unwrap();
        assert_eq!((report.selected, report.rows_deleted), (3, 2));
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "u/b.png");
        assert_eq!(db.int("SELECT COUNT(*) FROM stored_images WHERE id = 'b'", &[]), 1);
        assert_eq!(images(&db), 1);

        let healthy = FakeBucket::new();
        let retry = block_on(purge_expired(&db, &healthy, NOW, PURGE_BATCH)).unwrap();
        assert_eq!(retry, PurgeReport { selected: 1, rows_deleted: 1, failed: vec![] });
        assert_eq!(images(&db), 0);
    }

    #[test]
    fn a_run_is_bounded_and_the_next_run_takes_the_rest() {
        let db = database();
        for n in 0..(PURGE_BATCH + 5) {
            add_image(&db, &format!("img-{n:03}"), "scrubme", &format!("2026-10-01T00:00:{:02}+00:00", n % 60));
        }
        let bucket = FakeBucket::new();
        let first = block_on(purge_expired(&db, &bucket, NOW, PURGE_BATCH)).unwrap();
        assert_eq!(first.rows_deleted as i64, PURGE_BATCH);
        assert_eq!(images(&db), 5);
        let second = block_on(purge_expired(&db, &bucket, NOW, PURGE_BATCH)).unwrap();
        assert_eq!(second.rows_deleted, 5);
        assert_eq!(images(&db), 0);
        assert_eq!(bucket.deleted.borrow().len() as i64, PURGE_BATCH + 5);
    }

    #[test]
    fn deletes_are_chunked_under_the_bound_parameter_limit() {
        let ids: Vec<String> = (0..PURGE_BATCH).map(|n| format!("id-{n}")).collect();
        let statements = delete_statements(&ids);
        assert_eq!(statements.len(), 2 * (PURGE_BATCH as usize).div_ceil(DELETE_CHUNK));
        assert!(statements.iter().all(|s| s.params.len() <= DELETE_CHUNK && s.params.len() < 100));
        assert!(statements[0].sql.starts_with("DELETE FROM image_reports"));
        assert!(statements[1].sql.starts_with("DELETE FROM stored_images"));
        assert!(delete_statements(&[]).is_empty());
    }

    #[test]
    fn rows_without_an_id_or_key_are_dropped() {
        let rows = vec![
            serde_json::json!({"id": "a", "r2_key": "u/a.png"}),
            serde_json::json!({"id": "b"}),
            serde_json::json!({"r2_key": "u/c.png"}),
        ];
        assert_eq!(parse_expired(&rows), vec![ExpiredImage { id: "a".into(), r2_key: "u/a.png".into() }]);
    }
}
