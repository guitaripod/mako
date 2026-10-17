use std::cell::RefCell;

use serde_json::Value;
use worker::Fetcher;

/// Where identity publishes its signing keys, reached over the `IDENTITY` service binding.
pub const JWKS_URL: &str = "https://identity.internal/.well-known/jwks.json";

/// How long a fetched key set is used without asking identity again.
pub const KEY_CACHE_MS: u64 = 5 * 60 * 1000;

/// The shortest time between two fetches, whether forced by an unknown `kid` or retried after identity failed.
pub const KEY_REFRESH_MIN_MS: u64 = 60 * 1000;

/// The public half of one identity signing key, as the JWKS lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jwk {
    pub kty: String,
    pub crv: String,
    pub x: String,
    pub y: String,
    pub kid: String,
}

impl Jwk {
    /// Reads one JWKS entry, or `None` when a member the verifier needs is missing or not a string.
    pub fn from_value(value: &Value) -> Option<Self> {
        let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_string);
        Some(Self { kty: text("kty")?, crv: text("crv")?, x: text("x")?, y: text("y")?, kid: text("kid")? })
    }
}

/// Reads the `keys` array of a JWKS document, skipping entries that are not usable keys.
pub fn parse_jwks(body: &Value) -> Option<Vec<Jwk>> {
    let keys = body.get("keys")?.as_array()?;
    Some(keys.iter().filter_map(Jwk::from_value).collect())
}

/// Identity could not be reached or answered something that is not a key set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchFailed;

/// A source of the current key set; the real one calls identity, tests use a fake.
pub trait JwksFetcher {
    async fn fetch(&self) -> Result<Vec<Jwk>, FetchFailed>;
}

/// Identity's JWKS over the `IDENTITY` service binding.
pub struct IdentityJwks(pub Fetcher);

impl JwksFetcher for IdentityJwks {
    async fn fetch(&self) -> Result<Vec<Jwk>, FetchFailed> {
        let mut response = self.0.fetch(JWKS_URL, None).await.map_err(|_| FetchFailed)?;
        if response.status_code() != 200 {
            return Err(FetchFailed);
        }
        let body: Value = response.json().await.map_err(|_| FetchFailed)?;
        parse_jwks(&body).ok_or(FetchFailed)
    }
}

/// The key set of one isolate and when it was fetched or last failed to be.
#[derive(Debug, Default)]
pub struct KeyCache {
    keys: Option<Vec<Jwk>>,
    fetched_at: u64,
    failed_at: Option<u64>,
}

impl KeyCache {
    pub const fn new() -> Self {
        Self { keys: None, fetched_at: 0, failed_at: None }
    }

    /// The cached keys when they may be used without a fetch: younger than five minutes, or younger than one
    /// minute when `refresh` asks for a forced reload, or identity failed less than a minute ago.
    pub fn reusable(&self, now: u64, refresh: bool) -> Option<Vec<Jwk>> {
        let keys = self.keys.as_ref()?;
        let limit = if refresh { KEY_REFRESH_MIN_MS } else { KEY_CACHE_MS };
        let fresh = now.saturating_sub(self.fetched_at) < limit;
        let backing_off = self.failed_at.is_some_and(|at| now.saturating_sub(at) < KEY_REFRESH_MIN_MS);
        (fresh || backing_off).then(|| keys.clone())
    }

    fn store(&mut self, keys: Vec<Jwk>, now: u64) {
        self.keys = Some(keys);
        self.fetched_at = now;
        self.failed_at = None;
    }

    fn note_failure(&mut self, now: u64) {
        self.failed_at = Some(now);
    }

    fn last_keys(&self) -> Option<Vec<Jwk>> {
        self.keys.clone()
    }
}

/// Returns the key set: from the cache while it is usable, else fetched. When identity is down the last keys
/// are reused, and the failure is remembered for a minute so a dead identity costs one attempt a minute, not one per
/// request. With no keys at all it fails.
pub async fn load_keys(cache: &RefCell<KeyCache>, fetcher: &impl JwksFetcher, now: u64, refresh: bool) -> Result<Vec<Jwk>, FetchFailed> {
    let reusable = cache.borrow().reusable(now, refresh);
    if let Some(keys) = reusable {
        return Ok(keys);
    }
    match fetcher.fetch().await {
        Ok(keys) => {
            cache.borrow_mut().store(keys.clone(), now);
            Ok(keys)
        }
        Err(failed) => {
            let mut cache = cache.borrow_mut();
            cache.note_failure(now);
            cache.last_keys().ok_or(failed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use serde_json::json;
    use std::cell::{Cell, RefCell};

    struct FakeIdentity {
        responses: RefCell<Vec<Result<Vec<Jwk>, FetchFailed>>>,
        calls: Cell<u32>,
    }

    impl FakeIdentity {
        fn new(responses: Vec<Result<Vec<Jwk>, FetchFailed>>) -> Self {
            Self { responses: RefCell::new(responses), calls: Cell::new(0) }
        }
    }

    impl JwksFetcher for FakeIdentity {
        async fn fetch(&self) -> Result<Vec<Jwk>, FetchFailed> {
            self.calls.set(self.calls.get() + 1);
            let mut responses = self.responses.borrow_mut();
            if responses.len() > 1 {
                responses.remove(0)
            } else {
                responses[0].clone()
            }
        }
    }

    fn key(kid: &str) -> Jwk {
        Jwk { kty: "EC".into(), crv: "P-256".into(), x: "x".into(), y: "y".into(), kid: kid.into() }
    }

    fn load(cache: &RefCell<KeyCache>, fetcher: &FakeIdentity, now: u64, refresh: bool) -> Result<Vec<Jwk>, FetchFailed> {
        block_on(load_keys(cache, fetcher, now, refresh))
    }

    #[test]
    fn the_first_load_fetches_and_the_next_five_minutes_reuse_it() {
        let cache = RefCell::new(KeyCache::new());
        let identity = FakeIdentity::new(vec![Ok(vec![key("a")])]);
        assert_eq!(load(&cache, &identity, 1_000, false), Ok(vec![key("a")]));
        assert_eq!(load(&cache, &identity, 1_000 + KEY_CACHE_MS - 1, false), Ok(vec![key("a")]));
        assert_eq!(identity.calls.get(), 1);
    }

    #[test]
    fn the_keys_expire_after_five_minutes() {
        let cache = RefCell::new(KeyCache::new());
        let identity = FakeIdentity::new(vec![Ok(vec![key("a")]), Ok(vec![key("b")])]);
        load(&cache, &identity, 1_000, false).unwrap();
        assert_eq!(load(&cache, &identity, 1_000 + KEY_CACHE_MS, false), Ok(vec![key("b")]));
        assert_eq!(identity.calls.get(), 2);
    }

    #[test]
    fn a_forced_refresh_is_allowed_once_a_minute() {
        let cache = RefCell::new(KeyCache::new());
        let identity = FakeIdentity::new(vec![Ok(vec![key("a")]), Ok(vec![key("a"), key("b")])]);
        load(&cache, &identity, 1_000, false).unwrap();
        assert_eq!(load(&cache, &identity, 1_000 + KEY_REFRESH_MIN_MS - 1, true), Ok(vec![key("a")]));
        assert_eq!(identity.calls.get(), 1);
        assert_eq!(load(&cache, &identity, 1_000 + KEY_REFRESH_MIN_MS, true), Ok(vec![key("a"), key("b")]));
        assert_eq!(identity.calls.get(), 2);
    }

    #[test]
    fn a_forced_refresh_replaces_the_cache_for_later_plain_loads() {
        let cache = RefCell::new(KeyCache::new());
        let identity = FakeIdentity::new(vec![Ok(vec![key("a")]), Ok(vec![key("b")])]);
        load(&cache, &identity, 0, false).unwrap();
        load(&cache, &identity, KEY_REFRESH_MIN_MS, true).unwrap();
        assert_eq!(load(&cache, &identity, KEY_REFRESH_MIN_MS + 1, false), Ok(vec![key("b")]));
        assert_eq!(identity.calls.get(), 2);
    }

    #[test]
    fn the_last_keys_are_used_while_identity_is_down() {
        let cache = RefCell::new(KeyCache::new());
        let identity = FakeIdentity::new(vec![Ok(vec![key("a")]), Err(FetchFailed)]);
        load(&cache, &identity, 0, false).unwrap();
        assert_eq!(load(&cache, &identity, KEY_CACHE_MS, false), Ok(vec![key("a")]));
        assert_eq!(identity.calls.get(), 2);
    }

    #[test]
    fn a_dead_identity_is_asked_once_a_minute_not_once_a_request() {
        let cache = RefCell::new(KeyCache::new());
        let identity = FakeIdentity::new(vec![Ok(vec![key("a")]), Err(FetchFailed)]);
        load(&cache, &identity, 0, false).unwrap();
        let down = KEY_CACHE_MS;
        load(&cache, &identity, down, false).unwrap();
        load(&cache, &identity, down + 1_000, false).unwrap();
        load(&cache, &identity, down + 1_000, true).unwrap();
        assert_eq!(identity.calls.get(), 2);
        load(&cache, &identity, down + KEY_REFRESH_MIN_MS, false).unwrap();
        assert_eq!(identity.calls.get(), 3);
    }

    #[test]
    fn with_no_keys_and_identity_down_the_load_fails() {
        let cache = RefCell::new(KeyCache::new());
        let identity = FakeIdentity::new(vec![Err(FetchFailed)]);
        assert_eq!(load(&cache, &identity, 0, false), Err(FetchFailed));
        assert_eq!(load(&cache, &identity, 1, false), Err(FetchFailed));
        assert_eq!(identity.calls.get(), 2);
    }

    #[test]
    fn identity_coming_back_replaces_the_stale_keys() {
        let cache = RefCell::new(KeyCache::new());
        let identity = FakeIdentity::new(vec![Ok(vec![key("a")]), Err(FetchFailed), Ok(vec![key("c")])]);
        load(&cache, &identity, 0, false).unwrap();
        load(&cache, &identity, KEY_CACHE_MS, false).unwrap();
        assert_eq!(load(&cache, &identity, KEY_CACHE_MS + KEY_REFRESH_MIN_MS, false), Ok(vec![key("c")]));
    }

    #[test]
    fn a_jwks_document_yields_its_usable_keys() {
        let body = json!({"keys": [
            {"kty": "EC", "crv": "P-256", "x": "x1", "y": "y1", "kid": "one", "alg": "ES256", "use": "sig"},
            {"kty": "EC", "crv": "P-256", "x": "x2", "kid": "missing-y"},
            {"kty": "EC", "crv": "P-256", "x": 1, "y": "y3", "kid": "bad-type"}
        ]});
        let keys = parse_jwks(&body).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].kid, "one");
    }

    #[test]
    fn a_body_without_a_keys_array_is_not_a_key_set() {
        assert_eq!(parse_jwks(&json!({"error": "x"})), None);
        assert_eq!(parse_jwks(&json!({"keys": "nope"})), None);
    }
}
