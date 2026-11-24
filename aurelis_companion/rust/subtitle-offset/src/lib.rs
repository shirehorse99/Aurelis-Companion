//! Native SRT subtitle timestamp shifter for Aurelis's `core/domain`
//! `SubtitleOffsetLogic`.
//!
//! Ports `shiftSrtContent` and `shiftSrtTimestamp`: scan each line for the
//! SRT time-range pattern `HH:MM:SS,mmm --> HH:MM:SS,mmm`, shift both
//! timestamps by `offset_ms` (clamping negative results to 0), and preserve
//! any suffix text after the match. Lines without a timestamp pattern are
//! returned unchanged.
//!
//! Owns no long-lived state: each call is independent. Not wired in as the
//! active subtitle shifter anywhere — the Kotlin implementation remains the
//! only one actually selected, mirroring `rust/bitrate-calc`'s dormant status.

use std::sync::OnceLock;

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jlong, jstring};
use jni::Env;
use regex::Regex;

/// Bump when the FFI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

static SRT_TIME_LINE: OnceLock<Regex> = OnceLock::new();

fn srt_time_line_regex() -> &'static Regex {
    SRT_TIME_LINE.get_or_init(|| {
        Regex::new(r"(\d{2}:\d{2}:\d{2},\d{3})\s*-->\s*(\d{2}:\d{2}:\d{2},\d{3})").unwrap()
    })
}

/// Byte-for-byte port of `SubtitleOffsetLogic.shiftSrtTimestamp`: parse
/// `HH:MM:SS,mmm`, add `offset_ms`, clamp to 0, reformat as
/// `%02d:%02d:%02d,%03d`. Returns the raw string unchanged if it doesn't
/// parse into exactly 4 numeric parts.
pub fn shift_srt_timestamp(raw: &str, offset_ms: i64) -> String {
    let parts: Vec<&str> = raw.trim().split([':', ',']).collect();
    if parts.len() != 4 {
        return raw.to_string();
    }
    let hours = match parts[0].parse::<i64>() {
        Ok(v) => v,
        Err(_) => return raw.to_string(),
    };
    let minutes = match parts[1].parse::<i64>() {
        Ok(v) => v,
        Err(_) => return raw.to_string(),
    };
    let seconds = match parts[2].parse::<i64>() {
        Ok(v) => v,
        Err(_) => return raw.to_string(),
    };
    let millis = match parts[3].parse::<i64>() {
        Ok(v) => v,
        Err(_) => return raw.to_string(),
    };

    let total_ms = (((hours * 60 + minutes) * 60) + seconds) * 1000 + millis + offset_ms;
    let clamped = total_ms.max(0);
    let h = clamped / 3_600_000;
    let m = (clamped % 3_600_000) / 60_000;
    let s = (clamped % 60_000) / 1000;
    let ms = clamped % 1000;
    format!("{:02}:{:02}:{:02},{:03}", h, m, s, ms)
}

/// Byte-for-byte port of `SubtitleOffsetLogic.shiftSrtContent`: when
/// `offset_ms == 0`, return content unchanged. Otherwise, for each line, trim
/// it, look for the SRT time-range pattern, shift both timestamps, and
/// reconstruct `"$start --> $end$suffix"`. Lines without a match are returned
/// as-is (original, untrimmed).
pub fn shift_srt_content(content: &str, offset_ms: i64) -> String {
    if offset_ms == 0 {
        return content.to_string();
    }
    let re = srt_time_line_regex();
    content
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if let Some(caps) = re.captures(trimmed) {
                let start = shift_srt_timestamp(&caps[1], offset_ms);
                let end = shift_srt_timestamp(&caps[2], offset_ms);
                let m = caps.get(0).unwrap();
                let suffix = &trimmed[m.end()..];
                format!("{} --> {}{}", start, end, suffix)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// JNI boundary
// ---------------------------------------------------------------------------

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
/// Returns the full SRT content with timestamps shifted by `offset_ms`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_player_SubtitleOffsetNativeBridge_nativeShiftSrtContent<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    content: JString<'local>,
    offset_ms: jlong,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &content);
        let result = shift_srt_content(&text, offset_ms);
        env.new_string(result).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_player_SubtitleOffsetNativeBridge_nativeEngineVersion<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
) -> jint {
    ENGINE_VERSION
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_zero_returns_content_unchanged() {
        let content = "1\n00:00:01,000 --> 00:00:02,000\nHello\n";
        assert_eq!(shift_srt_content(content, 0), content);
    }

    #[test]
    fn shifts_timestamps_forward_by_positive_offset() {
        let content = "00:00:01,000 --> 00:00:02,000\nHello";
        let result = shift_srt_content(content, 500);
        assert!(result.contains("00:00:01,500 --> 00:00:02,500"));
        assert!(result.contains("Hello"));
    }

    #[test]
    fn shifts_timestamps_backward_by_negative_offset() {
        let content = "00:00:05,000 --> 00:00:10,000\nHello";
        let result = shift_srt_content(content, -2000);
        assert!(result.contains("00:00:03,000 --> 00:00:08,000"));
    }

    #[test]
    fn negative_timestamps_clamped_to_zero() {
        let content = "00:00:01,000 --> 00:00:02,000\nHello";
        let result = shift_srt_content(content, -5000);
        assert!(result.contains("00:00:00,000 --> 00:00:00,000"));
    }

    #[test]
    fn lines_without_timestamps_preserved() {
        let content = "1\n00:00:01,000 --> 00:00:02,000\nHello world\n3\nNo timestamp here";
        let result = shift_srt_content(content, 1000);
        assert!(result.contains("1\n"));
        assert!(result.contains("Hello world"));
        assert!(result.contains("3\nNo timestamp here"));
    }

    #[test]
    fn multi_line_content_all_timestamps_shifted() {
        let content = "1\n00:00:01,000 --> 00:00:02,000\nFirst\n2\n00:00:03,000 --> 00:00:04,000\nSecond";
        let result = shift_srt_content(content, 1000);
        assert!(result.contains("00:00:02,000 --> 00:00:03,000"));
        assert!(result.contains("00:00:04,000 --> 00:00:05,000"));
        assert!(result.contains("First"));
        assert!(result.contains("Second"));
    }

    #[test]
    fn suffix_after_timestamp_preserved() {
        let content = "00:00:01,000 --> 00:00:02,000  aligned:middle";
        let result = shift_srt_content(content, 1000);
        assert!(result.contains("00:00:02,000 --> 00:00:03,000  aligned:middle"));
    }

    #[test]
    fn shift_timestamp_parses_and_reformats() {
        let result = shift_srt_timestamp("01:02:03,400", 600);
        assert_eq!(result, "01:02:04,000");
    }

    #[test]
    fn shift_timestamp_clamps_negative_to_zero() {
        let result = shift_srt_timestamp("00:00:01,000", -5000);
        assert_eq!(result, "00:00:00,000");
    }

    #[test]
    fn shift_timestamp_returns_raw_on_invalid_format() {
        assert_eq!(shift_srt_timestamp("invalid", 100), "invalid");
        assert_eq!(shift_srt_timestamp("01:02:03", 100), "01:02:03"); // only 3 parts
    }

    #[test]
    fn empty_content_returns_empty() {
        assert_eq!(shift_srt_content("", 1000), "");
    }

    #[test]
    fn large_positive_offset_rolls_over_hours() {
        let result = shift_srt_timestamp("00:59:59,900", 200);
        assert_eq!(result, "01:00:00,100");
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}
