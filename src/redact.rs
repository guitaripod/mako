use serde_json::Value;

const MAX_CODE_LEN: usize = 48;
const MARKERS: [&str; 2] = ["content_policy_violation", "moderation"];

/// Reduces an upstream provider error body to its HTTP status and a short
/// machine code. Provider bodies can echo the prompt the user sent, so nothing
/// else from them is ever logged or stored. The two refusal markers the image
/// handlers key on are kept when the body mentions them.
pub fn provider_error_summary(status: u16, body: &str) -> String {
    let mut summary = format!("status {} code {}", status, error_code(body));
    for marker in MARKERS {
        if body.contains(marker) && !summary.contains(marker) {
            summary.push(' ');
            summary.push_str(marker);
        }
    }
    summary
}

fn error_code(body: &str) -> String {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = parsed.get("error").unwrap_or(&parsed);
    let candidates = [
        error.get("code"),
        error.get("status"),
        error.get("type"),
        parsed.get("type"),
    ];
    let code = candidates.into_iter().flatten().find_map(code_token);
    code.unwrap_or_else(|| "unknown".to_string())
}

fn code_token(value: &Value) -> Option<String> {
    let raw = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let token: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(MAX_CODE_LEN)
        .collect();
    if token.is_empty() || token == "error" {
        None
    } else {
        Some(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_status_and_code_but_drops_the_message() {
        let body = r#"{"error":{"message":"Invalid prompt: I dreamed my mother was drowning","code":"invalid_value","type":"invalid_request_error"}}"#;
        let summary = provider_error_summary(400, body);
        assert_eq!(summary, "status 400 code invalid_value");
        assert!(!summary.contains("dreamed"));
    }

    #[test]
    fn gemini_status_string_is_the_code() {
        let body = r#"{"error":{"code":429,"message":"quota for my secret dream","status":"RESOURCE_EXHAUSTED"}}"#;
        assert_eq!(provider_error_summary(429, body), "status 429 code 429");
    }

    #[test]
    fn anthropic_error_type_is_the_code() {
        let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"user text here"}}"#;
        assert_eq!(provider_error_summary(529, body), "status 529 code overloaded_error");
    }

    #[test]
    fn unparseable_bodies_collapse_to_unknown() {
        let summary = provider_error_summary(502, "<html>my private prompt</html>");
        assert_eq!(summary, "status 502 code unknown");
    }

    #[test]
    fn refusal_markers_survive_for_the_image_handlers() {
        let body = r#"{"error":{"message":"blocked by moderation: some prompt text","code":"bad"}}"#;
        let summary = provider_error_summary(400, body);
        assert!(summary.contains("moderation"));
        assert!(!summary.contains("some prompt text"));
        let policy = r#"{"error":{"code":"content_policy_violation","message":"x"}}"#;
        assert!(provider_error_summary(400, policy).contains("content_policy_violation"));
    }

    #[test]
    fn codes_are_stripped_of_odd_characters_and_bounded() {
        let body = format!(r#"{{"error":{{"code":"{}"}}}}"#, "a b!c".repeat(40));
        let summary = provider_error_summary(500, &body);
        assert_eq!(summary, format!("status 500 code {}", "abc".repeat(16)));
    }
}
