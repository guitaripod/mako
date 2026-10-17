use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::purchases::constant_time_eq;

pub const PLACEHOLDER_PREFIX: &str = "hashed:";
pub const BACKFILL_BATCH: u32 = 500;
pub const LAST_USED_REFRESH_HOURS: i64 = 1;
pub const PRUNE_AFTER_DAYS: i64 = 180;
pub const PRUNE_BATCH: u32 = 500;

/// SQL `LIKE` pattern that matches every placeholder, used to keep the
/// backfill and the plaintext fallback away from rows that are already hashed.
pub const PLACEHOLDER_LIKE: &str = "hashed:%";

/// Lowercase hex SHA-256 of an API key, the only form of the key the database
/// keeps.
pub fn hash_api_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

/// The non-secret value stored in `users.api_key` once the key lives only as a
/// hash. It embeds the user id, so it is unique and `api_key` stays NOT NULL
/// UNIQUE.
pub fn placeholder_for(user_id: &str) -> String {
    format!("{}{}", PLACEHOLDER_PREFIX, user_id)
}

/// True for a value written by `placeholder_for`. A placeholder is never a
/// credential, whatever a caller presents.
pub fn is_placeholder(stored: &str) -> bool {
    stored.starts_with(PLACEHOLDER_PREFIX)
}

/// A fresh random client key.
pub fn generate_api_key() -> String {
    format!("pixie_{}", Uuid::new_v4().to_string().replace('-', ""))
}

/// A newly issued key together with the two values the database stores for it.
/// Only `api_key` is ever returned to a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCredentials {
    pub api_key: String,
    pub api_key_hash: String,
    pub placeholder: String,
}

impl NewCredentials {
    pub fn generate(user_id: &str) -> Self {
        let api_key = generate_api_key();
        Self {
            api_key_hash: hash_api_key(&api_key),
            placeholder: placeholder_for(user_id),
            api_key,
        }
    }
}

/// Compares a stored hash with the hash of the presented key without stopping
/// at the first differing byte.
pub fn hash_matches(stored_hash: &str, presented_key: &str) -> bool {
    constant_time_eq(stored_hash.as_bytes(), hash_api_key(presented_key).as_bytes())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaintextFallback {
    Reject,
    Upgrade { api_key_hash: String },
}

/// What to do with a row found by the legacy plaintext lookup: reject
/// placeholders and mismatches, otherwise hand back the hash to write for that
/// row so the next request authenticates by hash.
pub fn plaintext_fallback(presented_key: &str, stored_plaintext: &str) -> PlaintextFallback {
    if is_placeholder(stored_plaintext) || !constant_time_eq(presented_key.as_bytes(), stored_plaintext.as_bytes()) {
        return PlaintextFallback::Reject;
    }
    PlaintextFallback::Upgrade { api_key_hash: hash_api_key(presented_key) }
}

/// True when the presented key can only ever be a placeholder, so the legacy
/// plaintext lookup is skipped.
pub fn skips_plaintext_lookup(presented_key: &str) -> bool {
    is_placeholder(presented_key)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupStep {
    KeyTable,
    LegacyHash,
    LegacyPlaintext,
}

/// The order authentication tries to resolve a presented key: the per-device
/// `api_keys` table, then the single legacy hash on `users`, then the legacy
/// plaintext column with upgrade on use. A placeholder can only ever be a
/// stored value, so it never reaches the plaintext step.
pub fn lookup_steps(presented_key: &str) -> &'static [LookupStep] {
    if skips_plaintext_lookup(presented_key) {
        &[LookupStep::KeyTable, LookupStep::LegacyHash]
    } else {
        &[LookupStep::KeyTable, LookupStep::LegacyHash, LookupStep::LegacyPlaintext]
    }
}

/// True when a key's `last_used_at` is missing, unreadable or older than the
/// refresh interval, so at most one write per key and hour reaches D1.
pub fn should_refresh_last_used(last_used_at: Option<&str>, now: DateTime<Utc>) -> bool {
    match last_used_at.and_then(|t| DateTime::parse_from_rfc3339(t).ok()) {
        Some(previous) => now.signed_duration_since(previous.with_timezone(&Utc)) >= Duration::hours(LAST_USED_REFRESH_HOURS),
        None => true,
    }
}

/// The oldest `last_used_at` (or `created_at`, for a key never used) a key may
/// have and survive the scheduled prune, in the RFC 3339 form both columns are
/// stored in, so it compares correctly as text.
pub fn prune_cutoff(now: DateTime<Utc>) -> String {
    (now - Duration::days(PRUNE_AFTER_DAYS)).to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_is_lowercase_hex_sha256() {
        assert_eq!(
            hash_api_key("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hash_api_key("pixie_x").len(), 64);
        assert_ne!(hash_api_key("pixie_a"), hash_api_key("pixie_b"));
    }

    #[test]
    fn placeholders_are_recognised_and_unique_per_user() {
        let p = placeholder_for("user-1");
        assert_eq!(p, "hashed:user-1");
        assert!(is_placeholder(&p));
        assert!(!is_placeholder("pixie_0123"));
        assert!(!is_placeholder(""));
        assert_ne!(placeholder_for("a"), placeholder_for("b"));
    }

    #[test]
    fn generated_keys_are_unique_and_never_placeholders() {
        let a = generate_api_key();
        let b = generate_api_key();
        assert!(a.starts_with("pixie_"));
        assert_ne!(a, b);
        assert!(!is_placeholder(&a));
    }

    #[test]
    fn new_credentials_hash_the_key_and_keep_the_plaintext_out_of_the_stored_values() {
        let c = NewCredentials::generate("u-7");
        assert!(hash_matches(&c.api_key_hash, &c.api_key));
        assert_eq!(c.placeholder, "hashed:u-7");
        assert_ne!(c.placeholder, c.api_key);
        assert!(!c.api_key_hash.contains(&c.api_key));
    }

    #[test]
    fn hash_matches_rejects_other_keys_and_malformed_hashes() {
        let stored = hash_api_key("pixie_good");
        assert!(hash_matches(&stored, "pixie_good"));
        assert!(!hash_matches(&stored, "pixie_bad"));
        assert!(!hash_matches(&stored, ""));
        assert!(!hash_matches("", "pixie_good"));
        assert!(!hash_matches(&stored[..63], "pixie_good"));
    }

    #[test]
    fn a_matching_plaintext_row_is_upgraded_with_the_hash_of_the_presented_key() {
        assert_eq!(
            plaintext_fallback("pixie_legacy", "pixie_legacy"),
            PlaintextFallback::Upgrade { api_key_hash: hash_api_key("pixie_legacy") }
        );
    }

    #[test]
    fn a_mismatching_plaintext_row_is_rejected() {
        assert_eq!(plaintext_fallback("pixie_a", "pixie_b"), PlaintextFallback::Reject);
        assert_eq!(plaintext_fallback("", "pixie_b"), PlaintextFallback::Reject);
        assert_eq!(plaintext_fallback("pixie_b", ""), PlaintextFallback::Reject);
    }

    #[test]
    fn a_placeholder_never_authenticates_even_when_presented_verbatim() {
        let p = placeholder_for("u-1");
        assert_eq!(plaintext_fallback(&p, &p), PlaintextFallback::Reject);
        assert!(skips_plaintext_lookup(&p));
        assert!(!skips_plaintext_lookup("pixie_real"));
    }

    #[test]
    fn the_like_pattern_matches_exactly_the_placeholders() {
        let prefix = PLACEHOLDER_LIKE.trim_end_matches('%');
        assert_eq!(prefix, PLACEHOLDER_PREFIX);
        assert!(placeholder_for("u").starts_with(prefix));
    }

    #[test]
    fn lookup_tries_the_key_table_then_the_legacy_hash_then_plaintext() {
        assert_eq!(
            lookup_steps("pixie_abc"),
            &[LookupStep::KeyTable, LookupStep::LegacyHash, LookupStep::LegacyPlaintext]
        );
    }

    #[test]
    fn a_placeholder_is_never_looked_up_as_plaintext() {
        assert_eq!(lookup_steps("hashed:u-1"), &[LookupStep::KeyTable, LookupStep::LegacyHash]);
    }

    #[test]
    fn last_used_is_refreshed_only_after_an_hour() {
        let now = DateTime::parse_from_rfc3339("2026-10-16T12:00:00Z").unwrap().with_timezone(&Utc);
        assert!(should_refresh_last_used(None, now));
        assert!(should_refresh_last_used(Some("not a time"), now));
        assert!(should_refresh_last_used(Some("2026-10-16T10:59:59Z"), now));
        assert!(should_refresh_last_used(Some("2026-10-16T11:00:00Z"), now));
        assert!(!should_refresh_last_used(Some("2026-10-16T11:00:01Z"), now));
        assert!(!should_refresh_last_used(Some("2026-10-16T12:00:00Z"), now));
        assert!(!should_refresh_last_used(Some("2026-10-16T13:30:00+02:00"), now));
    }

    #[test]
    fn the_prune_cutoff_is_180_days_before_now_in_the_stored_format() {
        let now = DateTime::parse_from_rfc3339("2026-10-16T12:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(prune_cutoff(now), "2026-04-19T12:00:00+00:00");
    }

    #[test]
    fn the_prune_cutoff_crosses_a_leap_day_and_a_year_boundary() {
        let leap = DateTime::parse_from_rfc3339("2028-08-26T00:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(prune_cutoff(leap), "2028-02-28T00:00:00+00:00");
        let new_year = DateTime::parse_from_rfc3339("2027-03-01T08:30:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(prune_cutoff(new_year), "2026-09-02T08:30:00+00:00");
    }

    #[test]
    fn the_prune_cutoff_sorts_as_text_against_timestamps_written_by_the_worker() {
        let now = DateTime::parse_from_rfc3339("2026-10-16T12:00:00Z").unwrap().with_timezone(&Utc);
        let cutoff = prune_cutoff(now);
        let stale = (now - Duration::days(181)).to_rfc3339();
        let fresh = (now - Duration::days(179)).to_rfc3339();
        assert!(stale < cutoff);
        assert!(fresh > cutoff);
        assert!(now.to_rfc3339() > cutoff);
    }

    #[test]
    fn the_prune_batch_is_bounded() {
        assert_eq!(PRUNE_BATCH, 500);
        assert_eq!(PRUNE_AFTER_DAYS, 180);
    }
}
