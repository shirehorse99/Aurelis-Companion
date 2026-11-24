//! Pairing: SPAKE2 (Ed25519 group, symmetric) over the 8-character code shown on the Companion.
//! Both sides derive the same 32-byte K only if they typed/showed the same code.

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

const IDENTITY: &[u8] = b"aurelis-companion-v1";
/// Crockford base32: no I, L, O, U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Confirm labels, shared by the Companion server and the television so they cannot drift.
pub const TV_CONFIRM: &str = "tv-confirm";
pub const COMPANION_CONFIRM: &str = "companion-confirm";

/// Loose typing made equal: trim, drop spaces and hyphens, uppercase, O->0, I/L->1.
pub fn normalise_code(code: &str) -> String {
    code.trim()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            u => u,
        })
        .collect()
}

pub struct Pairing {
    state: Spake2<Ed25519Group>,
}

impl Pairing {
    /// Begin a pairing; returns the state and the message to send to the peer.
    pub fn start(code: &str) -> (Pairing, Vec<u8>) {
        let (state, msg) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(normalise_code(code).as_bytes()),
            &Identity::new(IDENTITY),
        );
        (Pairing { state }, msg)
    }

    /// Complete with the peer's message. None when the message is malformed.
    pub fn finish(self, peer_msg: &[u8]) -> Option<[u8; 32]> {
        let k = self.state.finish(peer_msg).ok()?;
        k.try_into().ok()
    }
}

/// HMAC-SHA256(K, label): "tv-confirm" from the television, "companion-confirm" from the Companion.
pub fn confirm(k: &[u8; 32], label: &str) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(k).expect("HMAC accepts any key length");
    m.update(label.as_bytes());
    m.finalize().into_bytes().into()
}

/// A fresh 8-character pairing code. Rejection-free: 256 is a multiple of 32.
pub fn new_code<R: RngCore>(rng: &mut R) -> String {
    let mut b = [0u8; 8];
    rng.fill_bytes(&mut b);
    b.iter().map(|x| ALPHABET[(*x % 32) as usize] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(a_code: &str, b_code: &str) -> (Option<[u8; 32]>, Option<[u8; 32]>) {
        let (a, am) = Pairing::start(a_code);
        let (b, bm) = Pairing::start(b_code);
        (a.finish(&bm), b.finish(&am))
    }

    #[test]
    fn same_code_derives_same_key() {
        let (ka, kb) = run("7KQ2M9XA", "7KQ2M9XA");
        assert!(ka.is_some());
        assert_eq!(ka, kb);
    }

    #[test]
    fn different_code_derives_different_keys() {
        let (ka, kb) = run("7KQ2M9XA", "7KQ2M9XB");
        assert_ne!(ka, kb);
    }

    #[test]
    fn malformed_peer_message_is_none() {
        let (a, _) = Pairing::start("7KQ2M9XA");
        assert!(a.finish(&[1, 2, 3]).is_none());
    }

    #[test]
    fn loose_typing_derives_the_same_key() {
        let (ka, kb) = run("7kq2-m9xa", "7KQ2M9XA");
        assert!(ka.is_some());
        assert_eq!(ka, kb);
        let (ka, kb) = run("7KQ2M9XO", "7KQ2M9X0");
        assert_eq!(ka, kb);
        let (ka, kb) = run(" 7KQ2 M9X1 ", "7KQ2M9XL");
        assert_eq!(ka, kb);
        assert_eq!(normalise_code("  ab-il o "), "AB110");
    }

    #[test]
    fn confirm_constants_are_the_wire_labels() {
        assert_eq!(TV_CONFIRM, "tv-confirm");
        assert_eq!(COMPANION_CONFIRM, "companion-confirm");
    }

    #[test]
    fn confirm_labels_differ() {
        let k = [7u8; 32];
        assert_ne!(confirm(&k, "tv-confirm"), confirm(&k, "companion-confirm"));
        assert_eq!(confirm(&k, "tv-confirm"), confirm(&k, "tv-confirm"));
    }

    #[test]
    fn new_code_is_8_crockford_chars() {
        let mut rng = rand::thread_rng();
        for _ in 0..200 {
            let c = new_code(&mut rng);
            assert_eq!(c.len(), 8);
            assert!(c.bytes().all(|b| ALPHABET.contains(&b)), "{c}");
            assert!(!c.contains(['I', 'L', 'O', 'U']));
        }
    }
}
