//! Shared JNI infrastructure for Aurelis's native crates.
//! See `docs/performance/NATIVE_SPEED_WINS_PLAN.md` (WS-JNI).
//!
//! Two concerns live here:
//!
//! * **Handle cache** — a process-lifetime cache of hot JNI class lookups,
//!   populated once from `aurelis-core`'s `JNI_OnLoad`, so hot paths stop
//!   calling `find_class("java/lang/String")` on every invocation.
//! * **Zero-copy readers** — borrow the bytes of a direct `java.nio.ByteBuffer`
//!   with no copy, so multi-MB inputs (Xtream/XMLTV catalogues) cross the JNI
//!   boundary without the UTF-8 copy that `JNIEnv::get_string` performs.
//!
//! Every accessor degrades gracefully: if `on_load` never ran (e.g. a single
//! crate's own `cargo test`, or a leaf-crate cdylib built without
//! `aurelis-core`'s `JNI_OnLoad`), the handle cache falls back to `find_class`.
//! Nothing here can break a build that links one crate on its own.

use jni::objects::{JByteBuffer, JClass, JObject, JObjectArray};
use jni::refs::Reference;
use jni::sys::jobject;
use jni::{jni_str, Env, JavaVM};
use std::sync::OnceLock;

/// Cached global reference to `java/lang/String`, stored as the raw `jobject`
/// pointer. The backing `GlobalRef` is intentionally leaked (process-lifetime
/// cache) so the reference stays valid for as long as the `.so` is loaded.
static STRING_CLASS: OnceLock<usize> = OnceLock::new();

/// Populate the handle cache. Call exactly once, from `JNI_OnLoad`.
///
/// `JNI_OnLoad` runs on an already-attached thread, so `attach_current_thread`
/// only borrows that attachment and leaves it in place.
pub fn on_load(vm: &JavaVM) {
    if STRING_CLASS.get().is_some() {
        return;
    }
    let _ = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let cls = env.find_class(jni_str!("java/lang/String"))?;
        let global = env.new_global_ref(&cls)?;
        // Keep the global reference alive for the process lifetime: `into_raw`
        // hands ownership to the cache, which never deletes it.
        let raw = global.into_raw() as usize;
        let _ = STRING_CLASS.set(raw);
        Ok(())
    });
}

/// Allocate a `String[]` of `len` nulls, using the cached `java/lang/String`
/// class when available and falling back to `find_class` otherwise.
pub fn new_string_object_array<'local>(
    env: &mut Env<'local>,
    len: i32,
) -> jni::errors::Result<JObjectArray<'local>> {
    if let Some(&raw) = STRING_CLASS.get() {
        // SAFETY: `raw` is a live global ref to `java/lang/String` created in
        // `on_load` and never deleted. The `JClass` built from it is only
        // borrowed for this call; a raw-constructed `JClass`'s `Drop` is a
        // no-op (it does not delete the reference).
        let cls = unsafe { JClass::kind_from_raw(raw as jobject) };
        return env.new_object_array(len, &cls, JObject::null());
    }
    let cls = env.find_class(jni_str!("java/lang/String"))?;
    env.new_object_array(len, &cls, JObject::null())
}

/// Borrow the bytes of a `java.nio.ByteBuffer` allocated with
/// `allocateDirect(...)`, with no copy. Returns `None` for a non-direct
/// buffer, a null address, or a zero capacity.
///
/// # Safety
/// The returned slice borrows memory owned by the JVM `ByteBuffer`; it is only
/// valid while that buffer is reachable on the Kotlin side (i.e. for the
/// duration of the native call it was passed into). Do not retain it past the
/// call, and do not let Kotlin release/GC the buffer while it is borrowed.
pub unsafe fn direct_buffer_bytes<'a>(env: &Env, buf: &JByteBuffer) -> Option<&'a [u8]> {
    let addr = env.get_direct_buffer_address(buf).ok()?;
    if addr.is_null() {
        return None;
    }
    let cap = env.get_direct_buffer_capacity(buf).ok()?;
    if cap == 0 {
        return None;
    }
    Some(std::slice::from_raw_parts(addr, cap))
}
