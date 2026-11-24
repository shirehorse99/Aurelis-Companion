//! The GLM bearer token, byte for byte what `GlmJwtAuth.generateToken` makes on the television.
//! Proven against `companion/vectors/ai-rules.json`, whose tokens Kotlin generated.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// A short-lived HS256 JWT for an `id.secret` key. A key that is not `id.secret` (no dot, or
/// either half blank) is returned unchanged, as Kotlin does. The key is never logged.
pub fn glm_token(key: &str, now_ms: u64) -> String {
    let Some((id, secret)) = key.split_once('.') else {
        return key.to_string();
    };
    if id.trim().is_empty() || secret.trim().is_empty() {
        return key.to_string();
    }
    let exp = now_ms + 3_600_000;
    let header = r#"{"alg":"HS256","sign_type":"SIGN"}"#;
    let payload = format!(r#"{{"api_key":"{id}","exp":{exp},"timestamp":{now_ms}}}"#);
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.as_bytes()),
        URL_SAFE_NO_PAD.encode(payload.as_bytes())
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(signing_input.as_bytes());
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glm_token_matches_kotlin_vectors() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../companion/vectors/ai-rules.json");
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
        let v: serde_json::Value = serde_json::from_str(&text).expect("ai-rules.json is not JSON");
        let cases = v["glm"].as_array().expect("glm cases");
        assert!(!cases.is_empty());
        for c in cases {
            let key = c["key"].as_str().unwrap();
            let now = c["nowMs"].as_u64().unwrap();
            assert_eq!(glm_token(key, now), c["token"].as_str().unwrap(), "key {key} at {now}");
        }
    }
}
