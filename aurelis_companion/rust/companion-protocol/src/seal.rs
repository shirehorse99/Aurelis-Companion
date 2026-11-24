//! `HouseholdCipher` v3: `[0x03][salt 16][iv 12][ciphertext + 16-byte tag]`.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;

pub const VERSION: u8 = 0x03;
pub const SALT_BYTES: usize = 16;
pub const IV_BYTES: usize = 12;
pub const ITERATIONS: u32 = 10_000;

/// PBKDF2-HMAC-SHA256 over the secret's lowercase hex rendering (64 ASCII bytes),
/// exactly as `HouseholdCipher.deriveKey` feeds `PBEKeySpec`.
pub(crate) fn derive_key(secret: &[u8; 32], salt: &[u8]) -> [u8; 32] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hex = [0u8; 64];
    for (i, b) in secret.iter().enumerate() {
        hex[i * 2] = HEX[(b >> 4) as usize];
        hex[i * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(&hex, salt, ITERATIONS, &mut key);
    key
}

/// Seal `plain` under `secret` with a fresh salt and IV.
pub fn seal(plain: &[u8], secret: &[u8; 32]) -> Vec<u8> {
    let mut salt = [0u8; SALT_BYTES];
    let mut iv = [0u8; IV_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    rand::rngs::OsRng.fill_bytes(&mut iv);
    seal_with(plain, secret, &salt, &iv)
}

/// Deterministic variant for tests.
pub(crate) fn seal_with(plain: &[u8], secret: &[u8; 32], salt: &[u8; SALT_BYTES], iv: &[u8; IV_BYTES]) -> Vec<u8> {
    let key = derive_key(secret, salt);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let body = cipher
        .encrypt(Nonce::from_slice(iv), plain)
        .expect("AES-GCM encryption cannot fail for in-memory input");
    let mut out = Vec::with_capacity(1 + SALT_BYTES + IV_BYTES + body.len());
    out.push(VERSION);
    out.extend_from_slice(salt);
    out.extend_from_slice(iv);
    out.extend_from_slice(&body);
    out
}

/// The plaintext, or `None` for any failure (wrong secret, truncation, tampering, version).
pub fn open(sealed: &[u8], secret: &[u8; 32]) -> Option<Vec<u8>> {
    if sealed.len() <= 1 + SALT_BYTES + IV_BYTES || sealed[0] != VERSION {
        return None;
    }
    let salt = &sealed[1..1 + SALT_BYTES];
    let iv = &sealed[1 + SALT_BYTES..1 + SALT_BYTES + IV_BYTES];
    let body = &sealed[1 + SALT_BYTES + IV_BYTES..];
    let key = derive_key(secret, salt);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    cipher.decrypt(Nonce::from_slice(iv), body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [7u8; 32];

    #[test]
    fn seal_round_trips() {
        let sealed = seal(b"hello household", &SECRET);
        assert_eq!(sealed[0], VERSION);
        assert_eq!(open(&sealed, &SECRET).as_deref(), Some(&b"hello household"[..]));
        assert_eq!(open(&seal(b"", &SECRET), &SECRET), Some(Vec::new()));
    }

    #[test]
    fn seal_with_is_deterministic() {
        let a = seal_with(b"x", &SECRET, &[1; 16], &[2; 12]);
        let b = seal_with(b"x", &SECRET, &[1; 16], &[2; 12]);
        assert_eq!(a, b);
    }

    #[test]
    fn open_rejects_wrong_secret_truncation_and_version() {
        let sealed = seal(b"payload", &SECRET);
        assert_eq!(open(&sealed, &[8u8; 32]), None);
        assert_eq!(open(&sealed[..sealed.len() - 1], &SECRET), None);
        assert_eq!(open(&sealed[..20], &SECRET), None);
        let mut v = sealed.clone();
        v[0] = 0x04;
        assert_eq!(open(&v, &SECRET), None);
        let mut t = sealed.clone();
        let last = t.len() - 1;
        t[last] ^= 1;
        assert_eq!(open(&t, &SECRET), None);
    }

    #[test]
    fn seal_opens_kotlin_vector() {
        let v = crate::vectors::load();
        let secret = crate::vectors::secret(&v);
        let plain = crate::vectors::b64(&v, "plainB64");
        let sealed = crate::vectors::b64(&v, "sealedV3B64");
        assert_eq!(open(&sealed, &secret), Some(plain));
    }
}
