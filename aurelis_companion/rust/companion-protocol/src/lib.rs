//! Wire protocol shared by the Aurelis Companion and the television.
//! See the Companion design: v3 small-body sealing, v4 framed streams, request proof, card.

pub mod card;
#[cfg(feature = "android-jni")]
pub mod jni;
pub mod pairing;
pub mod registry;
pub mod proof;
pub mod seal;
pub mod stream;

/// Referenced by `aurelis-core` so the linker keeps this crate's JNI exports.
pub fn _force_link() {}

#[cfg(test)]
pub(crate) mod vectors {
    use serde_json::Value;

    /// The shared vector file written by the Kotlin test. Its absence is a test FAILURE: a
    /// skipped cross-language check reads as green and proves nothing.
    pub fn load() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../core/common/src/test/resources/companion_vectors.json"
        );
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).expect("companion_vectors.json is not JSON"),
            Err(e) => panic!("shared vector file {path} cannot be read ({e}); run the Kotlin CompanionVectorsTest to write it"),
        }
    }

    /// Standard base64, padded or not.
    pub fn b64(v: &Value, field: &str) -> Vec<u8> {
        use base64::Engine;
        let s = v[field].as_str().unwrap_or_else(|| panic!("missing field {field}"));
        let s = s.trim().trim_end_matches('=');
        base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(s)
            .unwrap_or_else(|e| panic!("bad base64 in {field}: {e}"))
    }

    pub fn secret(v: &Value) -> [u8; 32] {
        let hex = v["secretHex"].as_str().expect("missing secretHex");
        assert_eq!(hex.len(), 64);
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }
}
