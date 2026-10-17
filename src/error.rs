use worker::{Response, Result};
use crate::models::{ErrorResponse, ErrorDetail};

/// Seconds a client is told to wait after a 503 `wallet_unavailable`.
const WALLET_RETRY_AFTER_SECONDS: &str = "5";

#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    PaymentRequired(String),
    InternalError(String),
    RateLimitExceeded,
    Truncated,
    ContentBlocked,
    WalletUnavailable,
}

impl AppError {
    pub fn to_response(&self) -> Result<Response> {
        let (status, error_type, message, code) = match self {
            AppError::BadRequest(msg) => (400, "invalid_request_error", msg.clone(), "bad_request"),
            AppError::Unauthorized(msg) => (401, "authentication_error", msg.clone(), "unauthorized"),
            AppError::Forbidden(msg) => (403, "permission_denied", msg.clone(), "forbidden"),
            AppError::NotFound(msg) => (404, "not_found", msg.clone(), "not_found"),
            AppError::Conflict(msg) => (409, "conflict", msg.clone(), "conflict"),
            AppError::PaymentRequired(msg) => (402, "insufficient_credits", msg.clone(), "insufficient_credits"),
            AppError::InternalError(_) => (500, "internal_error", "An internal error occurred. Please try again later.".to_string(), "internal_error"),
            AppError::RateLimitExceeded => (429, "rate_limit_exceeded", "Rate limit exceeded. Please try again later.".to_string(), "rate_limit_exceeded"),
            AppError::Truncated => (422, "generation_error", "The reply was cut off before it finished. Nothing was charged. Try again.".to_string(), "truncated"),
            AppError::ContentBlocked => (422, "generation_error", "The provider declined to process this text. Nothing was charged.".to_string(), "content_blocked"),
            AppError::WalletUnavailable => (503, "service_unavailable", "Your credit balance is temporarily unavailable. Please try again in a moment.".to_string(), "wallet_unavailable"),
        };

        let error_response = ErrorResponse {
            error: ErrorDetail {
                message,
                error_type: error_type.to_string(),
                param: None,
                code: Some(code.to_string()),
            },
        };

        let mut resp = Response::from_json(&error_response)?.with_status(status);
        if let AppError::RateLimitExceeded = self {
            resp.headers_mut().set("Retry-After", "60")?;
        }
        if let AppError::WalletUnavailable = self {
            resp.headers_mut().set("Retry-After", WALLET_RETRY_AFTER_SECONDS)?;
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
            return AppError::PaymentRequired(msg.to_string());
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
            AppError::PaymentRequired(msg) => format!("AppError::PaymentRequired::{}", msg),
            AppError::InternalError(msg) => format!("AppError::InternalError::{}", msg),
            AppError::RateLimitExceeded => "AppError::RateLimitExceeded::".to_string(),
            AppError::Truncated => "AppError::Truncated::".to_string(),
            AppError::ContentBlocked => "AppError::ContentBlocked::".to_string(),
            AppError::WalletUnavailable => "AppError::WalletUnavailable::".to_string(),
        };
        worker::Error::RustError(encoded)
    }
}