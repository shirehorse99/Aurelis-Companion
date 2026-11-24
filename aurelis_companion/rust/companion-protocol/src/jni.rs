//! JNI exports for `CompanionPairingNativeBridge` (feature `android-jni`).
//!
//! A pairing is held in a process-wide registry keyed by an opaque id, never a raw pointer, so a
//! finish/free with an unknown or already-used handle finds nothing and returns null instead of
//! touching freed memory. No function here may panic: the release profile aborts on panic.

use crate::pairing::Pairing;
use crate::registry::Registry;
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jbyteArray, jlong};
use jni::EnvUnowned;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static LIVE: Mutex<Option<Registry>> = Mutex::new(None);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn insert(p: Pairing) -> Option<u64> {
    let mut live = LIVE.lock().ok()?;
    Some(live.get_or_insert_with(Registry::new).insert(p, now_ms()))
}

fn take(id: u64) -> Option<Pairing> {
    LIVE.lock().ok()?.as_mut()?.take(id, now_ms())
}

/// Returns `[8-byte big-endian handle][SPAKE2 message]`, or null on failure.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_security_CompanionPairingNativeBridge_nativeStart<'l>(
    mut env: EnvUnowned,
    _class: JClass<'l>,
    code: JString<'l>,
) -> jbyteArray {
    env.with_env(|env| -> Result<jbyteArray, jni::errors::Error> {
        let code = match code.try_to_string(env) {
            Ok(c) if !c.is_empty() => c,
            _ => return Ok(std::ptr::null_mut()),
        };
        let (p, msg) = Pairing::start(&code);
        let Some(id) = insert(p) else { return Ok(std::ptr::null_mut()) };
        let mut out = id.to_be_bytes().to_vec();
        out.extend_from_slice(&msg);
        Ok(env.byte_array_from_slice(&out).map(|a| a.into_raw()).unwrap_or(std::ptr::null_mut()))
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// Consumes the handle (valid or not). Returns the 32-byte K, or null.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_security_CompanionPairingNativeBridge_nativeFinish<'l>(
    mut env: EnvUnowned,
    _class: JClass<'l>,
    handle: jlong,
    peer_msg: JByteArray<'l>,
) -> jbyteArray {
    env.with_env(|env| -> Result<jbyteArray, jni::errors::Error> {
        let Some(p) = (handle > 0).then(|| take(handle as u64)).flatten() else {
            return Ok(std::ptr::null_mut());
        };
        let Ok(msg) = env.convert_byte_array(&peer_msg) else { return Ok(std::ptr::null_mut()) };
        let Some(mut k) = p.finish(&msg) else { return Ok(std::ptr::null_mut()) };
        let out = env.byte_array_from_slice(&k).map(|a| a.into_raw()).unwrap_or(std::ptr::null_mut());
        k.iter_mut().for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
        Ok(out)
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// Drops an abandoned pairing. Unknown handles are ignored.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_security_CompanionPairingNativeBridge_nativeFree<'l>(
    _env: EnvUnowned,
    _class: JClass<'l>,
    handle: jlong,
) {
    if handle > 0 {
        let _ = take(handle as u64);
    }
}
