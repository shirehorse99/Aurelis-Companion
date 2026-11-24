//! Native HLS-manifest/throughput compute helpers for Aurelis's `core/domain`
//! `BitrateProbeService`.
//!
//! Ports the three pure "given already-fetched text/bytes, compute a number"
//! pieces of `probeHlsBitrate`/`probeHlsSubManifest`/`probeTsBitrate`
//! (`BitrateProbeService.kt`): the `BANDWIDTH=` tag scan, the "first non-comment
//! manifest line" URI lookup (used to follow a master manifest to its highest
//! sub-manifest), and the byte-count/elapsed-time throughput formula. Every
//! network call (`OkHttpClient`/`Request`/tuner-slot allocation) and
//! `StreamHealthTracker` bookkeeping stays Kotlin-side -- this crate never
//! touches the network, mirroring `rust/xtream-parser`'s "raw text in, numbers
//! out" scope.
//!
//! Owns no long-lived state: each call is independent. Not wired in as the app's
//! active probe logic anywhere -- `BitrateProbeService`'s own Kotlin
//! implementations remain the only ones actually selected, mirroring
//! `rust/xtream-parser`'s dormant status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jlong, jstring};
use jni::Env;

/// Bump when the FFI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

/// Byte-for-byte port of the `BANDWIDTH_REGEX.findAll(body)` scan used by both
/// `probeHlsBitrate` and `probeHlsSubManifest` (`Regex("""BANDWIDTH=(\d+)""")`,
/// case-sensitive, matching the HLS spec's uppercase attribute name).
fn find_bandwidths(manifest: &str) -> Vec<i64> {
    let bytes = manifest.as_bytes();
    let needle = b"BANDWIDTH=";
    let mut result = Vec::new();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let start = i + needle.len();
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                if let Ok(n) = manifest[start..j].parse::<i64>() {
                    result.push(n);
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    result
}

/// Byte-for-byte port of the bandwidth-tag branch of `probeHlsBitrate`/
/// `probeHlsSubManifest`: highest `BANDWIDTH=` value in bps, converted to kbps
/// via integer division (matching Kotlin's `bandwidths.max() / 1000`).
pub fn parse_hls_bandwidth_kbps(manifest: &str) -> Option<i64> {
    find_bandwidths(manifest).into_iter().max().map(|bps| bps / 1000)
}

/// Byte-for-byte port of the sub-manifest URI lookup in `probeHlsBitrate`:
/// first trimmed, non-empty, non-comment (`#`-prefixed) line.
pub fn first_manifest_uri_line(manifest: &str) -> Option<String> {
    manifest
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
}

/// Byte-for-byte port of `probeTsBitrate`'s final throughput calculation:
/// `((bytesRead * 8.0) / (elapsedMs / 1000.0) / 1000.0).toInt().coerceIn(1, 100_000)`,
/// with the same `elapsedMs < 100 || bytesRead < 4096 -> 0` early-out.
pub fn calc_ts_throughput_kbps(bytes_read: i64, elapsed_ms: i64) -> i64 {
    if elapsed_ms < 100 || bytes_read < 4096 {
        return 0;
    }
    let kbps = ((bytes_read as f64 * 8.0) / (elapsed_ms as f64 / 1000.0) / 1000.0) as i64;
    kbps.clamp(1, 100_000)
}

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`. Returns
/// `-1` when no `BANDWIDTH=` tag is found (Kotlin treats that as "try the fallback path").
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_BitrateCalcNativeBridge_nativeParseHlsBandwidthKbps<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    manifest: JString<'local>,
) -> jlong {
    env.with_env(|env| -> std::result::Result<jlong, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &manifest);
        parse_hls_bandwidth_kbps(&text).unwrap_or(-1)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`. Returns
/// an empty string when no candidate URI line is found.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_BitrateCalcNativeBridge_nativeFirstManifestUriLine<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    manifest: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &manifest);
        let result = first_manifest_uri_line(&text).unwrap_or_default();
        env.new_string(result).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_BitrateCalcNativeBridge_nativeCalcTsThroughputKbps<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
    bytes_read: jlong,
    elapsed_ms: jlong,
) -> jlong {
    calc_ts_throughput_kbps(bytes_read, elapsed_ms)
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_BitrateCalcNativeBridge_nativeEngineVersion<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
) -> jint {
    ENGINE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_highest_bandwidth_from_master_manifest() {
        let manifest = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=640x360\nlow.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=5120000,RESOLUTION=1920x1080\nhigh.m3u8\n";
        assert_eq!(parse_hls_bandwidth_kbps(manifest), Some(5120));
    }

    #[test]
    fn returns_none_when_no_bandwidth_tag_present() {
        let manifest = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:10.0,\nsegment0.ts\n";
        assert_eq!(parse_hls_bandwidth_kbps(manifest), None);
    }

    #[test]
    fn bandwidth_scan_is_case_sensitive() {
        let manifest = "#EXT-X-STREAM-INF:bandwidth=999999\nlow.m3u8\n";
        assert_eq!(parse_hls_bandwidth_kbps(manifest), None);
    }

    #[test]
    fn finds_first_non_comment_uri_line() {
        let manifest = "#EXTM3U\n  \n#EXT-X-VERSION:3\nsub/playlist.m3u8\n#EXT-X-ENDLIST\n";
        assert_eq!(first_manifest_uri_line(manifest), Some("sub/playlist.m3u8".to_string()));
    }

    #[test]
    fn no_uri_line_returns_none() {
        let manifest = "#EXTM3U\n#EXT-X-VERSION:3\n";
        assert_eq!(first_manifest_uri_line(manifest), None);
    }

    #[test]
    fn calc_ts_throughput_matches_expected_formula() {
        // 256 KB in 1000ms = 256*1024*8 bits / 1s = 2097.152 kbps -> truncated to 2097.
        assert_eq!(calc_ts_throughput_kbps(256 * 1024, 1000), 2097);
    }

    #[test]
    fn calc_ts_throughput_rejects_too_fast_or_too_small_samples() {
        assert_eq!(calc_ts_throughput_kbps(256 * 1024, 50), 0);
        assert_eq!(calc_ts_throughput_kbps(1000, 1000), 0);
    }

    #[test]
    fn calc_ts_throughput_clamps_to_expected_range() {
        assert_eq!(calc_ts_throughput_kbps(1_000_000_000, 100), 100_000);
        assert_eq!(calc_ts_throughput_kbps(4096, 100_000), 1);
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
