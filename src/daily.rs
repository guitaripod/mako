//! One person's allowance of an alias per UTC day. An alias row may name how many answered calls a person gets in a
//! day (`chat_aliases.daily_limit`, 0 for no limit), whoever pays for them: a premium owner is charged nothing, so
//! without it one account could spend the day's provider ceiling for everyone.

use chrono::{DateTime, Duration, Utc};

use crate::error::AppError;
use crate::wallet::d1::D1Exec;

/// Whether another call fits: always under no limit, otherwise while fewer than `limit` were answered today.
pub fn has_room(used: u32, limit: u32) -> bool {
    limit == 0 || used < limit
}

/// The seconds from `now` to the next midnight UTC, when every allowance starts over. Never less than one.
pub fn seconds_until_next_day(now: DateTime<Utc>) -> i64 {
    let next = (now.date_naive() + Duration::days(1)).and_hms_opt(0, 0, 0).map(|midnight| midnight.and_utc());
    next.map(|midnight| (midnight - now).num_seconds()).unwrap_or(86_400).max(1)
}

/// How many calls of `alias` this person had answered on `day`.
pub async fn used(db: &impl D1Exec, app_id: &str, user_id: &str, alias: &str, day: &str) -> Result<u32, AppError> {
    let rows = db
        .rows(
            "SELECT used FROM daily_usage WHERE app_id = ?1 AND user_id = ?2 AND alias = ?3 AND day = ?4",
            &[app_id.into(), user_id.into(), alias.into(), day.into()],
        )
        .await?;
    Ok(rows.first().and_then(|row| row.get("used")).and_then(|value| value.as_i64()).unwrap_or(0).max(0) as u32)
}

/// Counts one answered call of `alias` for this person on `day`.
pub async fn count(db: &impl D1Exec, app_id: &str, user_id: &str, alias: &str, day: &str) -> Result<(), AppError> {
    db.exec(
        "INSERT INTO daily_usage (app_id, user_id, alias, day, used) VALUES (?1, ?2, ?3, ?4, 1)
         ON CONFLICT(app_id, user_id, alias, day) DO UPDATE SET used = used + 1",
        &[app_id.into(), user_id.into(), alias.into(), day.into()],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::testing::LocalD1;
    use chrono::TimeZone;
    use futures::executor::block_on;

    #[test]
    fn no_limit_always_has_room_and_a_limit_stops_at_its_number() {
        assert!(has_room(0, 0));
        assert!(has_room(10_000, 0));
        assert!(has_room(39, 40));
        assert!(!has_room(40, 40));
        assert!(!has_room(41, 40));
    }

    #[test]
    fn the_allowance_returns_at_midnight_utc() {
        assert_eq!(seconds_until_next_day(Utc.with_ymd_and_hms(2026, 11, 13, 23, 59, 30).unwrap()), 30);
        assert_eq!(seconds_until_next_day(Utc.with_ymd_and_hms(2026, 11, 13, 0, 0, 0).unwrap()), 86_400);
        assert_eq!(seconds_until_next_day(Utc.with_ymd_and_hms(2026, 12, 31, 12, 0, 0).unwrap()), 43_200);
    }

    #[test]
    fn calls_are_counted_per_person_alias_and_day() {
        let d1 = LocalD1::migrated();
        let seen = |user: &str, alias: &str, day: &str| block_on(used(&d1, "crucible", user, alias, day)).unwrap();
        assert_eq!(seen("u-1", "watch.pick", "2026-11-13"), 0);
        for _ in 0..3 {
            block_on(count(&d1, "crucible", "u-1", "watch.pick", "2026-11-13")).unwrap();
        }
        block_on(count(&d1, "crucible", "u-2", "watch.pick", "2026-11-13")).unwrap();
        block_on(count(&d1, "crucible", "u-1", "recap.describe", "2026-11-13")).unwrap();
        assert_eq!(seen("u-1", "watch.pick", "2026-11-13"), 3);
        assert_eq!(seen("u-2", "watch.pick", "2026-11-13"), 1);
        assert_eq!(seen("u-1", "recap.describe", "2026-11-13"), 1);
        assert_eq!(seen("u-1", "watch.pick", "2026-11-14"), 0);
    }

    #[test]
    fn the_crucible_tenant_is_seeded_with_pro_and_its_own_ceiling() {
        let d1 = LocalD1::migrated();
        assert_eq!(d1.int("SELECT COUNT(*) FROM apps WHERE app_id = 'crucible' AND premium_entitlement = 'pro' AND strict_premium = 1 AND store_prompts = 0", &[]), 1);
        assert_eq!(d1.int("SELECT anthropic_daily_micro_usd FROM apps WHERE app_id = 'crucible'", &[]), 750_000);
        assert_eq!(d1.int("SELECT new_user_free_credits FROM apps WHERE app_id = 'crucible'", &[]), 0);
    }
}
