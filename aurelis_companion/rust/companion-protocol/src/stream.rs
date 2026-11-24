//! v4 framed stream: header `[0x04][salt 16][prefix 8]`, frames `[u32 BE len][ct+tag]`,
//! IV = prefix || u32 BE index, AAD = u32 BE index || flag (0x01 last, 0x00 otherwise).

use crate::seal::{derive_key, SALT_BYTES};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;

pub const VERSION: u8 = 0x04;
pub const PREFIX_BYTES: usize = 8;
pub const HEADER_BYTES: usize = 1 + SALT_BYTES + PREFIX_BYTES;
pub const MAX_FRAME_PLAINTEXT: usize = 65_536;
const TAG_BYTES: usize = 16;

fn nonce_and_aad(prefix: &[u8; PREFIX_BYTES], index: u32, last: bool) -> ([u8; 12], [u8; 5]) {
    let mut iv = [0u8; 12];
    iv[..PREFIX_BYTES].copy_from_slice(prefix);
    iv[PREFIX_BYTES..].copy_from_slice(&index.to_be_bytes());
    let mut aad = [0u8; 5];
    aad[..4].copy_from_slice(&index.to_be_bytes());
    aad[4] = last as u8;
    (iv, aad)
}

pub struct StreamSealer {
    cipher: Aes256Gcm,
    prefix: [u8; PREFIX_BYTES],
    index: u32,
    finished: bool,
}

impl StreamSealer {
    /// A sealer and the header to send before the first frame.
    pub fn new(secret: &[u8; 32]) -> (Self, Vec<u8>) {
        let mut salt = [0u8; SALT_BYTES];
        let mut prefix = [0u8; PREFIX_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        rand::rngs::OsRng.fill_bytes(&mut prefix);
        Self::new_with(secret, &salt, &prefix)
    }

    pub(crate) fn new_with(secret: &[u8; 32], salt: &[u8; SALT_BYTES], prefix: &[u8; PREFIX_BYTES]) -> (Self, Vec<u8>) {
        let key = derive_key(secret, salt);
        let mut header = Vec::with_capacity(HEADER_BYTES);
        header.push(VERSION);
        header.extend_from_slice(salt);
        header.extend_from_slice(prefix);
        let s = StreamSealer {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
            prefix: *prefix,
            index: 0,
            finished: false,
        };
        (s, header)
    }

    /// Seal one frame. `chunk` must be at most 65 536 bytes; nothing may follow a `last` frame.
    pub fn frame(&mut self, chunk: &[u8], last: bool) -> Vec<u8> {
        assert!(chunk.len() <= MAX_FRAME_PLAINTEXT, "frame plaintext over 65536 bytes");
        assert!(!self.finished, "frame after the last frame");
        let (iv, aad) = nonce_and_aad(&self.prefix, self.index, last);
        let ct = self
            .cipher
            .encrypt(Nonce::from_slice(&iv), Payload { msg: chunk, aad: &aad })
            .expect("AES-GCM encryption cannot fail for in-memory input");
        self.index = self.index.checked_add(1).expect("stream frame index overflow");
        self.finished = last;
        let mut out = Vec::with_capacity(4 + ct.len());
        out.extend_from_slice(&(ct.len() as u32).to_be_bytes());
        out.extend_from_slice(&ct);
        out
    }
}

pub struct StreamOpener {
    cipher: Aes256Gcm,
    prefix: [u8; PREFIX_BYTES],
    index: u32,
    finished: bool,
}

impl StreamOpener {
    pub fn new(secret: &[u8; 32], header: &[u8]) -> Option<Self> {
        if header.len() != HEADER_BYTES || header[0] != VERSION {
            return None;
        }
        let salt = &header[1..1 + SALT_BYTES];
        let mut prefix = [0u8; PREFIX_BYTES];
        prefix.copy_from_slice(&header[1 + SALT_BYTES..]);
        let key = derive_key(secret, salt);
        Some(StreamOpener {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
            prefix,
            index: 0,
            finished: false,
        })
    }

    /// Open the next frame; `last` is what the reader believes (it ends the stream). `None` for a
    /// bad length, wrong position, wrong flag, tampering, or any frame after the last.
    /// A stream whose reader runs out of bytes before a `last` frame opened is truncated:
    /// check [`StreamOpener::is_finished`].
    pub fn open_frame(&mut self, frame: &[u8], last: bool) -> Option<Vec<u8>> {
        if self.finished || frame.len() < 4 + TAG_BYTES {
            return None;
        }
        let len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        if len != frame.len() - 4 || len > MAX_FRAME_PLAINTEXT + TAG_BYTES {
            return None;
        }
        let (iv, aad) = nonce_and_aad(&self.prefix, self.index, last);
        let plain = self
            .cipher
            .decrypt(Nonce::from_slice(&iv), Payload { msg: &frame[4..], aad: &aad })
            .ok()?;
        self.index += 1;
        self.finished = last;
        Some(plain)
    }

    /// True once the last frame has opened; false at end of input means truncation.
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [9u8; 32];

    fn three_frames() -> (Vec<u8>, Vec<Vec<u8>>) {
        let (mut s, header) = StreamSealer::new(&SECRET);
        let f = vec![s.frame(b"one", false), s.frame(b"two", false), s.frame(b"three", true)];
        (header, f)
    }

    #[test]
    fn stream_round_trips_three_frames() {
        let (header, f) = three_frames();
        assert_eq!(header.len(), HEADER_BYTES);
        let mut o = StreamOpener::new(&SECRET, &header).unwrap();
        assert_eq!(o.open_frame(&f[0], false).unwrap(), b"one");
        assert_eq!(o.open_frame(&f[1], false).unwrap(), b"two");
        assert!(!o.is_finished());
        assert_eq!(o.open_frame(&f[2], true).unwrap(), b"three");
        assert!(o.is_finished());
        assert_eq!(o.open_frame(&f[2], true), None, "nothing may follow the last frame");
    }

    #[test]
    fn stream_rejects_missing_last_frame() {
        let (header, f) = three_frames();
        // Truncated after two frames: never finished.
        let mut o = StreamOpener::new(&SECRET, &header).unwrap();
        o.open_frame(&f[0], false).unwrap();
        o.open_frame(&f[1], false).unwrap();
        assert!(!o.is_finished());
        // A last frame read as a middle frame, or a middle frame passed off as last, fails.
        assert_eq!(o.open_frame(&f[2], false), None);
        let mut o2 = StreamOpener::new(&SECRET, &header).unwrap();
        assert_eq!(o2.open_frame(&f[0], true), None);
    }

    #[test]
    fn stream_rejects_reordered_frames() {
        let (header, f) = three_frames();
        let mut o = StreamOpener::new(&SECRET, &header).unwrap();
        assert_eq!(o.open_frame(&f[1], false), None);
        let mut o = StreamOpener::new(&SECRET, &header).unwrap();
        o.open_frame(&f[0], false).unwrap();
        assert_eq!(o.open_frame(&f[0], false), None);
    }

    #[test]
    fn empty_stream_is_one_last_frame() {
        let (mut s, header) = StreamSealer::new(&SECRET);
        let f = s.frame(b"", true);
        let mut o = StreamOpener::new(&SECRET, &header).unwrap();
        assert_eq!(o.open_frame(&f, true).unwrap(), b"");
        assert!(o.is_finished());
    }

    #[test]
    fn rejects_wrong_secret_header_and_tampering() {
        let (header, f) = three_frames();
        let mut o = StreamOpener::new(&[1u8; 32], &header).unwrap();
        assert_eq!(o.open_frame(&f[0], false), None);
        let mut bad = header.clone();
        bad[0] = 0x03;
        assert!(StreamOpener::new(&SECRET, &bad).is_none());
        assert!(StreamOpener::new(&SECRET, &header[..10]).is_none());
        let mut t = f[0].clone();
        *t.last_mut().unwrap() ^= 1;
        let mut o = StreamOpener::new(&SECRET, &header).unwrap();
        assert_eq!(o.open_frame(&t, false), None);
    }

    #[test]
    fn full_size_frame_round_trips() {
        let (mut s, header) = StreamSealer::new(&SECRET);
        let big = vec![0xabu8; MAX_FRAME_PLAINTEXT];
        let f = s.frame(&big, true);
        let mut o = StreamOpener::new(&SECRET, &header).unwrap();
        assert_eq!(o.open_frame(&f, true).unwrap(), big);
    }

    #[test]
    fn stream_opens_kotlin_vector() {
        let v = crate::vectors::load();
        let secret = crate::vectors::secret(&v);
        let plain = crate::vectors::b64(&v, "plainB64");
        let stream = crate::vectors::b64(&v, "streamV4B64");
        let mut o = StreamOpener::new(&secret, &stream[..HEADER_BYTES]).expect("header");
        // Vector is header + a single last frame.
        let got = o.open_frame(&stream[HEADER_BYTES..], true).expect("frame");
        assert_eq!(got, plain);
    }

    #[test]
    fn stream_opens_kotlin_multi_frame_vector() {
        let v = crate::vectors::load();
        let secret = crate::vectors::secret(&v);
        let plain = crate::vectors::b64(&v, "plainMultiB64");
        let stream = crate::vectors::b64(&v, "streamV4MultiB64");
        assert_eq!(plain.len(), MAX_FRAME_PLAINTEXT + 1);
        let mut o = StreamOpener::new(&secret, &stream[..HEADER_BYTES]).expect("header");
        let first_len = 4 + MAX_FRAME_PLAINTEXT + TAG_BYTES;
        let f0 = &stream[HEADER_BYTES..HEADER_BYTES + first_len];
        let f1 = &stream[HEADER_BYTES + first_len..];
        let mut got = o.open_frame(f0, false).expect("first frame");
        assert!(!o.is_finished());
        got.extend(o.open_frame(f1, true).expect("last frame"));
        assert!(o.is_finished());
        assert_eq!(got, plain);
    }
}
