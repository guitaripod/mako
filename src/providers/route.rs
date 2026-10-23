use worker::{D1Database, Env, Headers};

use crate::error::AppError;
use crate::handlers::chat::Provider;
use crate::tenant::TenantPolicy;

pub const GATEWAY_BASE_VAR: &str = "AI_GATEWAY_BASE";
pub const GATEWAY_TOKEN_SECRET: &str = "CF_AIG_TOKEN";

/// Tails that never go through the gateway: image generation and edits wait for a canary request, and the realtime
/// mint sits next to a call the phone makes straight to OpenAI.
const DIRECT_ONLY_PREFIXES: [&str; 2] = ["images/", "realtime/"];

/// Where one provider call goes and the headers the gateway needs on top of the provider's own key headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
}

impl Route {
    /// Sets the extra gateway headers; a direct route sets none.
    pub fn apply(&self, headers: &Headers) -> worker::Result<()> {
        for (name, value) in &self.headers {
            headers.set(name, value)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Gateway {
    base: String,
    token: String,
    collect_log: bool,
}

/// Whether provider calls go straight to the provider or through Cloudflare AI Gateway. Unsetting `AI_GATEWAY_BASE`
/// is the kill switch: every call is direct again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routing {
    gateway: Option<Gateway>,
}

impl Routing {
    pub fn direct() -> Self {
        Self { gateway: None }
    }

    /// An unset or blank base is direct. A base without a token is an error, never a silent direct call.
    pub fn from_parts(base: Option<&str>, token: Option<&str>, store_prompts: bool) -> Result<Self, AppError> {
        let base = match base.map(|b| b.trim().trim_end_matches('/')).filter(|b| !b.is_empty()) {
            Some(base) => base,
            None => return Ok(Self::direct()),
        };
        let token = token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| AppError::InternalError(format!("{} not configured", GATEWAY_TOKEN_SECRET)))?;
        Ok(Self { gateway: Some(Gateway { base: base.to_string(), token: token.to_string(), collect_log: store_prompts }) })
    }

    pub fn for_policy(env: &Env, policy: &TenantPolicy) -> Result<Self, AppError> {
        Self::from_parts(gateway_base(env).as_deref(), gateway_token(env).as_deref(), policy.store_prompts)
    }

    /// Reads the tenant's `store_prompts` only when the gateway is on. A tenant row that cannot be read counts as
    /// `store_prompts = 0`, so the gateway keeps no log rather than one it should not.
    pub async fn for_tenant(env: &Env, db: &D1Database, app_id: &str) -> Result<Self, AppError> {
        if gateway_base(env).is_none() {
            return Ok(Self::direct());
        }
        let store_prompts = TenantPolicy::load(db, app_id).await.map(|p| p.store_prompts).unwrap_or(false);
        Self::from_parts(gateway_base(env).as_deref(), gateway_token(env).as_deref(), store_prompts)
    }

    /// The one place provider URLs live. `tail` is the path after the provider's API root: `chat/completions` and
    /// `images/generations` for OpenAI, `messages` for Anthropic, `<model>:generateContent` for Gemini.
    pub fn route(&self, provider: &Provider, tail: &str) -> Route {
        match self.gateway.as_ref().filter(|_| through_gateway(provider, tail)) {
            Some(gateway) => gateway.route(provider, tail),
            None => Route { url: direct_url(provider, tail), headers: Vec::new() },
        }
    }
}

impl Gateway {
    fn route(&self, provider: &Provider, tail: &str) -> Route {
        let mut headers = vec![
            ("cf-aig-authorization", format!("Bearer {}", self.token)),
            ("cf-aig-collect-log-payload", "false".to_string()),
        ];
        if !self.collect_log {
            headers.push(("cf-aig-collect-log", "false".to_string()));
        }
        Route { url: format!("{}/{}{}", self.base, gateway_prefix(provider), tail), headers }
    }
}

pub fn direct_url(provider: &Provider, tail: &str) -> String {
    format!("{}{}", direct_prefix(provider), tail)
}

fn direct_prefix(provider: &Provider) -> &'static str {
    match provider {
        Provider::OpenAI => "https://api.openai.com/v1/",
        Provider::Anthropic => "https://api.anthropic.com/v1/",
        Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta/models/",
    }
}

fn gateway_prefix(provider: &Provider) -> &'static str {
    match provider {
        Provider::OpenAI => "openai/",
        Provider::Anthropic => "anthropic/v1/",
        Provider::Gemini => "google-ai-studio/v1beta/models/",
    }
}

fn through_gateway(provider: &Provider, tail: &str) -> bool {
    !matches!(provider, Provider::OpenAI) || !DIRECT_ONLY_PREFIXES.iter().any(|prefix| tail.starts_with(prefix))
}

fn gateway_base(env: &Env) -> Option<String> {
    env.var(GATEWAY_BASE_VAR)
        .ok()
        .map(|v| v.to_string())
        .filter(|v| !v.trim().is_empty())
}

fn gateway_token(env: &Env) -> Option<String> {
    env.secret(GATEWAY_TOKEN_SECRET).ok().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://gateway.ai.cloudflare.com/v1/acct/mako";

    fn on(store_prompts: bool) -> Routing {
        Routing::from_parts(Some(BASE), Some("tok"), store_prompts).unwrap()
    }

    fn header<'a>(route: &'a Route, name: &str) -> Option<&'a str> {
        route.headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn unset_or_blank_base_routes_every_call_direct_with_no_extra_headers() {
        for base in [None, Some(""), Some("   ")] {
            let routing = Routing::from_parts(base, Some("tok"), true).unwrap();
            assert_eq!(routing, Routing::direct());
            let table = [
                (Provider::OpenAI, "chat/completions", "https://api.openai.com/v1/chat/completions"),
                (Provider::OpenAI, "images/generations", "https://api.openai.com/v1/images/generations"),
                (Provider::OpenAI, "images/edits", "https://api.openai.com/v1/images/edits"),
                (Provider::OpenAI, "realtime/translations/client_secrets", "https://api.openai.com/v1/realtime/translations/client_secrets"),
                (Provider::Anthropic, "messages", "https://api.anthropic.com/v1/messages"),
                (Provider::Gemini, "gemini-3-flash-preview:generateContent", "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-flash-preview:generateContent"),
            ];
            for (provider, tail, url) in table {
                let route = routing.route(&provider, tail);
                assert_eq!(route.url, url);
                assert!(route.headers.is_empty());
            }
        }
    }

    #[test]
    fn a_base_without_a_token_is_an_error_not_a_direct_call() {
        for token in [None, Some(""), Some("  ")] {
            assert!(Routing::from_parts(Some(BASE), token, true).is_err());
        }
    }

    #[test]
    fn a_token_without_a_base_stays_direct() {
        assert_eq!(Routing::from_parts(None, Some("tok"), false).unwrap(), Routing::direct());
    }

    #[test]
    fn chat_and_the_other_providers_go_through_the_gateway_with_its_headers() {
        let routing = on(true);
        let table = [
            (Provider::OpenAI, "chat/completions", format!("{BASE}/openai/chat/completions")),
            (Provider::Anthropic, "messages", format!("{BASE}/anthropic/v1/messages")),
            (
                Provider::Gemini,
                "gemini-3.1-flash-image:generateContent",
                format!("{BASE}/google-ai-studio/v1beta/models/gemini-3.1-flash-image:generateContent"),
            ),
        ];
        for (provider, tail, url) in table {
            let route = routing.route(&provider, tail);
            assert_eq!(route.url, url);
            assert_eq!(header(&route, "cf-aig-authorization"), Some("Bearer tok"));
            assert_eq!(header(&route, "cf-aig-collect-log-payload"), Some("false"));
            assert_eq!(header(&route, "cf-aig-collect-log"), None);
            assert_eq!(route.headers.len(), 2);
        }
    }

    #[test]
    fn a_tenant_that_stores_no_prompts_also_turns_the_whole_log_off() {
        let routing = on(false);
        for (provider, tail) in [
            (Provider::OpenAI, "chat/completions"),
            (Provider::Anthropic, "messages"),
            (Provider::Gemini, "gemini-3.1-flash-image:generateContent"),
        ] {
            let route = routing.route(&provider, tail);
            assert_eq!(header(&route, "cf-aig-collect-log"), Some("false"));
            assert_eq!(header(&route, "cf-aig-collect-log-payload"), Some("false"));
            assert_eq!(header(&route, "cf-aig-authorization"), Some("Bearer tok"));
            assert_eq!(route.headers.len(), 3);
        }
    }

    #[test]
    fn openai_images_and_the_realtime_mint_stay_direct_with_the_gateway_on() {
        for routing in [on(true), on(false)] {
            for tail in ["images/generations", "images/edits", "realtime/translations/client_secrets", "realtime/translations/calls"] {
                let route = routing.route(&Provider::OpenAI, tail);
                assert_eq!(route.url, format!("https://api.openai.com/v1/{tail}"));
                assert!(route.headers.is_empty());
            }
        }
    }

    #[test]
    fn a_trailing_slash_or_padding_on_the_base_does_not_double_the_separator() {
        let routing = Routing::from_parts(Some(&format!(" {BASE}/ ")), Some(" tok "), true).unwrap();
        let route = routing.route(&Provider::Anthropic, "messages");
        assert_eq!(route.url, format!("{BASE}/anthropic/v1/messages"));
        assert_eq!(header(&route, "cf-aig-authorization"), Some("Bearer tok"));
    }

    #[test]
    fn the_direct_url_of_the_realtime_call_the_phone_makes_is_the_provider_url() {
        assert_eq!(
            direct_url(&Provider::OpenAI, "realtime/translations/calls"),
            "https://api.openai.com/v1/realtime/translations/calls"
        );
    }
}
