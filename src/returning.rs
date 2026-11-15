//! Identities whose free start in a tenant is spent (migrations 046 and 047). A tenant's free start, the welcome bonus
//! and the edit runway, is meant once per identity: the device's anonymous Keychain id, its Midgar ID, or the Apple,
//! Google or GitHub account it signs in with. Deleting an account erases the row that proved the free start was had,
//! and linking an anonymous account to Apple moves that row off its device id, so either event remembers the identity
//! as a one-way hash that names nothing else. A new account from a remembered identity starts without the free start.
//! A hash is forgotten twelve months after the identity was last remembered, as the privacy policies say.

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use worker::D1Database;

use crate::error::AppError;
use crate::wallet::d1::{int, text, D1Exec};

/// A runway start long past, so the runway of a returning identity's new account has already ended.
pub const ENDED_RUNWAY_START: &str = "1970-01-01T00:00:00+00:00";

/// How long a remembered identity is kept after it was last remembered.
pub const KEPT_FOR_DAYS: i64 = 365;

/// The welcome credits a tenant without an `apps` row grants, as `credits::initialize_user_credits` does.
const DEFAULT_WELCOME_CREDITS: i64 = 6;

/// The one-way name of an identity in a tenant.
pub fn identity_hash(app_id: &str, provider: &str, provider_id: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [app_id, provider, provider_id] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    hex::encode(hasher.finalize())
}

/// Whether the tenant gives new accounts anything for free, the only case in which an identity is worth remembering.
pub async fn grants_free_start(db: &impl D1Exec, app_id: &str) -> Result<bool, AppError> {
    let rows = db
        .rows("SELECT new_user_free_credits, runway_days, runway_daily_edits FROM apps WHERE app_id = ?1", &[app_id.into()])
        .await?;
    let Some(row) = rows.first() else { return Ok(DEFAULT_WELCOME_CREDITS > 0) };
    let welcome = int(row, "new_user_free_credits").unwrap_or(DEFAULT_WELCOME_CREDITS);
    let runway = int(row, "runway_days").unwrap_or(0) > 0 && int(row, "runway_daily_edits").unwrap_or(0) > 0;
    Ok(welcome > 0 || runway)
}

/// Remembers the identity of `user_id` before its account is deleted and returns it as provider and provider id.
/// Nothing is remembered for a user that does not exist, has no provider id, or lives in a tenant without a free start.
pub async fn remember(db: &impl D1Exec, app_id: &str, user_id: &str, now: &str) -> Result<Option<(String, String)>, AppError> {
    let rows = db
        .rows("SELECT provider, provider_id FROM users WHERE id = ?1 AND app_id = ?2", &[user_id.into(), app_id.into()])
        .await?;
    let Some(row) = rows.first() else { return Ok(None) };
    let (Some(provider), Some(provider_id)) = (text(row, "provider"), text(row, "provider_id")) else { return Ok(None) };
    if provider.is_empty() || provider_id.is_empty() || !grants_free_start(db, app_id).await? {
        return Ok(None);
    }
    remember_identity(db, app_id, provider, provider_id, now).await?;
    Ok(Some((provider.to_string(), provider_id.to_string())))
}

/// Remembers one identity in a tenant whose free start it has spent. Repeating it counts the event and moves its time.
pub async fn remember_identity(db: &impl D1Exec, app_id: &str, provider: &str, provider_id: &str, now: &str) -> Result<(), AppError> {
    if provider_id.is_empty() {
        return Ok(());
    }
    db.exec(
        "INSERT INTO returning_devices (app_id, device_hash, first_deleted_at, last_deleted_at, deletions) VALUES (?1, ?2, ?3, ?3, 1)
         ON CONFLICT(app_id, device_hash) DO UPDATE SET last_deleted_at = ?3, deletions = deletions + 1",
        &[app_id.into(), identity_hash(app_id, provider, provider_id).into(), now.into()],
    )
    .await?;
    Ok(())
}

/// Whether the identity spent its free start in the tenant before.
pub async fn is_returning(db: &impl D1Exec, app_id: &str, provider: &str, provider_id: &str) -> Result<bool, AppError> {
    let hash = identity_hash(app_id, provider, provider_id);
    let rows = db
        .rows("SELECT 1 AS found FROM returning_devices WHERE app_id = ?1 AND device_hash = ?2", &[app_id.into(), hash.into()])
        .await?;
    Ok(!rows.is_empty())
}

/// Ends the runway of a returning identity's account before its first edit can start it. Safe to repeat: a runway
/// already started is left as it is.
pub async fn end_runway(db: &impl D1Exec, app_id: &str, user_id: &str) -> Result<(), AppError> {
    db.exec(
        "INSERT OR IGNORE INTO runways (app_id, user_id, started_at) VALUES (?1, ?2, ?3)",
        &[app_id.into(), user_id.into(), ENDED_RUNWAY_START.into()],
    )
    .await?;
    Ok(())
}

/// Ends the runway of an account that registration found already made, when its identity is a returning one. A
/// registration that failed after inserting the user is retried into this path, which would otherwise leave the
/// account an open runway.
pub async fn end_runway_if_returning(db: &impl D1Exec, app_id: &str, provider: &str, provider_id: &str, user_id: &str) -> Result<(), AppError> {
    if is_returning(db, app_id, provider, provider_id).await? {
        end_runway(db, app_id, user_id).await?;
    }
    Ok(())
}

/// Forgets every identity last remembered before `now` minus `KEPT_FOR_DAYS`, and returns how many.
pub async fn forget_expired(db: &impl D1Exec, now: DateTime<Utc>) -> Result<u64, AppError> {
    let cutoff = (now - Duration::days(KEPT_FOR_DAYS)).to_rfc3339();
    db.exec("DELETE FROM returning_devices WHERE last_deleted_at < ?1", &[cutoff.into()]).await
}

/// Opens a new account's balance: the welcome bonus and an open runway for an identity that never spent its free start
/// in the tenant, a zero balance and an ended runway for one that did.
pub async fn open_new_account(db: &D1Database, app_id: &str, user_id: &str) -> worker::Result<()> {
    open_new_account_on_device(db, app_id, user_id, false).await.map(|_| ())
}

/// `open_new_account` for a device whose DeviceCheck bit may already say it had the free start
/// (`crate::device_bits`). Returns whether the account got the free start.
pub async fn open_new_account_on_device(db: &D1Database, app_id: &str, user_id: &str, spent_on_device: bool) -> worker::Result<bool> {
    let rows = db
        .rows("SELECT provider, provider_id FROM users WHERE id = ?1 AND app_id = ?2", &[user_id.into(), app_id.into()])
        .await?;
    let identity = rows.first().and_then(|row| Some((text(row, "provider")?.to_string(), text(row, "provider_id")?.to_string())));
    let returning = match &identity {
        Some((provider, provider_id)) => is_returning(db, app_id, provider, provider_id).await?,
        None => false,
    };
    if !returning && !spent_on_device {
        crate::credits::initialize_user_credits(app_id, user_id, db).await?;
        return Ok(true);
    }
    crate::credits::open_user_credits(app_id, user_id, db).await?;
    end_runway(db, app_id, user_id).await?;
    let why = if returning { "a returning identity" } else { "a device whose DeviceCheck bit is set" };
    worker::console_log!("New user {} in {} is {}: no free start", user_id, app_id, why);
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::{runway_phase, RunwayPhase};
    use crate::wallet::testing::LocalD1;
    use chrono::TimeZone;
    use futures::executor::block_on;

    const NOW: &str = "2026-11-15T06:01:00+00:00";

    fn user(d1: &LocalD1, app_id: &str, user_id: &str, provider: &str, provider_id: &str) {
        d1.run(
            "INSERT INTO users (id, provider, provider_id, name, created_at, updated_at, app_id) VALUES (?1, ?2, ?3, 'T', ?4, ?4, ?5)",
            &[user_id.into(), provider.into(), provider_id.into(), NOW.into(), app_id.into()],
        );
    }

    fn tenant(d1: &LocalD1, app_id: &str, welcome: i64, runway_days: i64) {
        d1.run("DELETE FROM apps WHERE app_id = ?1", &[app_id.into()]);
        d1.run(
            "INSERT INTO apps (app_id, name, new_user_free_credits, runway_days, runway_daily_edits) VALUES (?1, ?1, ?2, ?3, 5)",
            &[app_id.into(), welcome.into(), runway_days.into()],
        );
    }

    fn rows(d1: &LocalD1) -> i64 {
        d1.int("SELECT COUNT(*) FROM returning_devices", &[])
    }

    #[test]
    fn a_deleted_identity_returns_and_only_in_its_tenant_and_provider() {
        let d1 = LocalD1::migrated();
        tenant(&d1, "pixie", 65, 7);
        user(&d1, "pixie", "u1", "anonymous", "keychain-1");
        assert!(!block_on(is_returning(&d1, "pixie", "anonymous", "keychain-1")).unwrap());
        let remembered = block_on(remember(&d1, "pixie", "u1", NOW)).unwrap();
        assert_eq!(remembered, Some(("anonymous".to_string(), "keychain-1".to_string())));
        assert!(block_on(is_returning(&d1, "pixie", "anonymous", "keychain-1")).unwrap());
        assert!(!block_on(is_returning(&d1, "doublekick", "anonymous", "keychain-1")).unwrap());
        assert!(!block_on(is_returning(&d1, "pixie", "anonymous", "keychain-2")).unwrap());
        assert!(!block_on(is_returning(&d1, "pixie", "midgar", "keychain-1")).unwrap());
    }

    #[test]
    fn every_sign_in_kind_is_remembered() {
        let d1 = LocalD1::migrated();
        tenant(&d1, "pixie", 65, 7);
        for (n, provider) in ["anonymous", "midgar", "apple", "google", "github"].iter().enumerate() {
            let id = format!("u{n}");
            user(&d1, "pixie", &id, provider, &format!("{provider}-id"));
            assert!(block_on(remember(&d1, "pixie", &id, NOW)).unwrap().is_some(), "{provider}");
            assert!(block_on(is_returning(&d1, "pixie", provider, &format!("{provider}-id"))).unwrap(), "{provider}");
        }
        assert_eq!(rows(&d1), 5);
    }

    #[test]
    fn a_tenant_without_a_free_start_remembers_nothing() {
        let d1 = LocalD1::migrated();
        tenant(&d1, "crucible", 0, 0);
        user(&d1, "crucible", "u1", "midgar", "mid-1");
        assert_eq!(block_on(remember(&d1, "crucible", "u1", NOW)).unwrap(), None);
        tenant(&d1, "runwayonly", 0, 7);
        user(&d1, "runwayonly", "u2", "anonymous", "k");
        assert!(block_on(remember(&d1, "runwayonly", "u2", NOW)).unwrap().is_some());
        user(&d1, "noappsrow", "u3", "anonymous", "k");
        assert!(block_on(remember(&d1, "noappsrow", "u3", NOW)).unwrap().is_some());
        assert_eq!(rows(&d1), 2);
    }

    #[test]
    fn a_missing_user_or_an_empty_id_is_not_remembered() {
        let d1 = LocalD1::migrated();
        tenant(&d1, "pixie", 65, 7);
        user(&d1, "pixie", "u1", "anonymous", "");
        assert_eq!(block_on(remember(&d1, "pixie", "u1", NOW)).unwrap(), None);
        assert_eq!(block_on(remember(&d1, "pixie", "nobody", NOW)).unwrap(), None);
        block_on(remember_identity(&d1, "pixie", "midgar", "", NOW)).unwrap();
        assert_eq!(rows(&d1), 0);
    }

    #[test]
    fn repeats_count_up_and_keep_the_first_time() {
        let d1 = LocalD1::migrated();
        tenant(&d1, "pixie", 65, 7);
        user(&d1, "pixie", "u1", "anonymous", "keychain-1");
        block_on(remember(&d1, "pixie", "u1", "2026-11-15T05:45:00+00:00")).unwrap();
        block_on(remember(&d1, "pixie", "u1", "2026-11-15T06:01:00+00:00")).unwrap();
        assert_eq!(d1.int("SELECT deletions FROM returning_devices", &[]), 2);
        let times: (String, String) =
            d1.0.query_row("SELECT first_deleted_at, last_deleted_at FROM returning_devices", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(times, ("2026-11-15T05:45:00+00:00".to_string(), "2026-11-15T06:01:00+00:00".to_string()));
    }

    #[test]
    fn the_hash_names_no_id_and_separates_its_parts() {
        let hash = identity_hash("pixie", "anonymous", "keychain-1");
        assert_eq!(hash.len(), 64);
        assert!(!hash.contains("keychain"));
        assert_ne!(identity_hash("pixie", "anonymous", "x"), identity_hash("pixiea", "nonymous", "x"));
    }

    #[test]
    fn an_ended_runway_is_over_and_a_started_one_is_kept() {
        let d1 = LocalD1::migrated();
        user(&d1, "pixie", "u1", "anonymous", "keychain-1");
        block_on(end_runway(&d1, "pixie", "u1")).unwrap();
        let start: String = d1.0.query_row("SELECT started_at FROM runways WHERE user_id = 'u1'", [], |r| r.get(0)).unwrap();
        let start = DateTime::parse_from_rfc3339(&start).unwrap().with_timezone(&Utc);
        let now = DateTime::parse_from_rfc3339(NOW).unwrap().with_timezone(&Utc);
        assert!(matches!(runway_phase(Some(start), now, 7), RunwayPhase::Ended));

        user(&d1, "pixie", "u2", "anonymous", "keychain-2");
        d1.run("INSERT INTO runways (app_id, user_id, started_at) VALUES ('pixie', 'u2', ?1)", &[NOW.into()]);
        block_on(end_runway(&d1, "pixie", "u2")).unwrap();
        assert_eq!(d1.int("SELECT COUNT(*) FROM runways WHERE user_id = 'u2' AND started_at = ?1", &[NOW.into()]), 1);
    }

    #[test]
    fn a_retried_registration_of_a_returning_identity_still_ends_the_runway() {
        let d1 = LocalD1::migrated();
        user(&d1, "pixie", "u1", "anonymous", "keychain-1");
        block_on(end_runway_if_returning(&d1, "pixie", "anonymous", "keychain-1", "u1")).unwrap();
        assert_eq!(d1.int("SELECT COUNT(*) FROM runways WHERE user_id = 'u1'", &[]), 0);
        block_on(remember_identity(&d1, "pixie", "anonymous", "keychain-1", NOW)).unwrap();
        block_on(end_runway_if_returning(&d1, "pixie", "anonymous", "keychain-1", "u1")).unwrap();
        assert_eq!(d1.int("SELECT COUNT(*) FROM runways WHERE user_id = 'u1' AND started_at = ?1", &[ENDED_RUNWAY_START.into()]), 1);
    }

    #[test]
    fn an_identity_is_forgotten_a_year_after_it_was_last_remembered() {
        let d1 = LocalD1::migrated();
        block_on(remember_identity(&d1, "pixie", "anonymous", "old", "2025-11-01T00:00:00+00:00")).unwrap();
        block_on(remember_identity(&d1, "pixie", "anonymous", "renewed", "2025-11-01T00:00:00+00:00")).unwrap();
        block_on(remember_identity(&d1, "pixie", "anonymous", "renewed", "2026-11-01T00:00:00+00:00")).unwrap();
        block_on(remember_identity(&d1, "pixie", "anonymous", "recent", NOW)).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 11, 15, 0, 0, 0).unwrap();
        assert_eq!(block_on(forget_expired(&d1, now)).unwrap(), 1);
        assert!(!block_on(is_returning(&d1, "pixie", "anonymous", "old")).unwrap());
        assert!(block_on(is_returning(&d1, "pixie", "anonymous", "renewed")).unwrap());
        assert!(block_on(is_returning(&d1, "pixie", "anonymous", "recent")).unwrap());
    }
}
