//! Request proof: HMAC-SHA256(secret, nonce || METHOD || 0x00 || path-with-query || 0x00 || SHA-256(body)).

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

fn mac(secret: &[u8; 32], nonce: &[u8], method: &str, path: &str, body: &[u8]) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    m.update(nonce);
    m.update(method.as_bytes());
    m.update(&[0]);
    m.update(path.as_bytes());
    m.update(&[0]);
    m.update(&Sha256::digest(body));
    m
}

pub fn proof(secret: &[u8; 32], nonce: &[u8], method: &str, path: &str, body: &[u8]) -> [u8; 32] {
    mac(secret, nonce, method, path, body).finalize().into_bytes().into()
}

/// Constant-time check of an offered proof.
pub fn verify(secret: &[u8; 32], nonce: &[u8], method: &str, path: &str, body: &[u8], offered: &[u8]) -> bool {
    mac(secret, nonce, method, path, body).verify_slice(offered).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_accepts_only_the_exact_request() {
        let s = [3u8; 32];
        let p = proof(&s, b"nonce", "POST", "/v1/ai/get?x=1", b"body");
        assert!(verify(&s, b"nonce", "POST", "/v1/ai/get?x=1", b"body", &p));
        assert!(!verify(&s, b"nonce2", "POST", "/v1/ai/get?x=1", b"body", &p));
        assert!(!verify(&s, b"nonce", "GET", "/v1/ai/get?x=1", b"body", &p));
        assert!(!verify(&s, b"nonce", "POST", "/v1/ai/get", b"body", &p));
        assert!(!verify(&s, b"nonce", "POST", "/v1/ai/get?x=1", b"bodz", &p));
        assert!(!verify(&[4u8; 32], b"nonce", "POST", "/v1/ai/get?x=1", b"body", &p));
        assert!(!verify(&s, b"nonce", "POST", "/v1/ai/get?x=1", b"body", &p[..31]));
    }

    #[test]
    fn proof_matches_vector() {
        let v = crate::vectors::load();
        let secret = crate::vectors::secret(&v);
        let nonce = crate::vectors::b64(&v, "nonceB64");
        let body = crate::vectors::b64(&v, "bodyB64");
        let want = crate::vectors::b64(&v, "proofB64");
        let method = v["method"].as_str().unwrap();
        let path = v["path"].as_str().unwrap();
        assert_eq!(proof(&secret, &nonce, method, path, &body).to_vec(), want);
    }
}
