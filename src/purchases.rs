use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};

pub const PREMIUM_CACHE_HOURS: i64 = 24;

/// Compares two byte strings without stopping at the first difference. Both
/// sides are hashed first so the comparison always covers 32 bytes, whatever
/// the lengths are.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let left = Sha256::digest(a);
    let right = Sha256::digest(b);
    left.iter().zip(right.iter()).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// Credits taken back for a refund: whatever was granted, but never more than
/// the wallet still holds, so the balance stops at zero.
pub fn refund_reversal(balance: i32, credited: u32) -> u32 {
    if balance <= 0 {
        return 0;
    }
    credited.min(balance as u32)
}

pub fn refund_reference(transaction_id: &str) -> String {
    format!("refund:{}", transaction_id)
}

/// The last known premium state while it is still fresh (24 hours), used when
/// the purchase ledger cannot be asked.
pub fn cached_premium(checked_at: Option<&str>, cached: Option<bool>, now: DateTime<Utc>) -> Option<bool> {
    let checked = DateTime::parse_from_rfc3339(checked_at?).ok()?.with_timezone(&Utc);
    let age = now - checked;
    if age < Duration::zero() || age > Duration::hours(PREMIUM_CACHE_HOURS) {
        return None;
    }
    cached
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn constant_time_eq_handles_unequal_lengths_and_empty_input() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn a_refund_takes_back_the_credits_down_to_zero() {
        assert_eq!(refund_reversal(50, 15), 15);
        assert_eq!(refund_reversal(15, 15), 15);
        assert_eq!(refund_reversal(4, 15), 4);
        assert_eq!(refund_reversal(0, 15), 0);
        assert_eq!(refund_reversal(-3, 15), 0);
        assert_eq!(refund_reversal(10, 0), 0);
    }

    #[test]
    fn refund_references_carry_the_transaction() {
        assert_eq!(refund_reference("2000001"), "refund:2000001");
    }

    #[test]
    fn the_premium_cache_lasts_a_day() {
        let now = Utc.with_ymd_and_hms(2026, 10, 12, 12, 0, 0).unwrap();
        assert_eq!(cached_premium(Some("2026-10-12T00:00:00+00:00"), Some(true), now), Some(true));
        assert_eq!(cached_premium(Some("2026-10-11T12:00:00Z"), Some(false), now), Some(false));
        assert_eq!(cached_premium(Some("2026-10-11T11:59:59Z"), Some(true), now), None);
        assert_eq!(cached_premium(None, Some(true), now), None);
        assert_eq!(cached_premium(Some("2026-10-12T00:00:00Z"), None, now), None);
        assert_eq!(cached_premium(Some("garbage"), Some(true), now), None);
        assert_eq!(cached_premium(Some("2026-10-13T00:00:00Z"), Some(true), now), None);
    }
}
