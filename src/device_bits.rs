//! Apple DeviceCheck's two bits per device. Apple keeps them for the developer team, so they outlast app deletion,
//! reinstall, a new Keychain id and a new account, and every app of the team shares the same two. Apple names this use:
//! "identify devices that have already taken advantage of a promotional offer". A tenant may claim one of them in `apps.devicecheck_free_start_bit` (migration 048) to mark that the device
//! has had its free start: a new account on a device whose bit is set opens without it, whatever account or device id
//! it presents. A bit set more than `FRESH_FOR_MONTHS` ago is ignored, so a device sold on gets a free start again.

use chrono::{DateTime, Datelike, Utc};
use serde_json::Value;

/// How long a set bit counts, in whole calendar months after the month Apple last updated it.
pub const FRESH_FOR_MONTHS: i32 = 12;

/// A device's two bits as Apple answered them. `last_update` is Apple's `YYYY-MM`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BitState {
    pub bit0: bool,
    pub bit1: bool,
    pub last_update: Option<String>,
}

/// Reads the body of a 2xx `query_two_bits` answer. Apple answers a device whose bits were never set with a text body
/// instead of JSON, which reads as both bits clear.
pub fn parse_query(body: &str) -> BitState {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) else {
        return BitState::default();
    };
    BitState {
        bit0: fields.get("bit0").and_then(Value::as_bool).unwrap_or(false),
        bit1: fields.get("bit1").and_then(Value::as_bool).unwrap_or(false),
        last_update: fields.get("last_update_time").and_then(Value::as_str).map(str::to_string),
    }
}

/// Whether `bit` says the device already had the tenant's free start: set, and updated within `FRESH_FOR_MONTHS`.
/// A set bit without a readable update month counts, since only this service sets it.
pub fn free_start_spent(state: &BitState, bit: u8, now: DateTime<Utc>) -> bool {
    let set = match bit {
        0 => state.bit0,
        1 => state.bit1,
        _ => false,
    };
    if !set {
        return false;
    }
    match state.last_update.as_deref().and_then(month_index) {
        Some(updated) => month_index_of(now) - updated < FRESH_FOR_MONTHS,
        None => true,
    }
}

/// The two bits to write so `bit` is set and the other keeps the value Apple answered.
pub fn with_free_start(state: &BitState, bit: u8) -> (bool, bool) {
    match bit {
        0 => (true, state.bit1),
        1 => (state.bit0, true),
        _ => (state.bit0, state.bit1),
    }
}

/// The tenant's claimed bit from its `apps` value: only 0 and 1 are bits.
pub fn claimed_bit(value: Option<i64>) -> Option<u8> {
    match value {
        Some(0) => Some(0),
        Some(1) => Some(1),
        _ => None,
    }
}

fn month_index(text: &str) -> Option<i32> {
    let (year, month) = text.trim().split_once('-')?;
    let year: i32 = year.parse().ok()?;
    let month: i32 = month.parse().ok()?;
    (1..=12).contains(&month).then_some(year * 12 + month - 1)
}

fn month_index_of(now: DateTime<Utc>) -> i32 {
    now.year() * 12 + now.month0() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap()
    }

    fn state(bit0: bool, bit1: bool, month: Option<&str>) -> BitState {
        BitState { bit0, bit1, last_update: month.map(str::to_string) }
    }

    #[test]
    fn apples_answers_read_as_bits() {
        assert_eq!(parse_query(r#"{"bit0":true,"bit1":false,"last_update_time":"2026-11"}"#), state(true, false, Some("2026-11")));
        assert_eq!(parse_query(r#"{"bit0":false,"bit1":true}"#), state(false, true, None));
        assert_eq!(parse_query("Failed to find bit state"), BitState::default());
        assert_eq!(parse_query(""), BitState::default());
        assert_eq!(parse_query("[true]"), BitState::default());
        assert_eq!(parse_query(r#"{"bit0":"yes"}"#), BitState::default());
    }

    #[test]
    fn only_a_set_claimed_bit_spends_the_free_start() {
        let set0 = state(true, false, Some("2026-11"));
        assert!(free_start_spent(&set0, 0, now()));
        assert!(!free_start_spent(&set0, 1, now()));
        assert!(!free_start_spent(&BitState::default(), 0, now()));
        assert!(!free_start_spent(&set0, 2, now()));
    }

    #[test]
    fn a_bit_counts_for_twelve_months_after_its_update_month() {
        assert!(free_start_spent(&state(true, false, Some("2025-12")), 0, now()));
        assert!(!free_start_spent(&state(true, false, Some("2025-11")), 0, now()));
        assert!(!free_start_spent(&state(true, false, Some("2024-03")), 0, now()));
        assert!(free_start_spent(&state(true, false, None), 0, now()));
        assert!(free_start_spent(&state(true, false, Some("garbled")), 0, now()));
    }

    #[test]
    fn writing_the_claimed_bit_keeps_the_other() {
        assert_eq!(with_free_start(&state(false, true, None), 0), (true, true));
        assert_eq!(with_free_start(&state(false, false, None), 0), (true, false));
        assert_eq!(with_free_start(&state(true, false, None), 1), (true, true));
        assert_eq!(with_free_start(&state(false, true, None), 7), (false, true));
    }

    #[test]
    fn only_zero_and_one_are_claimable() {
        assert_eq!(claimed_bit(Some(0)), Some(0));
        assert_eq!(claimed_bit(Some(1)), Some(1));
        assert_eq!(claimed_bit(Some(2)), None);
        assert_eq!(claimed_bit(Some(-1)), None);
        assert_eq!(claimed_bit(None), None);
    }
}
