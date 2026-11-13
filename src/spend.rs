//! What a model provider has cost today, counted in millionths of a dollar from each call's token usage, and the
//! ceiling that stops calls for the rest of the UTC day once that much is spent. Anthropic is the provider counted:
//! its monthly credit carries features in every app (Midgar D39), and no per-user limit bounds the day's total.

use chrono::{DateTime, Utc};
use worker::{console_log, Env};

use crate::error::AppError;
use crate::wallet::d1::D1Exec;

pub const ANTHROPIC: &str = "anthropic";

const ANTHROPIC_CEILING_VAR: &str = "ANTHROPIC_DAILY_SPEND_LIMIT_USD";
const DEFAULT_ANTHROPIC_CEILING_USD: f64 = 2.00;
const MICRO_PER_USD: f64 = 1_000_000.0;

/// What a call cost in millionths of a dollar. Prices are dollars per million tokens, so tokens times price is
/// already in millionths; a fraction rounds up, so no call counts as free.
pub fn micro_usd(prices: (f64, f64), prompt_tokens: u64, output_tokens: u64) -> u64 {
    let (input, output) = prices;
    (prompt_tokens as f64 * input + output_tokens as f64 * output).ceil() as u64
}

/// The ceiling in millionths of a dollar: the configured dollars, or the default when the setting is missing, not a
/// number or not above zero. A typo must never read as "no ceiling" or as "nothing allowed".
pub fn ceiling_micro_usd(configured: Option<&str>) -> u64 {
    let dollars = configured
        .and_then(|text| text.trim().parse::<f64>().ok())
        .filter(|dollars| dollars.is_finite() && *dollars > 0.0)
        .unwrap_or(DEFAULT_ANTHROPIC_CEILING_USD);
    (dollars * MICRO_PER_USD).round() as u64
}

/// The UTC day a moment falls on, as `2026-11-13`: the key a day's total is kept under, so the count starts over at
/// midnight UTC without anything resetting it.
pub fn day_of(moment: DateTime<Utc>) -> String {
    moment.format("%Y-%m-%d").to_string()
}

/// What `provider` has cost on `day`, in millionths of a dollar.
pub async fn spent(db: &impl D1Exec, provider: &str, day: &str) -> Result<u64, AppError> {
    let rows = db
        .rows("SELECT micro_usd FROM provider_spend WHERE day = ?1 AND provider = ?2", &[day.into(), provider.into()])
        .await?;
    Ok(rows.first().and_then(|row| row.get("micro_usd")).and_then(|value| value.as_i64()).unwrap_or(0).max(0) as u64)
}

/// Adds a call's cost to the day's total for `provider`.
pub async fn add(db: &impl D1Exec, provider: &str, day: &str, micro_usd: u64) -> Result<(), AppError> {
    db.exec(
        "INSERT INTO provider_spend (day, provider, micro_usd, calls) VALUES (?1, ?2, ?3, 1)
         ON CONFLICT(day, provider) DO UPDATE SET micro_usd = micro_usd + excluded.micro_usd, calls = calls + 1",
        &[day.into(), provider.into(), (micro_usd as i64).into()],
    )
    .await?;
    Ok(())
}

/// Whether a call to `provider` may go out: false once the day's total has reached the ceiling.
pub async fn has_room(db: &impl D1Exec, provider: &str, day: &str, ceiling_micro_usd: u64) -> Result<bool, AppError> {
    Ok(spent(db, provider, day).await? < ceiling_micro_usd)
}

/// Refuses an Anthropic call with the rate-limit error once today's Anthropic spend has reached the ceiling. A count
/// that cannot be read lets the call through: a storage fault must not turn off a paid feature.
pub async fn refuse_anthropic_over_ceiling(env: &Env, db: &impl D1Exec) -> Result<(), AppError> {
    let ceiling = ceiling_micro_usd(env.var(ANTHROPIC_CEILING_VAR).ok().map(|value| value.to_string()).as_deref());
    match has_room(db, ANTHROPIC, &day_of(Utc::now()), ceiling).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            console_log!("Anthropic spend ceiling reached for today ({} micro-USD); call refused", ceiling);
            Err(AppError::RateLimitExceeded)
        }
        Err(error) => {
            console_log!("Anthropic spend could not be read, call allowed: {:?}", error);
            Ok(())
        }
    }
}

/// Counts an Anthropic call that was answered. A count that cannot be written is logged and the answer still goes
/// back to the person who paid for it.
pub async fn count_anthropic(db: &impl D1Exec, prices: (f64, f64), prompt_tokens: u64, output_tokens: u64) {
    let cost = micro_usd(prices, prompt_tokens, output_tokens);
    if let Err(error) = add(db, ANTHROPIC, &day_of(Utc::now()), cost).await {
        console_log!("Anthropic spend of {} micro-USD was not counted: {:?}", cost, error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::testing::LocalD1;
    use chrono::TimeZone;
    use futures::executor::block_on;

    const HAIKU: (f64, f64) = (0.10, 0.50);

    #[test]
    fn a_call_costs_its_tokens_at_the_models_prices() {
        assert_eq!(micro_usd(HAIKU, 1_000_000, 1_000_000), 600_000);
        assert_eq!(micro_usd(HAIKU, 30_000, 2_000), 4_000);
        assert_eq!(micro_usd((3.00, 15.00), 1_000, 1_000), 18_000);
    }

    #[test]
    fn a_fraction_of_a_millionth_rounds_up_and_nothing_costs_nothing() {
        assert_eq!(micro_usd(HAIKU, 1, 0), 1);
        assert_eq!(micro_usd(HAIKU, 11, 1), 2);
        assert_eq!(micro_usd(HAIKU, 0, 0), 0);
    }

    #[test]
    fn the_ceiling_is_the_setting_or_two_dollars() {
        assert_eq!(ceiling_micro_usd(Some("2.00")), 2_000_000);
        assert_eq!(ceiling_micro_usd(Some(" 0.50 ")), 500_000);
        assert_eq!(ceiling_micro_usd(None), 2_000_000);
        for wrong in ["", "two", "0", "-1", "NaN", "inf"] {
            assert_eq!(ceiling_micro_usd(Some(wrong)), 2_000_000, "{wrong}");
        }
    }

    #[test]
    fn the_day_is_the_utc_date() {
        assert_eq!(day_of(Utc.with_ymd_and_hms(2026, 11, 13, 23, 59, 59).unwrap()), "2026-11-13");
        assert_eq!(day_of(Utc.with_ymd_and_hms(2026, 11, 14, 0, 0, 0).unwrap()), "2026-11-14");
    }

    #[test]
    fn calls_add_up_per_provider_and_day() {
        let d1 = LocalD1::migrated();
        assert_eq!(block_on(spent(&d1, ANTHROPIC, "2026-11-13")).unwrap(), 0);
        block_on(add(&d1, ANTHROPIC, "2026-11-13", 4_000)).unwrap();
        block_on(add(&d1, ANTHROPIC, "2026-11-13", 6_500)).unwrap();
        block_on(add(&d1, "openai", "2026-11-13", 900_000)).unwrap();
        assert_eq!(block_on(spent(&d1, ANTHROPIC, "2026-11-13")).unwrap(), 10_500);
        assert_eq!(d1.int("SELECT calls FROM provider_spend WHERE day = '2026-11-13' AND provider = 'anthropic'", &[]), 2);
        assert_eq!(block_on(spent(&d1, ANTHROPIC, "2026-11-14")).unwrap(), 0);
    }

    #[test]
    fn calls_stop_at_the_ceiling_and_start_again_the_next_day() {
        let d1 = LocalD1::migrated();
        let room = |day: &str| block_on(has_room(&d1, ANTHROPIC, day, 2_000_000)).unwrap();
        assert!(room("2026-11-13"));
        block_on(add(&d1, ANTHROPIC, "2026-11-13", 1_999_999)).unwrap();
        assert!(room("2026-11-13"));
        block_on(add(&d1, ANTHROPIC, "2026-11-13", 1)).unwrap();
        assert!(!room("2026-11-13"));
        assert!(room("2026-11-14"));
    }

    #[test]
    fn another_providers_spend_never_stops_anthropic() {
        let d1 = LocalD1::migrated();
        block_on(add(&d1, "openai", "2026-11-13", 50_000_000)).unwrap();
        assert!(block_on(has_room(&d1, ANTHROPIC, "2026-11-13", 2_000_000)).unwrap());
    }
}
