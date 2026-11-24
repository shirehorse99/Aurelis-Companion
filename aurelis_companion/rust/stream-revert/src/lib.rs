//! Native twin of Aurelis's `core/domain` `BestStreamRevertPolicy`.
//!
//! Decides when to return to the candidate a poor-stream switch abandoned.
//! `PoorStreamSwitchPolicy` (see `rust/`'s Kotlin side) only ever moves
//! downhill: three picture breaks in ninety seconds and the viewer is left on
//! a backup for the rest of the session. This is the uphill half — once the
//! backup has been clean long enough to suggest the fault has passed, the
//! better feed is worth another try.
//!
//! Owns no state: every call takes the counters the engine already keeps.
//! Dormant by the same terms as `rust/bitrate-calc` and
//! `rust/subtitle-offset` — the Kotlin object remains the only implementation
//! actually selected. Ported so a change to the rule cannot land on one side
//! only, which is the failure mode `AGENTS.md` records for `channel-merge`.

use jni::{EnvUnowned};
use jni::objects::JClass;
use jni::sys::{jboolean, jint, jlong, JNI_FALSE, JNI_TRUE};
use jni::Env;

/// Bump when the FFI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

/// Clean playback on the backup before the better feed is worth another try.
pub const STABILITY_MS: i64 = 5 * 60_000;

/// Reverts per session before leaving it to the viewer.
pub const MAX_ATTEMPTS: i32 = 2;

/// Byte-for-byte port of `BestStreamRevertPolicy.shouldRevert`.
///
/// `ms_since_switch` is negative when the Kotlin side had `null` — no switch
/// has happened, so there is nothing to revert to.
pub fn should_revert(
    enabled: bool,
    left_better_candidate: bool,
    ms_since_switch: i64,
    breaks_since_switch: i32,
    attempts_made: i32,
) -> bool {
    if !enabled {
        return false;
    }
    if !left_better_candidate {
        return false;
    }
    if attempts_made >= MAX_ATTEMPTS {
        return false;
    }
    // A backup that is itself breaking up is no platform to gamble from.
    if breaks_since_switch > 0 {
        return false;
    }
    if ms_since_switch < 0 {
        return false;
    }
    ms_since_switch >= STABILITY_MS
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_player_StreamRevertNativeBridge_nativeShouldRevert<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
    enabled: jboolean,
    left_better_candidate: jboolean,
    ms_since_switch: jlong,
    breaks_since_switch: jint,
    attempts_made: jint,
) -> jboolean {
    let revert = should_revert(
        enabled,
        left_better_candidate,
        ms_since_switch,
        breaks_since_switch,
        attempts_made,
    );
    if revert {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_player_StreamRevertNativeBridge_nativeEngineVersion<
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

    /// Mirrors `BestStreamRevertPolicyTest` on the Kotlin side, name for name, so a
    /// divergence shows up as one suite passing and the other failing.
    #[test]
    fn reverts_once_the_backup_has_been_clean_long_enough() {
        assert!(should_revert(true, true, STABILITY_MS, 0, 0));
        assert!(should_revert(true, true, STABILITY_MS * 3, 0, 0));
    }

    #[test]
    fn the_setting_is_honoured() {
        assert!(!should_revert(false, true, STABILITY_MS, 0, 0));
    }

    #[test]
    fn a_stream_nobody_was_driven_off_is_never_reverted() {
        assert!(!should_revert(true, false, STABILITY_MS, 0, 0));
        // Negative stands in for Kotlin's null "no switch has happened".
        assert!(!should_revert(true, true, -1, 0, 0));
    }

    #[test]
    fn an_unstable_backup_does_not_revert() {
        assert!(!should_revert(true, true, STABILITY_MS, 1, 0));
        assert!(!should_revert(true, true, STABILITY_MS * 10, 5, 0));
    }

    #[test]
    fn too_soon_after_the_switch_does_not_revert() {
        assert!(!should_revert(true, true, 0, 0, 0));
        assert!(!should_revert(true, true, STABILITY_MS - 1, 0, 0));
    }

    #[test]
    fn gives_up_after_two_attempts() {
        assert!(should_revert(true, true, STABILITY_MS, 0, MAX_ATTEMPTS - 1));
        assert!(!should_revert(true, true, STABILITY_MS, 0, MAX_ATTEMPTS));
        assert!(!should_revert(true, true, STABILITY_MS, 0, MAX_ATTEMPTS + 5));
    }

    #[test]
    fn the_bar_is_minutes_not_seconds() {
        assert!(STABILITY_MS >= 60_000);
    }
}

// ── Adaptive playback ────────────────────────────────────────────────────────
//
// Twin of `core/domain` `AdaptivePlaybackPolicy`: what to do when a stream that
// WAS playing smoothly starts rebuffering. Same dormant status as the revert
// half above — ported so the rule cannot change on one side only.

/// Smooth playback before a rebuffer counts as degradation rather than a bad start.
pub const SMOOTH_BEFORE_TRIGGER_MS: i64 = 30_000;

/// Extra buffer, in ms, requested while eased.
pub const BUFFER_BOOST_MS: i32 = 4_000;

/// Byte-for-byte port of `AdaptivePlaybackPolicy.shouldEase`.
///
/// `smooth_ms_before_rebuffer` is negative when the Kotlin side had `null` — the
/// stream has not started playing at all.
pub fn should_ease(enabled: bool, already_eased: bool, smooth_ms_before_rebuffer: i64) -> bool {
    if !enabled {
        return false;
    }
    if already_eased {
        return false;
    }
    if smooth_ms_before_rebuffer < 0 {
        return false;
    }
    smooth_ms_before_rebuffer >= SMOOTH_BEFORE_TRIGGER_MS
}

/// Byte-for-byte port of `AdaptivePlaybackPolicy.forceHighestBitrate`.
pub fn force_highest_bitrate(config_forces_highest: bool, eased: bool) -> bool {
    config_forces_highest && !eased
}

/// Byte-for-byte port of `AdaptivePlaybackPolicy.bufferBoostMs`.
pub fn buffer_boost_ms(eased: bool) -> i32 {
    if eased {
        BUFFER_BOOST_MS
    } else {
        0
    }
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_player_StreamRevertNativeBridge_nativeShouldEase<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
    enabled: jboolean,
    already_eased: jboolean,
    smooth_ms_before_rebuffer: jlong,
) -> jboolean {
    if should_ease(enabled, already_eased, smooth_ms_before_rebuffer) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[cfg(test)]
mod adaptive_tests {
    use super::*;

    #[test]
    fn eases_when_a_good_stream_starts_rebuffering() {
        assert!(should_ease(true, false, SMOOTH_BEFORE_TRIGGER_MS));
        assert!(should_ease(true, false, SMOOTH_BEFORE_TRIGGER_MS * 10));
    }

    #[test]
    fn the_setting_is_honoured() {
        assert!(!should_ease(false, false, SMOOTH_BEFORE_TRIGGER_MS));
    }

    #[test]
    fn a_stream_that_never_got_going_is_not_eased() {
        assert!(!should_ease(true, false, -1));
        assert!(!should_ease(true, false, 0));
        assert!(!should_ease(true, false, SMOOTH_BEFORE_TRIGGER_MS - 1));
    }

    #[test]
    fn eases_only_once_per_stream() {
        assert!(!should_ease(true, true, SMOOTH_BEFORE_TRIGGER_MS));
        assert!(!should_ease(true, true, SMOOTH_BEFORE_TRIGGER_MS * 100));
    }

    #[test]
    fn easing_releases_the_bitrate_pin_without_inventing_one() {
        assert!(force_highest_bitrate(true, false));
        assert!(!force_highest_bitrate(true, true));
        assert!(!force_highest_bitrate(false, false));
        assert!(!force_highest_bitrate(false, true));
    }

    #[test]
    fn buffer_boost_applies_only_while_eased() {
        assert_eq!(buffer_boost_ms(false), 0);
        assert_eq!(buffer_boost_ms(true), BUFFER_BOOST_MS);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
