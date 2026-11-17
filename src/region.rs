use worker::{Request, Response, Result};

use crate::error::AppError;

/// Countries where no model call is made for anyone. China mainland: Anthropic, Google and OpenAI do not serve it, and
/// China's generative AI rules need CAC filings Midgar cannot hold, so Apple's China mainland declarations can only
/// answer No when model features are geoblocked there. Kept in step with `services/lib/src/region.ts`.
const MODEL_UNSERVED_COUNTRIES: [&str; 1] = ["CN"];

/// The header the gateway sets on every internal hop to the country Cloudflare resolved for the caller.
const COUNTRY_HEADER: &str = "X-Midgar-Country";

/// The routes that call a model provider or mint a session with one.
const MODEL_ROUTES: [&str; 4] = [
    "/v1/images/generations",
    "/v1/images/edits",
    "/v1/run/chat.completion",
    "/v1/run/realtime.translate/start",
];

/// A two-letter country code, upper-cased, or none when the value is not one.
fn country_code(value: &str) -> Option<String> {
    let code = value.trim().to_ascii_uppercase();
    (code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase())).then_some(code)
}

/// Whether a request must be refused: a model route, and Cloudflare's country for the connection or the gateway's
/// header naming an unserved country. A header can only refuse its own sender, never lift Cloudflare's country, and a
/// caller with no known country is served.
fn refuses(path: &str, connection: Option<&str>, forwarded: Option<&str>) -> bool {
    MODEL_ROUTES.contains(&path)
        && [connection, forwarded]
            .into_iter()
            .flatten()
            .filter_map(country_code)
            .any(|code| MODEL_UNSERVED_COUNTRIES.contains(&code.as_str()))
}

/// The 451 a model route answers from a country where no model call is made, before any credit is held or a provider
/// is called; none for every other request.
pub fn refusal(req: &Request) -> Option<Result<Response>> {
    let connection = req.cf().and_then(|cf| cf.country());
    let forwarded = req.headers().get(COUNTRY_HEADER).ok().flatten();
    refuses(&req.path(), connection.as_deref(), forwarded.as_deref()).then(|| AppError::UnavailableInRegion.to_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_every_model_route_from_china_mainland_by_either_country() {
        for path in MODEL_ROUTES {
            assert!(refuses(path, Some("CN"), None), "{path}");
            assert!(refuses(path, None, Some("CN")), "{path}");
            assert!(refuses(path, Some("CN"), Some("US")), "{path}");
            assert!(refuses(path, None, Some(" cn ")), "{path}");
        }
    }

    #[test]
    fn serves_other_countries_and_unknown_callers() {
        for (connection, forwarded) in [(Some("FI"), None), (None, Some("HK")), (Some("TW"), Some("TW")), (None, None), (None, Some("china")), (Some(""), None)] {
            assert!(!refuses("/v1/run/chat.completion", connection, forwarded), "{connection:?} {forwarded:?}");
        }
    }

    #[test]
    fn leaves_routes_without_a_model_call_open_in_china_mainland() {
        for path in ["/v1/credits/balance", "/v1/identity/anonymous", "/v1/credits/purchase/validate", "/r2/u/i", "/privacy/pixie", "/v1/run/realtime.translate/settle", "/v1/images"] {
            assert!(!refuses(path, Some("CN"), Some("CN")), "{path}");
        }
    }

    #[test]
    fn answers_451_unavailable_in_region_through_a_worker_error_too() {
        let parts = AppError::UnavailableInRegion.parts();
        assert_eq!((parts.status, parts.code), (451, "unavailable_in_region"));
        let round_trip = AppError::from(worker::Error::from(AppError::UnavailableInRegion));
        assert!(matches!(round_trip, AppError::UnavailableInRegion));
    }
}
