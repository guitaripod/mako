//! Devices that deleted an account in a tenant (migration 046). A tenant's free start, the welcome bonus and the edit
//! runway, is meant once per device, and the account row that proved a device had it is erased with the account. So
//! the device is remembered as a one-way hash that names nothing else, and a new account from it starts without the
//! free start.

use sha2::{Digest, Sha256};

use crate::error::AppError;
use crate::wallet::d1::{text, D1Exec};

/// Providers whose `provider_id` is a device: the anonymous Keychain id or a device-bound Midgar ID. Accounts signed in
/// another way belong to a person's sign-in, not a device, and are not remembered.
const DEVICE_PROVIDERS: &[&str] = &["anonymous", "midgar"];

/// A runway start long past, so the runway of a returning device's new account has already ended.
pub const ENDED_RUNWAY_START: &str = "1970-01-01T00:00:00+00:00";

/// The one-way name of a device in a tenant.
pub fn device_hash(app_id: &str, provider: &str, provider_id: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [app_id, provider, provider_id] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    hex::encode(hasher.finalize())
}

/// Remembers the device behind `user_id` before its account is deleted. Nothing is remembered for an account that is
/// not a device's or that does not exist.
pub async fn remember(db: &impl D1Exec, app_id: &str, user_id: &str, now: &str) -> Result<bool, AppError> {
    let rows = db
        .rows("SELECT provider, provider_id FROM users WHERE id = ?1 AND app_id = ?2", &[user_id.into(), app_id.into()])
        .await?;
    let Some(row) = rows.first() else { return Ok(false) };
    let (Some(provider), Some(provider_id)) = (text(row, "provider"), text(row, "provider_id")) else { return Ok(false) };
    if !DEVICE_PROVIDERS.contains(&provider) || provider_id.is_empty() {
        return Ok(false);
    }
    let hash = device_hash(app_id, provider, provider_id);
    db.exec(
        "INSERT INTO returning_devices (app_id, device_hash, first_deleted_at, last_deleted_at, deletions) VALUES (?1, ?2, ?3, ?3, 1)
         ON CONFLICT(app_id, device_hash) DO UPDATE SET last_deleted_at = ?3, deletions = deletions + 1",
        &[app_id.into(), hash.into(), now.into()],
    )
    .await?;
    Ok(true)
}

/// Whether the device once deleted an account in the tenant.
pub async fn is_returning(db: &impl D1Exec, app_id: &str, provider: &str, provider_id: &str) -> Result<bool, AppError> {
    let hash = device_hash(app_id, provider, provider_id);
    let rows = db
        .rows("SELECT 1 AS found FROM returning_devices WHERE app_id = ?1 AND device_hash = ?2", &[app_id.into(), hash.into()])
        .await?;
    Ok(!rows.is_empty())
}

/// Ends the runway of a returning device's new account before its first edit can start it.
pub async fn end_runway(db: &impl D1Exec, app_id: &str, user_id: &str) -> Result<(), AppError> {
    db.exec(
        "INSERT OR IGNORE INTO runways (app_id, user_id, started_at) VALUES (?1, ?2, ?3)",
        &[app_id.into(), user_id.into(), ENDED_RUNWAY_START.into()],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::{runway_phase, RunwayPhase};
    use crate::wallet::testing::LocalD1;
    use chrono::{DateTime, Utc};
    use futures::executor::block_on;

    const NOW: &str = "2026-11-15T06:01:00+00:00";

    fn device_user(d1: &LocalD1, app_id: &str, user_id: &str, provider: &str, provider_id: &str) {
        d1.run(
            "INSERT INTO users (id, provider, provider_id, name, created_at, updated_at, app_id) VALUES (?1, ?2, ?3, 'T', ?4, ?4, ?5)",
            &[user_id.into(), provider.into(), provider_id.into(), NOW.into(), app_id.into()],
        );
    }

    #[test]
    fn a_deleted_anonymous_device_returns_and_only_in_its_tenant() {
        let d1 = LocalD1::migrated();
        device_user(&d1, "pixie", "u1", "anonymous", "keychain-1");
        assert!(!block_on(is_returning(&d1, "pixie", "anonymous", "keychain-1")).unwrap());
        assert!(block_on(remember(&d1, "pixie", "u1", NOW)).unwrap());
        assert!(block_on(is_returning(&d1, "pixie", "anonymous", "keychain-1")).unwrap());
        assert!(!block_on(is_returning(&d1, "doublekick", "anonymous", "keychain-1")).unwrap());
        assert!(!block_on(is_returning(&d1, "pixie", "anonymous", "keychain-2")).unwrap());
        assert!(!block_on(is_returning(&d1, "pixie", "midgar", "keychain-1")).unwrap());
    }

    #[test]
    fn a_midgar_id_returns_too() {
        let d1 = LocalD1::migrated();
        device_user(&d1, "pixie", "u1", "midgar", "mid-1");
        assert!(block_on(remember(&d1, "pixie", "u1", NOW)).unwrap());
        assert!(block_on(is_returning(&d1, "pixie", "midgar", "mid-1")).unwrap());
    }

    #[test]
    fn a_signed_in_person_or_a_missing_user_is_not_remembered() {
        let d1 = LocalD1::migrated();
        device_user(&d1, "pixie", "u1", "apple", "apple-sub");
        assert!(!block_on(remember(&d1, "pixie", "u1", NOW)).unwrap());
        assert!(!block_on(remember(&d1, "pixie", "nobody", NOW)).unwrap());
        assert!(!block_on(remember(&d1, "doublekick", "u1", NOW)).unwrap());
        assert_eq!(d1.int("SELECT COUNT(*) FROM returning_devices", &[]), 0);
    }

    #[test]
    fn deletions_count_up_and_keep_the_first_time() {
        let d1 = LocalD1::migrated();
        device_user(&d1, "pixie", "u1", "anonymous", "keychain-1");
        block_on(remember(&d1, "pixie", "u1", "2026-11-15T05:45:00+00:00")).unwrap();
        block_on(remember(&d1, "pixie", "u1", "2026-11-15T06:01:00+00:00")).unwrap();
        assert_eq!(d1.int("SELECT deletions FROM returning_devices", &[]), 2);
        let first: String = d1.0.query_row("SELECT first_deleted_at FROM returning_devices", [], |r| r.get(0)).unwrap();
        let last: String = d1.0.query_row("SELECT last_deleted_at FROM returning_devices", [], |r| r.get(0)).unwrap();
        assert_eq!((first.as_str(), last.as_str()), ("2026-11-15T05:45:00+00:00", "2026-11-15T06:01:00+00:00"));
    }

    #[test]
    fn the_hash_names_no_id_and_separates_its_parts() {
        let hash = device_hash("pixie", "anonymous", "keychain-1");
        assert_eq!(hash.len(), 64);
        assert!(!hash.contains("keychain"));
        assert_ne!(device_hash("pixie", "anonymous", "x"), device_hash("pixiea", "nonymous", "x"));
    }

    #[test]
    fn an_ended_runway_is_over_and_a_started_one_is_kept() {
        let d1 = LocalD1::migrated();
        device_user(&d1, "pixie", "u1", "anonymous", "keychain-1");
        block_on(end_runway(&d1, "pixie", "u1")).unwrap();
        let start: String = d1.0.query_row("SELECT started_at FROM runways WHERE user_id = 'u1'", [], |r| r.get(0)).unwrap();
        let start = DateTime::parse_from_rfc3339(&start).unwrap().with_timezone(&Utc);
        let now = DateTime::parse_from_rfc3339(NOW).unwrap().with_timezone(&Utc);
        assert!(matches!(runway_phase(Some(start), now, 7), RunwayPhase::Ended));

        device_user(&d1, "pixie", "u2", "anonymous", "keychain-2");
        d1.run("INSERT INTO runways (app_id, user_id, started_at) VALUES ('pixie', 'u2', ?1)", &[NOW.into()]);
        block_on(end_runway(&d1, "pixie", "u2")).unwrap();
        assert_eq!(d1.int("SELECT COUNT(*) FROM runways WHERE user_id = 'u2' AND started_at = ?1", &[NOW.into()]), 1);
    }
}
