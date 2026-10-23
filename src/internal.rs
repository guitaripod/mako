use worker::{Env, Request};

use crate::error::AppError;
use crate::purchases::constant_time_eq;

/// The header the shared secret of an internal route travels in, as identity and the ledger use it.
pub const HEADER: &str = "X-Midgar-Internal";

/// The secret the ledger presents to `/internal/wallet/grant`. It can create credits, so it is not the secret identity
/// uses.
pub const GRANT_SECRET: &str = "WALLET_INTERNAL_SECRET";

/// The secret identity presents to `/internal/wallet/erase_all` and `/internal/wallet/merge`. It can destroy and move
/// credits, but never create them.
pub const LIFECYCLE_SECRET: &str = "WALLET_LIFECYCLE_SECRET";

/// What a caller of an internal route is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Granted,
    Hidden,
    Denied,
}

/// An unset or empty secret hides the route, as if it did not exist; a missing or wrong one is denied. The comparison
/// takes the same time whatever the two values share.
pub fn access(secret: Option<&str>, presented: Option<&str>) -> Access {
    let Some(secret) = secret.filter(|s| !s.is_empty()) else { return Access::Hidden };
    match presented {
        Some(value) if constant_time_eq(value.as_bytes(), secret.as_bytes()) => Access::Granted,
        _ => Access::Denied,
    }
}

/// Requires the named secret in `X-Midgar-Internal`: 404 when the worker has none, 401 when the request's is wrong.
pub fn require(req: &Request, env: &Env, secret_name: &str) -> Result<(), AppError> {
    let secret = env.secret(secret_name).ok().map(|s| s.to_string());
    let presented = req.headers().get(HEADER).ok().flatten();
    match access(secret.as_deref(), presented.as_deref()) {
        Access::Granted => Ok(()),
        Access::Hidden => Err(AppError::NotFound("No such route.".to_string())),
        Access::Denied => Err(AppError::Unauthorized("A valid secret is required.".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_without_the_secret_hides_the_route_whatever_is_presented() {
        assert_eq!(access(None, Some("anything")), Access::Hidden);
        assert_eq!(access(Some(""), Some("")), Access::Hidden);
        assert_eq!(access(None, None), Access::Hidden);
    }

    #[test]
    fn a_missing_or_wrong_secret_is_denied() {
        assert_eq!(access(Some("s3cret"), None), Access::Denied);
        assert_eq!(access(Some("s3cret"), Some("")), Access::Denied);
        assert_eq!(access(Some("s3cret"), Some("s3cre")), Access::Denied);
        assert_eq!(access(Some("s3cret"), Some("s3cret!")), Access::Denied);
    }

    #[test]
    fn the_exact_secret_is_granted() {
        assert_eq!(access(Some("s3cret"), Some("s3cret")), Access::Granted);
    }

    #[test]
    fn the_two_kinds_of_route_use_two_different_secrets() {
        assert_ne!(GRANT_SECRET, LIFECYCLE_SECRET);
    }
}
