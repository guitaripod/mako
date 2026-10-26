use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
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

/// True when the `Authorization` header is exactly `Bearer <expected>`.
pub fn webhook_authorized(header: Option<&str>, expected: &str) -> bool {
    let wanted = format!("Bearer {}", expected);
    constant_time_eq(header.unwrap_or("").as_bytes(), wanted.as_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventAction {
    CreditPack,
    TagRevenueOnly,
    ReversePackRefund,
    LogOnly,
    Ignore,
}

/// What the webhook does with an event type. `NON_RENEWING_PURCHASE` credits
/// and `CANCELLATION` reverses only for tenants on strict purchases.
pub fn classify_event(event_type: &str, strict: bool) -> EventAction {
    match (event_type, strict) {
        ("INITIAL_PURCHASE" | "RENEWAL", _) => EventAction::CreditPack,
        ("NON_RENEWING_PURCHASE", true) => EventAction::CreditPack,
        ("NON_RENEWING_PURCHASE", false) => EventAction::TagRevenueOnly,
        ("CANCELLATION", true) => EventAction::ReversePackRefund,
        ("CANCELLATION" | "UNCANCELLATION" | "EXPIRATION" | "BILLING_ISSUE", _) => EventAction::LogOnly,
        _ => EventAction::Ignore,
    }
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

/// Whether an active entitlement item is the tenant's premium entitlement
/// without asking RevenueCat for more: it names the id or lookup key directly.
pub fn item_names_entitlement(item: &Value, required: &str) -> bool {
    ["entitlement_id", "lookup_key", "identifier"]
        .iter()
        .any(|field| item.get(*field).and_then(|v| v.as_str()) == Some(required))
}

/// The item's internal entitlement id, when a lookup is needed to learn its key.
pub fn entitlement_id_to_resolve(item: &Value, required: &str) -> Option<String> {
    if item_names_entitlement(item, required) || item.get("lookup_key").and_then(|v| v.as_str()).is_some() {
        return None;
    }
    item.get("entitlement_id").and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// A resolved entitlement object matches when its lookup key (or id) is the
/// configured one.
pub fn resolved_entitlement_matches(entitlement: &Value, required: &str) -> bool {
    item_names_entitlement(entitlement, required) || entitlement.get("id").and_then(|v| v.as_str()) == Some(required)
}

/// The last known premium state while it is still fresh (24 hours), used when
/// RevenueCat cannot be asked.
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
    use serde_json::json;

    #[test]
    fn the_webhook_secret_comparison_accepts_only_the_exact_bearer() {
        assert!(webhook_authorized(Some("Bearer s3cret"), "s3cret"));
        assert!(!webhook_authorized(Some("Bearer s3cret "), "s3cret"));
        assert!(!webhook_authorized(Some("bearer s3cret"), "s3cret"));
        assert!(!webhook_authorized(Some("s3cret"), "s3cret"));
        assert!(!webhook_authorized(Some("Bearer s3creT"), "s3cret"));
        assert!(!webhook_authorized(Some(""), "s3cret"));
        assert!(!webhook_authorized(None, "s3cret"));
    }

    #[test]
    fn constant_time_eq_handles_unequal_lengths_and_empty_input() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn other_tenants_keep_the_old_event_handling() {
        assert_eq!(classify_event("INITIAL_PURCHASE", false), EventAction::CreditPack);
        assert_eq!(classify_event("RENEWAL", false), EventAction::CreditPack);
        assert_eq!(classify_event("NON_RENEWING_PURCHASE", false), EventAction::TagRevenueOnly);
        assert_eq!(classify_event("CANCELLATION", false), EventAction::LogOnly);
        assert_eq!(classify_event("UNCANCELLATION", false), EventAction::LogOnly);
        assert_eq!(classify_event("EXPIRATION", false), EventAction::LogOnly);
        assert_eq!(classify_event("BILLING_ISSUE", false), EventAction::LogOnly);
        assert_eq!(classify_event("TEST", false), EventAction::Ignore);
    }

    #[test]
    fn strict_tenants_credit_consumables_and_reverse_refunds() {
        assert_eq!(classify_event("NON_RENEWING_PURCHASE", true), EventAction::CreditPack);
        assert_eq!(classify_event("CANCELLATION", true), EventAction::ReversePackRefund);
        assert_eq!(classify_event("EXPIRATION", true), EventAction::LogOnly);
        assert_eq!(classify_event("INITIAL_PURCHASE", true), EventAction::CreditPack);
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
    fn premium_needs_the_configured_entitlement_not_any_entitlement() {
        let premium = json!({ "object": "customer.active_entitlement", "entitlement_id": "premium" });
        let by_key = json!({ "lookup_key": "premium", "entitlement_id": "entl123" });
        let other = json!({ "object": "customer.active_entitlement", "entitlement_id": "entl999", "lookup_key": "plus_trial" });
        assert!(item_names_entitlement(&premium, "premium"));
        assert!(item_names_entitlement(&by_key, "premium"));
        assert!(!item_names_entitlement(&other, "premium"));
        assert!(!item_names_entitlement(&json!({}), "premium"));
    }

    #[test]
    fn an_unnamed_internal_entitlement_id_is_resolved_through_revenuecat() {
        let internal = json!({ "object": "customer.active_entitlement", "entitlement_id": "entl123" });
        assert_eq!(entitlement_id_to_resolve(&internal, "premium"), Some("entl123".to_string()));
        let named = json!({ "entitlement_id": "entl123", "lookup_key": "other" });
        assert_eq!(entitlement_id_to_resolve(&named, "premium"), None);
        assert_eq!(entitlement_id_to_resolve(&json!({ "entitlement_id": "premium" }), "premium"), None);
        assert_eq!(entitlement_id_to_resolve(&json!({}), "premium"), None);
    }

    #[test]
    fn a_resolved_entitlement_matches_by_lookup_key() {
        assert!(resolved_entitlement_matches(&json!({ "id": "entl123", "lookup_key": "premium" }), "premium"));
        assert!(!resolved_entitlement_matches(&json!({ "id": "entl123", "lookup_key": "other" }), "premium"));
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
