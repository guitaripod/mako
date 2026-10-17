use std::cell::RefCell;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;
use worker::{console_log, Env, Request};

mod keys;
mod subtle;

use keys::{load_keys, IdentityJwks, Jwk, JwksFetcher, KeyCache};
use subtle::SubtleCheck;

/// The header a Midgar-aware client sends next to its API key.
pub const TOKEN_HEADER: &str = "X-Midgar-Token";

/// The `iss` claim identity writes.
const ISSUER: &str = "midgar-identity";

/// The `aud` claim identity writes.
const AUDIENCE: &str = "midgar";

/// The only signing algorithm identity uses.
const ALGORITHM: &str = "ES256";

/// Longest token read at all; identity's are about 300 bytes.
const MAX_TOKEN_BYTES: usize = 4096;

/// Length of an ES256 signature in the raw `r || s` form.
const SIGNATURE_BYTES: usize = 64;

/// How well the session behind a token proved its device, as identity records it in the `att` claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    AppAttest,
    Development,
    Unverified,
}

impl Tier {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "app_attest" => Some(Self::AppAttest),
            "development" => Some(Self::Development),
            "unverified" => Some(Self::Unverified),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AppAttest => "app_attest",
            Self::Development => "development",
            Self::Unverified => "unverified",
        }
    }
}

/// Who a verified Midgar token names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidgarCaller {
    /// The token's `sub`, lowercased.
    pub midgar_id: String,
    pub tier: Tier,
}

/// Why a token was not accepted. Only ever logged; the request carries on with its API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Malformed,
    UnknownKey,
    BadSignature,
    InvalidClaims,
    Expired,
    WrongApp,
    KeysUnavailable,
}

impl Rejection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::UnknownKey => "unknown_key",
            Self::BadSignature => "bad_signature",
            Self::InvalidClaims => "invalid_claims",
            Self::Expired => "expired",
            Self::WrongApp => "wrong_app",
            Self::KeysUnavailable => "keys_unavailable",
        }
    }
}

/// The three segments of a token, decoded but not yet trusted.
#[derive(Debug)]
struct ParsedToken {
    kid: String,
    signing_input: String,
    signature: Vec<u8>,
    claims: Value,
}

/// Decodes one base64url JSON object segment.
fn decode_object(segment: &str) -> Option<Value> {
    let bytes = URL_SAFE_NO_PAD.decode(segment).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok().filter(Value::is_object)
}

/// Splits a compact token and decodes it. Requires three segments, object header and payload, `alg` ES256 and a
/// string `kid`; says nothing about whether any of it is true.
fn parse(token: &str) -> Result<ParsedToken, Rejection> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(Rejection::Malformed);
    }
    let parts: Vec<&str> = token.split('.').collect();
    let [header_part, payload_part, signature_part] = parts[..] else {
        return Err(Rejection::Malformed);
    };
    let header = decode_object(header_part).ok_or(Rejection::Malformed)?;
    let claims = decode_object(payload_part).ok_or(Rejection::Malformed)?;
    let signature = URL_SAFE_NO_PAD.decode(signature_part).map_err(|_| Rejection::Malformed)?;
    if header.get("alg").and_then(Value::as_str) != Some(ALGORITHM) {
        return Err(Rejection::Malformed);
    }
    let kid = header.get("kid").and_then(Value::as_str).ok_or(Rejection::Malformed)?.to_string();
    Ok(ParsedToken { kid, signing_input: format!("{header_part}.{payload_part}"), signature, claims })
}

/// Checks the claims of a token whose signature already verified: every claim present with its type, issuer and
/// audience as identity writes them, not expired at `now_secs`, and issued to the app named by `app_slug`.
/// Order matches the shared TypeScript verifier: shape, issuer and audience, expiry, then the app.
fn check_claims(claims: &Value, now_secs: u64, app_slug: &str) -> Result<MidgarCaller, Rejection> {
    let text = |name: &str| claims.get(name).and_then(Value::as_str);
    let number = |name: &str| claims.get(name).filter(|v| v.is_number()).and_then(Value::as_f64);
    let subject = text("sub").filter(|s| !s.is_empty()).ok_or(Rejection::InvalidClaims)?;
    let tier = text("att").and_then(Tier::parse).ok_or(Rejection::InvalidClaims)?;
    let key_ok = match claims.get("key") {
        None => true,
        Some(value) => value.as_str().is_some_and(|k| !k.is_empty()),
    };
    let (Some(issuer), Some(audience), Some(app), Some(_issued_at), Some(expires_at)) =
        (text("iss"), text("aud"), text("app"), number("iat"), number("exp"))
    else {
        return Err(Rejection::InvalidClaims);
    };
    if !key_ok || issuer != ISSUER || audience != AUDIENCE {
        return Err(Rejection::InvalidClaims);
    }
    if expires_at <= now_secs as f64 {
        return Err(Rejection::Expired);
    }
    if app != app_slug {
        return Err(Rejection::WrongApp);
    }
    Ok(MidgarCaller { midgar_id: subject.to_lowercase(), tier })
}

/// Verifies one signature against a key; the real check is `SubtleCrypto`, tests use a stand-in.
pub trait SignatureCheck {
    async fn valid(&self, jwk: &Jwk, signature: &[u8], input: &[u8]) -> bool;
}

/// Verifies a token end to end with the given key source and signature check. A `kid` missing from the cached
/// keys forces one refresh (rate limited by the cache) before the token is called unknown.
async fn verify_with(
    token: &str,
    app_slug: &str,
    now_ms: u64,
    cache: &RefCell<KeyCache>,
    fetcher: &impl JwksFetcher,
    check: &impl SignatureCheck,
) -> Result<MidgarCaller, Rejection> {
    let parsed = parse(token)?;
    let unavailable = |_| Rejection::KeysUnavailable;
    let mut keys = load_keys(cache, fetcher, now_ms, false).await.map_err(unavailable)?;
    if !keys.iter().any(|k| k.kid == parsed.kid) {
        keys = load_keys(cache, fetcher, now_ms, true).await.map_err(unavailable)?;
    }
    let jwk = keys.iter().find(|k| k.kid == parsed.kid).ok_or(Rejection::UnknownKey)?;
    let well_formed = parsed.signature.len() == SIGNATURE_BYTES;
    if !well_formed || !check.valid(jwk, &parsed.signature, parsed.signing_input.as_bytes()).await {
        return Err(Rejection::BadSignature);
    }
    check_claims(&parsed.claims, now_ms / 1000, app_slug)
}

thread_local! {
    static KEYS: &'static RefCell<KeyCache> = Box::leak(Box::new(RefCell::new(KeyCache::new())));
}

/// Verifies a token against identity's keys, cached per isolate.
async fn verify(env: &Env, token: &str, app_slug: &str) -> Result<MidgarCaller, Rejection> {
    let fetcher = IdentityJwks(env.service("IDENTITY").map_err(|_| Rejection::KeysUnavailable)?);
    let cache: &'static RefCell<KeyCache> = KEYS.with(|cache| *cache);
    let now_ms = worker::Date::now().as_millis();
    verify_with(token, app_slug, now_ms, cache, &fetcher, &SubtleCheck).await
}

/// The first characters of an id, enough to correlate log lines without recording the id.
fn id_prefix(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Verifies a token a request presents as its only credential. `None` when it is not acceptable, logged like an
/// ignored header token.
pub async fn caller_from_token(env: &Env, token: &str, app_id: &str) -> Option<MidgarCaller> {
    let slug = crate::tenant::registry_slug(app_id);
    match verify(env, token, &slug).await {
        Ok(caller) => {
            console_log!("midgar_token recognised app={} tier={} id={} native", app_id, caller.tier.as_str(), id_prefix(&caller.midgar_id));
            Some(caller)
        }
        Err(rejection) => {
            console_log!("midgar_token refused app={} reason={} native", app_id, rejection.as_str());
            None
        }
    }
}

/// Reads `X-Midgar-Token` from a request that already authenticated with its API key. Returns the verified
/// caller, or `None` when there is no token or it is not acceptable; a bad token is logged and ignored so a
/// clock or JWKS problem never breaks an app that worked yesterday.
pub async fn caller_from_request(req: &Request, env: &Env, app_id: &str) -> Option<MidgarCaller> {
    let token = req.headers().get(TOKEN_HEADER).ok().flatten().filter(|t| !t.is_empty())?;
    let slug = crate::tenant::registry_slug(app_id);
    match verify(env, &token, &slug).await {
        Ok(caller) => {
            console_log!("midgar_token recognised app={} tier={} id={}", app_id, caller.tier.as_str(), id_prefix(&caller.midgar_id));
            Some(caller)
        }
        Err(rejection) => {
            console_log!("midgar_token ignored app={} reason={}", app_id, rejection.as_str());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::keys::{FetchFailed, KEY_REFRESH_MIN_MS};
    use super::*;
    use futures::executor::block_on;
    use serde_json::json;
    use std::cell::Cell;

    const VECTORS: &str = include_str!("../test-vectors/midgar-token.json");

    struct Vectors(Value);

    impl Vectors {
        fn load() -> Self {
            Self(serde_json::from_str(VECTORS).unwrap())
        }

        fn token(&self, name: &str) -> String {
            self.0["tokens"][name].as_str().unwrap().to_string()
        }

        fn claims(&self, name: &str) -> Value {
            parse(&self.token(name)).unwrap().claims
        }

        fn jwks(&self, name: &str) -> Vec<Jwk> {
            keys::parse_jwks(&self.0[name]).unwrap()
        }

        fn iat(&self) -> u64 {
            self.0["iat"].as_u64().unwrap()
        }

        fn exp(&self) -> u64 {
            self.0["exp"].as_u64().unwrap()
        }

        fn midgar_id(&self) -> String {
            self.0["midgar_id"].as_str().unwrap().to_lowercase()
        }
    }

    struct FakeIdentity {
        responses: RefCell<Vec<Result<Vec<Jwk>, FetchFailed>>>,
        calls: Cell<u32>,
    }

    impl FakeIdentity {
        fn serving(responses: Vec<Result<Vec<Jwk>, FetchFailed>>) -> Self {
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

    struct Signature(bool);

    impl SignatureCheck for Signature {
        async fn valid(&self, _jwk: &Jwk, _signature: &[u8], _input: &[u8]) -> bool {
            self.0
        }
    }

    fn verify_vector(vectors: &Vectors, name: &str, slug: &str, now_secs: u64, identity: &FakeIdentity, cache: &RefCell<KeyCache>) -> Result<MidgarCaller, Rejection> {
        block_on(verify_with(&vectors.token(name), slug, now_secs * 1000, cache, identity, &Signature(true)))
    }

    fn fresh(vectors: &Vectors) -> (FakeIdentity, RefCell<KeyCache>) {
        (FakeIdentity::serving(vec![Ok(vectors.jwks("jwks"))]), RefCell::new(KeyCache::new()))
    }

    #[test]
    fn an_attested_token_names_the_caller_and_the_tier() {
        let vectors = Vectors::load();
        let (identity, cache) = fresh(&vectors);
        let caller = verify_vector(&vectors, "attested", "dream-eater", vectors.iat() + 5, &identity, &cache).unwrap();
        assert_eq!(caller, MidgarCaller { midgar_id: vectors.midgar_id(), tier: Tier::AppAttest });
    }

    #[test]
    fn the_subject_is_lowercased() {
        let vectors = Vectors::load();
        assert_ne!(vectors.0["midgar_id"].as_str().unwrap(), vectors.midgar_id());
        let (identity, cache) = fresh(&vectors);
        let caller = verify_vector(&vectors, "attested", "dream-eater", vectors.iat(), &identity, &cache).unwrap();
        assert_eq!(caller.midgar_id, vectors.midgar_id());
    }

    #[test]
    fn every_tier_identity_issues_is_understood() {
        let vectors = Vectors::load();
        let (identity, cache) = fresh(&vectors);
        let now = vectors.iat();
        assert_eq!(verify_vector(&vectors, "development", "pay-day", now, &identity, &cache).unwrap().tier, Tier::Development);
        assert_eq!(verify_vector(&vectors, "unverified", "pixiepocket", now, &identity, &cache).unwrap().tier, Tier::Unverified);
    }

    #[test]
    fn a_token_is_valid_until_exp_and_expired_from_exp() {
        let vectors = Vectors::load();
        let (identity, cache) = fresh(&vectors);
        assert!(verify_vector(&vectors, "attested", "dream-eater", vectors.exp() - 1, &identity, &cache).is_ok());
        assert_eq!(verify_vector(&vectors, "attested", "dream-eater", vectors.exp(), &identity, &cache), Err(Rejection::Expired));
        assert_eq!(verify_vector(&vectors, "attested", "dream-eater", vectors.exp() + 86_400, &identity, &cache), Err(Rejection::Expired));
    }

    #[test]
    fn a_token_for_another_app_is_refused() {
        let vectors = Vectors::load();
        let (identity, cache) = fresh(&vectors);
        let now = vectors.iat();
        assert_eq!(verify_vector(&vectors, "attested", "pay-day", now, &identity, &cache), Err(Rejection::WrongApp));
        assert_eq!(verify_vector(&vectors, "attested", "dreameater", now, &identity, &cache), Err(Rejection::WrongApp));
    }

    #[test]
    fn claims_with_the_wrong_issuer_audience_or_shape_are_refused() {
        let vectors = Vectors::load();
        let (identity, cache) = fresh(&vectors);
        for name in ["wrong_issuer", "wrong_audience", "wrong_tier", "missing_tier", "empty_subject"] {
            assert_eq!(
                verify_vector(&vectors, name, "dream-eater", vectors.iat(), &identity, &cache),
                Err(Rejection::InvalidClaims),
                "{name}"
            );
        }
    }

    #[test]
    fn structurally_broken_tokens_are_malformed() {
        let vectors = Vectors::load();
        let (identity, cache) = fresh(&vectors);
        for name in ["hs256", "no_kid"] {
            assert_eq!(verify_vector(&vectors, name, "dream-eater", vectors.iat(), &identity, &cache), Err(Rejection::Malformed), "{name}");
        }
        for token in ["", "a.b", "a.b.c.d", "!!!.!!!.!!!", "e30.e30.", "eyJhbGciOiJFUzI1NiJ9.e30"] {
            assert_eq!(parse(token).unwrap_err(), Rejection::Malformed, "{token}");
        }
        assert_eq!(parse(&"a".repeat(MAX_TOKEN_BYTES + 1)).unwrap_err(), Rejection::Malformed);
    }

    #[test]
    fn a_token_whose_key_is_not_published_is_unknown() {
        let vectors = Vectors::load();
        let (identity, cache) = fresh(&vectors);
        for name in ["other_signer", "unknown_kid"] {
            assert_eq!(verify_vector(&vectors, name, "dream-eater", vectors.iat(), &identity, &cache), Err(Rejection::UnknownKey), "{name}");
        }
    }

    #[test]
    fn a_failing_signature_check_refuses_the_token_before_any_claim_matters() {
        let vectors = Vectors::load();
        let (identity, cache) = fresh(&vectors);
        let outcome = block_on(verify_with(&vectors.token("attested"), "dream-eater", vectors.iat() * 1000, &cache, &identity, &Signature(false)));
        assert_eq!(outcome, Err(Rejection::BadSignature));
        let expired = block_on(verify_with(&vectors.token("attested"), "dream-eater", vectors.exp() * 1000, &cache, &identity, &Signature(false)));
        assert_eq!(expired, Err(Rejection::BadSignature));
    }

    #[test]
    fn a_signature_of_the_wrong_length_never_reaches_the_crypto() {
        struct MustNotRun;
        impl SignatureCheck for MustNotRun {
            async fn valid(&self, _: &Jwk, _: &[u8], _: &[u8]) -> bool {
                panic!("the signature check ran")
            }
        }
        let vectors = Vectors::load();
        let (identity, cache) = fresh(&vectors);
        let token = vectors.token("attested");
        let mut parts: Vec<&str> = token.split('.').collect();
        parts[2] = "AAAA";
        let outcome = block_on(verify_with(&parts.join("."), "dream-eater", vectors.iat() * 1000, &cache, &identity, &MustNotRun));
        assert_eq!(outcome, Err(Rejection::BadSignature));
    }

    #[test]
    fn an_unknown_kid_forces_one_refresh_and_a_new_key_is_then_accepted() {
        let vectors = Vectors::load();
        let identity = FakeIdentity::serving(vec![Ok(vectors.jwks("other_jwks")), Ok(vectors.jwks("jwks"))]);
        let cache = RefCell::new(KeyCache::new());
        let start = vectors.iat();
        let before_rotation = verify_vector(&vectors, "attested", "dream-eater", start, &identity, &cache);
        assert_eq!(before_rotation, Err(Rejection::UnknownKey));
        assert_eq!(identity.calls.get(), 1);
        let after = verify_vector(&vectors, "attested", "dream-eater", start + KEY_REFRESH_MIN_MS / 1000, &identity, &cache);
        assert!(after.is_ok());
        assert_eq!(identity.calls.get(), 2);
    }

    #[test]
    fn an_unknown_kid_cannot_force_a_second_fetch_within_a_minute() {
        let vectors = Vectors::load();
        let (identity, cache) = fresh(&vectors);
        let now = vectors.iat();
        for _ in 0..5 {
            assert_eq!(verify_vector(&vectors, "unknown_kid", "dream-eater", now, &identity, &cache), Err(Rejection::UnknownKey));
        }
        assert_eq!(identity.calls.get(), 1);
    }

    #[test]
    fn the_last_keys_still_verify_while_identity_is_down() {
        let vectors = Vectors::load();
        let identity = FakeIdentity::serving(vec![Ok(vectors.jwks("jwks")), Err(FetchFailed)]);
        let cache = RefCell::new(KeyCache::new());
        let first = vectors.iat();
        assert!(verify_vector(&vectors, "attested", "dream-eater", first, &identity, &cache).is_ok());
        let later = first + 10 * 60;
        assert!(verify_vector(&vectors, "attested", "dream-eater", later, &identity, &cache).is_ok());
        assert_eq!(identity.calls.get(), 2);
    }

    #[test]
    fn with_identity_down_from_the_start_the_token_is_ignored_for_lack_of_keys() {
        let vectors = Vectors::load();
        let identity = FakeIdentity::serving(vec![Err(FetchFailed)]);
        let cache = RefCell::new(KeyCache::new());
        assert_eq!(verify_vector(&vectors, "attested", "dream-eater", vectors.iat(), &identity, &cache), Err(Rejection::KeysUnavailable));
    }

    #[test]
    fn claim_types_are_checked_like_the_shared_verifier_does() {
        let vectors = Vectors::load();
        let good = vectors.claims("attested");
        let now = vectors.iat();
        let with = |name: &str, value: Value| {
            let mut claims = good.clone();
            claims[name] = value;
            claims
        };
        assert!(check_claims(&good, now, "dream-eater").is_ok());
        assert_eq!(check_claims(&with("exp", json!("later")), now, "dream-eater"), Err(Rejection::InvalidClaims));
        assert_eq!(check_claims(&with("iat", Value::Null), now, "dream-eater"), Err(Rejection::InvalidClaims));
        assert_eq!(check_claims(&with("key", json!("")), now, "dream-eater"), Err(Rejection::InvalidClaims));
        assert_eq!(check_claims(&with("key", json!(7)), now, "dream-eater"), Err(Rejection::InvalidClaims));
        assert_eq!(check_claims(&with("app", json!(7)), now, "dream-eater"), Err(Rejection::InvalidClaims));
        let mut without_key = good.clone();
        without_key.as_object_mut().unwrap().remove("key");
        assert!(check_claims(&without_key, now, "dream-eater").is_ok());
    }

    #[test]
    fn an_id_prefix_is_short_and_safe_on_any_text() {
        assert_eq!(id_prefix("0194f3a2-7b1c-4d2e"), "0194f3a2");
        assert_eq!(id_prefix("ab"), "ab");
        assert_eq!(id_prefix("ééééééééééé"), "éééééééé");
    }
}
