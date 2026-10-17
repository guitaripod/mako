use serde_json::Value;
use uuid::Uuid;

use super::d1::{int, text, D1Exec, Stmt};
use crate::error::AppError;

/// Where a link is in the saga that moves a user's D1 balance into the wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Moving,
    Linked,
}

/// One `wallet_links` row: the saga log of a user and its Midgar ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRow {
    pub app_id: String,
    pub user_id: String,
    pub midgar_id: String,
    pub state: LinkState,
    pub moved_credits: Option<i64>,
    pub welcome_part: Option<i64>,
    pub residue_moves: i64,
    pub residue_pending: Option<i64>,
}

/// A linked user whose frozen D1 balance holds credits again, or whose residue move was interrupted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Residue {
    pub link: LinkRow,
    pub balance: i64,
    pub stamp: String,
}

pub const COLUMNS: &str = "app_id, user_id, midgar_id, state, moved_credits, welcome_part, residue_moves, residue_pending";

impl LinkRow {
    /// Parses a row selected with `COLUMNS`.
    fn parse(row: &Value) -> Result<Self, AppError> {
        let corrupt = || AppError::InternalError("wallet_links row is malformed".to_string());
        let state = match text(row, "state") {
            Some("moving") => LinkState::Moving,
            Some("linked") => LinkState::Linked,
            _ => return Err(corrupt()),
        };
        Ok(Self {
            app_id: text(row, "app_id").ok_or_else(corrupt)?.to_string(),
            user_id: text(row, "user_id").ok_or_else(corrupt)?.to_string(),
            midgar_id: text(row, "midgar_id").ok_or_else(corrupt)?.to_string(),
            state,
            moved_credits: int(row, "moved_credits"),
            welcome_part: int(row, "welcome_part"),
            residue_moves: int(row, "residue_moves").unwrap_or(0),
            residue_pending: int(row, "residue_pending"),
        })
    }
}

/// The idempotency key of the first migration of a user's D1 balance.
pub fn migrate_key(app_id: &str, user_id: &str) -> String {
    format!("migrate:{app_id}:{user_id}")
}

/// The idempotency key of the `n`-th sweep of credits that landed in the frozen D1 row.
pub fn residue_key(app_id: &str, user_id: &str, n: i64) -> String {
    format!("migrate:{app_id}:{user_id}:{n}")
}

/// A user's link, in any state.
pub async fn read(db: &impl D1Exec, app_id: &str, user_id: &str) -> Result<Option<LinkRow>, AppError> {
    let sql = format!("SELECT {COLUMNS} FROM wallet_links WHERE app_id = ?1 AND user_id = ?2");
    db.rows(&sql, &[app_id.into(), user_id.into()]).await?.first().map(LinkRow::parse).transpose()
}

/// The oldest link of a Midgar ID in one tenant: the user a Midgar-ID-native request acts as.
pub async fn oldest_for_midgar(db: &impl D1Exec, app_id: &str, midgar_id: &str) -> Result<Option<LinkRow>, AppError> {
    let sql = format!("SELECT {COLUMNS} FROM wallet_links WHERE app_id = ?1 AND midgar_id = ?2 ORDER BY linked_at, user_id LIMIT 1");
    db.rows(&sql, &[app_id.into(), midgar_id.into()]).await?.first().map(LinkRow::parse).transpose()
}

/// Step 1 of the saga: records the intent to link. A user that already has a row keeps it.
pub async fn insert_moving(db: &impl D1Exec, app_id: &str, user_id: &str, midgar_id: &str, now: &str) -> Result<(), AppError> {
    let sql = "INSERT OR IGNORE INTO wallet_links (app_id, user_id, midgar_id, state, linked_at) VALUES (?1, ?2, ?3, 'moving', ?4)";
    db.exec(sql, &[app_id.into(), user_id.into(), midgar_id.into(), now.into()]).await?;
    Ok(())
}

/// A link that has nothing to move, born in the `linked` state: the Midgar-ID-native user, who has no D1 balance.
pub async fn insert_linked(db: &impl D1Exec, app_id: &str, user_id: &str, midgar_id: &str, now: &str) -> Result<(), AppError> {
    let sql = "INSERT OR IGNORE INTO wallet_links (app_id, user_id, midgar_id, state, moved_credits, welcome_part, linked_at)
        VALUES (?1, ?2, ?3, 'linked', 0, 0, ?4)";
    db.exec(sql, &[app_id.into(), user_id.into(), midgar_id.into(), now.into()]).await?;
    Ok(())
}

/// The `updated_at` of a user's D1 balance row, empty when the user has none. Every write to the row changes it, so
/// a freeze that names the value it read cannot land on a balance that moved in between.
pub async fn stamp_of(db: &impl D1Exec, app_id: &str, user_id: &str) -> Result<String, AppError> {
    let rows = db.rows("SELECT updated_at FROM user_credits WHERE app_id = ?1 AND user_id = ?2", &[app_id.into(), user_id.into()]).await?;
    Ok(rows.first().and_then(|r| text(r, "updated_at")).unwrap_or_default().to_string())
}

/// Step 2 of the saga: records the balance and the welcome bonus it contains in the link row and zeroes the balance,
/// in one transaction. Both statements only apply while the balance row is still the one that was read, so a
/// concurrent run, or a credit that landed in between, makes this a no-op and the caller reads the row again.
pub async fn freeze(db: &impl D1Exec, app_id: &str, user_id: &str, now: &str) -> Result<(), AppError> {
    let stamp = stamp_of(db, app_id, user_id).await?;
    freeze_at(db, app_id, user_id, now, &stamp).await
}

/// `freeze` for a balance row last seen with the given `updated_at`.
pub async fn freeze_at(db: &impl D1Exec, app_id: &str, user_id: &str, now: &str, stamp: &str) -> Result<(), AppError> {
    let stamp = stamp.to_string();
    let record = Stmt::new(
        "UPDATE wallet_links SET
            moved_credits = COALESCE((SELECT balance FROM user_credits WHERE app_id = ?1 AND user_id = ?2), 0),
            welcome_part = (SELECT COALESCE(SUM(amount), 0) FROM credit_transactions
                            WHERE app_id = ?1 AND user_id = ?2 AND type = 'bonus' AND description = 'Welcome bonus')
         WHERE app_id = ?1 AND user_id = ?2 AND moved_credits IS NULL
           AND COALESCE((SELECT updated_at FROM user_credits WHERE app_id = ?1 AND user_id = ?2), '') = ?3",
        vec![app_id.into(), user_id.into(), stamp.as_str().into()],
    );
    let zero = Stmt::new(
        "UPDATE user_credits SET balance = 0, updated_at = ?3
         WHERE app_id = ?1 AND user_id = ?2 AND updated_at = ?4
           AND balance = (SELECT moved_credits FROM wallet_links WHERE app_id = ?1 AND user_id = ?2)",
        vec![app_id.into(), user_id.into(), now.into(), stamp.as_str().into()],
    );
    db.batch(&[record, zero]).await
}

/// Step 4 of the saga.
pub async fn mark_linked(db: &impl D1Exec, app_id: &str, user_id: &str) -> Result<(), AppError> {
    db.exec("UPDATE wallet_links SET state = 'linked' WHERE app_id = ?1 AND user_id = ?2 AND state = 'moving'", &[app_id.into(), user_id.into()])
        .await?;
    Ok(())
}

/// Rows still `moving` that began before `cutoff`, oldest first.
pub async fn stale_moving(db: &impl D1Exec, cutoff: &str, limit: i64) -> Result<Vec<LinkRow>, AppError> {
    let sql = format!("SELECT {COLUMNS} FROM wallet_links WHERE state = 'moving' AND linked_at < ?1 ORDER BY linked_at LIMIT ?2");
    db.rows(&sql, &[cutoff.into(), limit.into()]).await?.iter().map(LinkRow::parse).collect()
}

/// Linked users whose frozen D1 row holds credits, or whose residue move was interrupted, oldest link first.
pub async fn residue_candidates(db: &impl D1Exec, limit: i64) -> Result<Vec<Residue>, AppError> {
    let sql = "SELECT l.app_id AS app_id, l.user_id AS user_id, l.midgar_id AS midgar_id, l.state AS state,
            l.moved_credits AS moved_credits, l.welcome_part AS welcome_part, l.residue_moves AS residue_moves,
            l.residue_pending AS residue_pending, COALESCE(c.balance, 0) AS balance, COALESCE(c.updated_at, '') AS stamp
         FROM wallet_links l LEFT JOIN user_credits c ON c.app_id = l.app_id AND c.user_id = l.user_id
         WHERE l.state = 'linked' AND (COALESCE(c.balance, 0) > 0 OR l.residue_pending IS NOT NULL)
         ORDER BY l.linked_at, l.user_id LIMIT ?1";
    db.rows(sql, &[limit.into()])
        .await?
        .iter()
        .map(|row| {
            Ok(Residue { link: LinkRow::parse(row)?, balance: int(row, "balance").unwrap_or(0), stamp: text(row, "stamp").unwrap_or_default().to_string() })
        })
        .collect()
}

/// Freezes the credits that landed in a linked user's D1 row: the amount moves into `residue_pending` and the balance
/// goes to zero, in one transaction that only applies while the balance row is the one that was read.
pub async fn freeze_residue(db: &impl D1Exec, app_id: &str, user_id: &str, stamp: &str, now: &str) -> Result<(), AppError> {
    let record = Stmt::new(
        "UPDATE wallet_links SET residue_pending = (SELECT balance FROM user_credits WHERE app_id = ?1 AND user_id = ?2)
         WHERE app_id = ?1 AND user_id = ?2 AND state = 'linked' AND residue_pending IS NULL
           AND (SELECT updated_at FROM user_credits WHERE app_id = ?1 AND user_id = ?2) = ?3
           AND (SELECT balance FROM user_credits WHERE app_id = ?1 AND user_id = ?2) > 0",
        vec![app_id.into(), user_id.into(), stamp.into()],
    );
    let zero = Stmt::new(
        "UPDATE user_credits SET balance = 0, updated_at = ?3
         WHERE app_id = ?1 AND user_id = ?2 AND updated_at = ?4
           AND balance = (SELECT residue_pending FROM wallet_links WHERE app_id = ?1 AND user_id = ?2)",
        vec![app_id.into(), user_id.into(), now.into(), stamp.into()],
    );
    db.batch(&[record, zero]).await
}

/// Closes a residue move once the wallet answered: counts it and clears the pending amount.
pub async fn finish_residue(db: &impl D1Exec, app_id: &str, user_id: &str, moves: i64) -> Result<(), AppError> {
    db.exec(
        "UPDATE wallet_links SET residue_moves = residue_moves + 1, residue_pending = NULL
         WHERE app_id = ?1 AND user_id = ?2 AND residue_pending IS NOT NULL AND residue_moves = ?3",
        &[app_id.into(), user_id.into(), moves.into()],
    )
    .await?;
    Ok(())
}

/// The welcome credits a tenant grants a new user, as `initialize_user_credits` reads them.
pub async fn welcome_credits(db: &impl D1Exec, app_id: &str) -> Result<i64, AppError> {
    let rows = db.rows("SELECT new_user_free_credits FROM apps WHERE app_id = ?1", &[app_id.into()]).await?;
    Ok(rows.first().and_then(|r| int(r, "new_user_free_credits")).unwrap_or(6))
}

/// The user of a Midgar ID in one tenant, created when missing: `provider = 'midgar'`, no API key. Safe to repeat and
/// to race, because the unique `(app_id, provider, provider_id)` index makes the loser read the winner's row.
pub async fn midgar_user(db: &impl D1Exec, app_id: &str, midgar_id: &str, now: &str) -> Result<String, AppError> {
    let candidate = Uuid::new_v4().to_string();
    db.exec(
        "INSERT INTO users (id, app_id, email, provider, provider_id, name, created_at, updated_at)
         VALUES (?1, ?2, NULL, 'midgar', ?3, 'Midgar ID', ?4, ?4)
         ON CONFLICT(app_id, provider, provider_id) DO NOTHING",
        &[candidate.as_str().into(), app_id.into(), midgar_id.into(), now.into()],
    )
    .await?;
    let rows = db
        .rows("SELECT id FROM users WHERE app_id = ?1 AND provider = 'midgar' AND provider_id = ?2", &[app_id.into(), midgar_id.into()])
        .await?;
    rows.first()
        .and_then(|r| text(r, "id"))
        .map(str::to_string)
        .ok_or_else(|| AppError::InternalError("midgar user lookup failed".to_string()))
}

/// Whether a tenant is configured. A Midgar-ID-native request names its tenant only in a header, so nothing else
/// stops it from creating users and welcome bonuses under a tenant that is not one.
pub async fn tenant_exists(db: &impl D1Exec, app_id: &str) -> Result<bool, AppError> {
    Ok(!db.rows("SELECT 1 AS found FROM apps WHERE app_id = ?1", &[app_id.into()]).await?.is_empty())
}

/// Whether the user exists in the tenant.
pub async fn user_exists(db: &impl D1Exec, app_id: &str, user_id: &str) -> Result<bool, AppError> {
    Ok(!db.rows("SELECT 1 AS found FROM users WHERE app_id = ?1 AND id = ?2", &[app_id.into(), user_id.into()]).await?.is_empty())
}
