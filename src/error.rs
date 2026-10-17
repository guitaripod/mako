use worker::{Response, Result};
use crate::models::{ErrorResponse, ErrorDetail};

/// Seconds a client is told to wait after a 503 `wallet_unavailable`.
const WALLET_RETRY_AFTER_SECONDS: &str = "5";

/// Seconds a client is told to wait after a 409 `credit_pending`.
const CREDIT_PENDING_RETRY_AFTER_SECONDS: &str = "5";

#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    /// 402. The second field is the balance the caller holds, when the failing check knew it.
    PaymentRequired(String, Option<i32>),
    InternalError(String),
    RateLimitExceeded,
    Truncated,
    ContentBlocked,
    WalletUnavailable,
    /// 409 `credit_pending`: the purchase is accepted but not credited yet, and the same request (or the
    /// RevenueCat webhook) will credit it. Replaces the masked 500 the fast-track used to answer.
    CreditPending(String),
    /// 400 `already_processed`: the store transaction was credited before. Clients treat it as success.
    AlreadyProcessed(String),
    /// 400 `unverifiable_purchase`: the purchase could not be verified as it stands. A client may retry it for a
    /// while, because RevenueCat can lag the App Store.
    UnverifiablePurchase(String),
}

/// What an error answers with, before it becomes a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorParts {
    pub status: u16,
    pub error_type: &'static str,
    pub message: String,
    pub code: &'static str,
    pub balance: Option<i32>,
}

impl AppError {
    /// The status, type, message, stable code and, on a 402, the balance of this error.
    pub fn parts(&self) -> ErrorParts {
        let (status, error_type, message, code, balance) = match self {
            AppError::BadRequest(msg) => (400, "invalid_request_error", msg.clone(), "bad_request", None),
            AppError::Unauthorized(msg) => (401, "authentication_error", msg.clone(), "unauthorized", None),
            AppError::Forbidden(msg) => (403, "permission_denied", msg.clone(), "forbidden", None),
            AppError::NotFound(msg) => (404, "not_found", msg.clone(), "not_found", None),
            AppError::Conflict(msg) => (409, "conflict", msg.clone(), "conflict", None),
            AppError::PaymentRequired(msg, balance) => (402, "insufficient_credits", msg.clone(), "insufficient_credits", *balance),
            AppError::InternalError(_) => (500, "internal_error", "An internal error occurred. Please try again later.".to_string(), "internal_error", None),
            AppError::RateLimitExceeded => (429, "rate_limit_exceeded", "Rate limit exceeded. Please try again later.".to_string(), "rate_limit_exceeded", None),
            AppError::Truncated => (422, "generation_error", "The reply was cut off before it finished. Nothing was charged. Try again.".to_string(), "truncated", None),
            AppError::ContentBlocked => (422, "generation_error", "The provider declined to process this text. Nothing was charged.".to_string(), "content_blocked", None),
            AppError::WalletUnavailable => (503, "service_unavailable", "Your credit balance is temporarily unavailable. Please try again in a moment.".to_string(), "wallet_unavailable", None),
            AppError::CreditPending(msg) => (409, "conflict", msg.clone(), "credit_pending", None),
            AppError::AlreadyProcessed(msg) => (400, "invalid_request_error", msg.clone(), "already_processed", None),
            AppError::UnverifiablePurchase(msg) => (400, "invalid_request_error", msg.clone(), "unverifiable_purchase", None),
        };
        ErrorParts { status, error_type, message, code, balance }
    }

    /// The JSON envelope of this error: `{"error":{"message","type","param","code"}}` plus `balance` on a 402 that knows it.
    pub fn body(&self) -> ErrorResponse {
        let parts = self.parts();
        ErrorResponse {
            error: ErrorDetail {
                message: parts.message,
                error_type: parts.error_type.to_string(),
                param: None,
                code: Some(parts.code.to_string()),
                balance: parts.balance,
            },
        }
    }

    pub fn to_response(&self) -> Result<Response> {
        let mut resp = Response::from_json(&self.body())?.with_status(self.parts().status);
        if let AppError::RateLimitExceeded = self {
            resp.headers_mut().set("Retry-After", "60")?;
        }
        if let AppError::WalletUnavailable = self {
            resp.headers_mut().set("Retry-After", WALLET_RETRY_AFTER_SECONDS)?;
        }
        if let AppError::CreditPending(_) = self {
            resp.headers_mut().set("Retry-After", CREDIT_PENDING_RETRY_AFTER_SECONDS)?;
        }
        Ok(resp)
    }
}

/// True when a worker error carries `AppError::WalletUnavailable`, which every wallet failure is mapped to.
pub fn is_wallet_unavailable(error: &worker::Error) -> bool {
    error.to_string().starts_with("AppError::WalletUnavailable::")
}

/// Answers a wallet outage with its 503, and hands any other error back untouched, for handlers that let a ledger
/// failure propagate as they always did.
pub fn wallet_outage_response(error: worker::Error) -> Result<Response> {
    if is_wallet_unavailable(&error) {
        return AppError::WalletUnavailable.to_response();
    }
    Err(error)
}

impl From<worker::Error> for AppError {
    fn from(err: worker::Error) -> Self {
        let error_str = err.to_string();
        
        if let Some(msg) = error_str.strip_prefix("AppError::PaymentRequired::") {
            return AppError::PaymentRequired(msg.to_string(), None);
        }
        if let Some(rest) = error_str.strip_prefix("AppError::PaymentRequired#") {
            if let Some((balance, msg)) = rest.split_once("::") {
                if let Ok(balance) = balance.parse::<i32>() {
                    return AppError::PaymentRequired(msg.to_string(), Some(balance));
                }
            }
        }
        if let Some(msg) = error_str.strip_prefix("AppError::BadRequest::") {
            return AppError::BadRequest(msg.to_string());
        }
        if let Some(msg) = error_str.strip_prefix("AppError::Unauthorized::") {
            return AppError::Unauthorized(msg.to_string());
        }
        if let Some(msg) = error_str.strip_prefix("AppError::Forbidden::") {
            return AppError::Forbidden(msg.to_string());
        }
        if let Some(msg) = error_str.strip_prefix("AppError::NotFound::") {
            return AppError::NotFound(msg.to_string());
        }
        if let Some(msg) = error_str.strip_prefix("AppError::Conflict::") {
            return AppError::Conflict(msg.to_string());
        }
        if error_str.starts_with("AppError::RateLimitExceeded::") {
            return AppError::RateLimitExceeded;
        }
        if error_str.starts_with("AppError::Truncated::") {
            return AppError::Truncated;
        }
        if error_str.starts_with("AppError::ContentBlocked::") {
            return AppError::ContentBlocked;
        }
        if error_str.starts_with("AppError::WalletUnavailable::") {
            return AppError::WalletUnavailable;
        }
        if let Some(msg) = error_str.strip_prefix("AppError::CreditPending::") {
            return AppError::CreditPending(msg.to_string());
        }
        if let Some(msg) = error_str.strip_prefix("AppError::AlreadyProcessed::") {
            return AppError::AlreadyProcessed(msg.to_string());
        }
        if let Some(msg) = error_str.strip_prefix("AppError::UnverifiablePurchase::") {
            return AppError::UnverifiablePurchase(msg.to_string());
        }
        if let Some(msg) = error_str.strip_prefix("AppError::InternalError::") {
            return AppError::InternalError(msg.to_string());
        }
        
        AppError::InternalError(error_str)
    }
}

impl From<AppError> for worker::Error {
    fn from(err: AppError) -> Self {
        let encoded = match &err {
            AppError::BadRequest(msg) => format!("AppError::BadRequest::{}", msg),
            AppError::Unauthorized(msg) => format!("AppError::Unauthorized::{}", msg),
            AppError::Forbidden(msg) => format!("AppError::Forbidden::{}", msg),
            AppError::NotFound(msg) => format!("AppError::NotFound::{}", msg),
            AppError::Conflict(msg) => format!("AppError::Conflict::{}", msg),
            AppError::PaymentRequired(msg, None) => format!("AppError::PaymentRequired::{}", msg),
            AppError::PaymentRequired(msg, Some(balance)) => format!("AppError::PaymentRequired#{}::{}", balance, msg),
            AppError::CreditPending(msg) => format!("AppError::CreditPending::{}", msg),
            AppError::AlreadyProcessed(msg) => format!("AppError::AlreadyProcessed::{}", msg),
            AppError::UnverifiablePurchase(msg) => format!("AppError::UnverifiablePurchase::{}", msg),
            AppError::InternalError(msg) => format!("AppError::InternalError::{}", msg),
            AppError::RateLimitExceeded => "AppError::RateLimitExceeded::".to_string(),
            AppError::Truncated => "AppError::Truncated::".to_string(),
            AppError::ContentBlocked => "AppError::ContentBlocked::".to_string(),
            AppError::WalletUnavailable => "AppError::WalletUnavailable::".to_string(),
        };
        worker::Error::RustError(encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn json_of(error: &AppError) -> Value {
        serde_json::to_value(error.body()).unwrap()
    }

    fn round_trip(error: AppError) -> AppError {
        AppError::from(worker::Error::from(error))
    }

    #[test]
    fn the_credit_errors_have_stable_codes_and_statuses() {
        let cases = [
            (AppError::CreditPending("p".into()), 409, "credit_pending"),
            (AppError::AlreadyProcessed("a".into()), 400, "already_processed"),
            (AppError::UnverifiablePurchase("u".into()), 400, "unverifiable_purchase"),
            (AppError::PaymentRequired("i".into(), Some(5)), 402, "insufficient_credits"),
            (AppError::WalletUnavailable, 503, "wallet_unavailable"),
        ];
        for (error, status, code) in cases {
            let parts = error.parts();
            assert_eq!((parts.status, parts.code), (status, code));
            assert_eq!(json_of(&error)["error"]["code"], code);
        }
    }

    #[test]
    fn the_messages_old_clients_match_are_kept() {
        assert_eq!(AppError::AlreadyProcessed("Purchase has already been processed".into()).parts().message, "Purchase has already been processed");
        assert_eq!(
            AppError::UnverifiablePurchase("Purchase could not be verified with RevenueCat".into()).parts().message,
            "Purchase could not be verified with RevenueCat"
        );
        assert_eq!(AppError::AlreadyProcessed("x".into()).parts().error_type, "invalid_request_error");
        assert_eq!(AppError::UnverifiablePurchase("x".into()).parts().error_type, "invalid_request_error");
        assert_eq!(AppError::PaymentRequired("x".into(), None).parts().error_type, "insufficient_credits");
    }

    #[test]
    fn a_402_carries_the_balance_as_a_number_next_to_its_message() {
        let body = json_of(&AppError::PaymentRequired("Insufficient credits. Need 6 credits, have 5".into(), Some(5)));
        assert_eq!(body["error"]["balance"], json!(5));
        assert!(body["error"]["balance"].is_i64());
        assert_eq!(body["error"]["message"], "Insufficient credits. Need 6 credits, have 5");
        assert_eq!(body["error"]["code"], "insufficient_credits");
        assert_eq!(body["error"]["type"], "insufficient_credits");
    }

    #[test]
    fn a_zero_balance_is_still_a_balance() {
        assert_eq!(json_of(&AppError::PaymentRequired("x".into(), Some(0)))["error"]["balance"], json!(0));
    }

    #[test]
    fn a_402_that_does_not_know_the_balance_and_every_other_error_omit_the_field() {
        for error in [AppError::PaymentRequired("x".into(), None), AppError::CreditPending("x".into()), AppError::BadRequest("x".into()), AppError::WalletUnavailable] {
            assert!(json_of(&error)["error"].get("balance").is_none(), "{error:?}");
        }
    }

    #[test]
    fn the_credit_errors_survive_the_trip_through_a_worker_error() {
        assert!(matches!(round_trip(AppError::CreditPending("p".into())), AppError::CreditPending(m) if m == "p"));
        assert!(matches!(round_trip(AppError::AlreadyProcessed("a".into())), AppError::AlreadyProcessed(m) if m == "a"));
        assert!(matches!(round_trip(AppError::UnverifiablePurchase("u".into())), AppError::UnverifiablePurchase(m) if m == "u"));
    }

    #[test]
    fn the_balance_survives_the_trip_through_a_worker_error() {
        assert!(matches!(round_trip(AppError::PaymentRequired("no: still 4".into(), Some(4))), AppError::PaymentRequired(m, Some(4)) if m == "no: still 4"));
        assert!(matches!(round_trip(AppError::PaymentRequired("no".into(), None)), AppError::PaymentRequired(m, None) if m == "no"));
    }

    #[test]
    fn a_message_that_looks_like_a_balance_prefix_is_not_misread() {
        let looks = AppError::PaymentRequired("#7::sneaky".into(), None);
        assert!(matches!(round_trip(looks), AppError::PaymentRequired(m, None) if m == "#7::sneaky"));
    }
}
