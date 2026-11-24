//! Build script that forces the linker to include ALL object files from
//! dependency rlibs, ensuring `#[no_mangle]` JNI exports are present
//! in the final `libaurelis_core.so`.
//!
//! `--whole-archive` tells the linker to include every object file in
//! each archive (rlib), not just those with referenced symbols.
//! `--no-whole-archive` restores normal behavior for subsequent archives.
fn main() {
    // The rlib dependencies are linked between these two flags.
    // This ensures every crate's #[no_mangle] JNI functions survive
    // LTO and appear in the final .so's dynamic symbol table.
    println!("cargo:rustc-cdylib-link-arg=-Wl,--whole-archive");
    println!("cargo:rustc-cdylib-link-arg=-Wl,--no-whole-archive");
}
