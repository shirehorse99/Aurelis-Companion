//! Hashes `src/lib.rs` into `XMLTV_PARSER_LOGIC`, the parser's logic version. A television and
//! the Companion built from different parser code therefore never mix their parsed guides.
use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    let bytes = std::fs::read("src/lib.rs").expect("xmltv-parser build.rs: cannot read src/lib.rs");
    let hex: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=XMLTV_PARSER_LOGIC={hex}");
}
