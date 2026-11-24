//! How the Companion reads an AI provider's failure: wait and retry, park the provider until a
//! human acts, or give up on the one request. Mirrors `AiChatClient` (402 and an exhausted 429
//! park the provider; `isAccountExhausted429`). Proven against `companion/vectors/ai-rules.json`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Busy or timed out: ask again.
    Retry,
    /// Out of credit: stand the provider down for hours.
    Park,
    /// The key is refused (401/403): park until the key changes.
    AuthPark,
    /// The request itself is wrong; asking again will not help.
    Failed,
}

/// Same codes as `AiChatClient.EXHAUSTED_429_CODES` (Zhipu).
const EXHAUSTED_429_CODES: [&str; 2] = ["1113", "1304"];
/// Same phrases as `AiChatClient.EXHAUSTED_429_PHRASES`, matched case-insensitively.
const EXHAUSTED_429_PHRASES: [&str; 5] = ["credits are depleted", "insufficient balance", "quota exceeded", "billing", "daily free allocation"];

fn is_account_exhausted_429(body: &str) -> bool {
    let lower = body.to_lowercase();
    EXHAUSTED_429_CODES.iter().any(|c| body.contains(c)) || EXHAUSTED_429_PHRASES.iter().any(|p| lower.contains(p))
}

pub fn classify(http_code: Option<u16>, body: &str, timed_out: bool) -> FailureClass {
    if timed_out {
        return FailureClass::Retry;
    }
    match http_code {
        Some(402) => FailureClass::Park,
        Some(429) if is_account_exhausted_429(body) => FailureClass::Park,
        Some(429) | Some(408) => FailureClass::Retry,
        Some(c) if (500..600).contains(&c) => FailureClass::Retry,
        Some(401) | Some(403) => FailureClass::AuthPark,
        _ => FailureClass::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_classes_match_vectors() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../companion/vectors/ai-rules.json");
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
        let v: serde_json::Value = serde_json::from_str(&text).expect("ai-rules.json is not JSON");
        let cases = v["failures"].as_array().expect("failure cases");
        assert!(cases.len() >= 15);
        for c in cases {
            let code = c["httpCode"].as_u64().map(|n| n as u16);
            let body = c["body"].as_str().unwrap();
            let want = match c["class"].as_str().unwrap() {
                "Retry" => FailureClass::Retry,
                "Park" => FailureClass::Park,
                "AuthPark" => FailureClass::AuthPark,
                "Failed" => FailureClass::Failed,
                other => panic!("unknown class {other}"),
            };
            assert_eq!(classify(code, body, c["timedOut"].as_bool().unwrap()), want, "case {c}");
        }
    }
}
