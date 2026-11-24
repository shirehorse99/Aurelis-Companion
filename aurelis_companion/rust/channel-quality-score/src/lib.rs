//! Native port of `ChannelArbiter`'s per-variant quality/regional scoring
//! heuristics (`core/domain` `ChannelArbiter.kt`), the ranking pass that runs
//! on every stream candidate during `unify()` on every playlist sync/reload
//! -- distinct from the already-ported `rust/channel-merge`, which handles
//! grouping/identity-merging, not scoring.
//!
//! Only the pieces with zero dependency on other unported Kotlin/Android
//! objects are ported: `getSourcePriority`, `detectAudioProfile`,
//! `isUhdChannel`, `getQualityScore`, `detectRegionFromPostcode`,
//! `normalizePreferredRegionalLabel`, `cymruNamingTier`. `getRegionalBonus`
//! and `regionalPreferenceTier` internally call `RegionalChannelFilter`
//! (ported separately as `rust/regional-channel-filter`), so here they take
//! its outputs (`taggedRegion`/`isForeign`) as caller-precomputed inputs,
//! mirroring `rust/channel-audit-heuristics`'s treatment of
//! `CountryAffinity.detectForChannel`. `smartCandidateOrdering` stays
//! Kotlin-side entirely -- it operates on `StreamCandidate`/`LocalSource`
//! domain objects and URL rewriting, not primitive scoring.
//!
//! A few dead/redundant branches in the original Kotlin are preserved
//! byte-for-byte rather than "corrected" (see `get_quality_score`'s
//! raw/direct/source else-branch) -- this crate documents but does not fix
//! them, same policy as every other crate in this repo.
//!
//! Owns no long-lived state. Not wired in as the app's active scorer
//! anywhere -- `ChannelArbiter`'s own Kotlin implementation remains the only
//! one actually selected.

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jstring, JNI_FALSE, JNI_TRUE};
use jni::{Env, EnvUnowned};
use regex::Regex;
use std::sync::OnceLock;

/// Bump when the JNI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

const DIRECT_TUNER_SOURCES: [&str; 3] = ["tvheadend", "hdhomerun", "enigma2"];

fn is_hdhomerun(source_type: Option<&str>) -> bool {
    source_type.is_some_and(|t| t.eq_ignore_ascii_case("hdhomerun"))
}

fn is_direct_tuner_source(source_type: Option<&str>) -> bool {
    match source_type {
        Some(s) => DIRECT_TUNER_SOURCES.contains(&s.to_lowercase().as_str()),
        None => false,
    }
}

/// Byte-for-byte port of `getSourcePriority`.
pub fn get_source_priority(source_type: &str) -> i32 {
    match source_type.to_lowercase().as_str() {
        "tvheadend" => 1,
        "hdhomerun" => 2,
        "enigma2" => 3,
        "xtream" => 4,
        "stalker" => 5,
        "m3u" => 6,
        _ => 10,
    }
}

fn no_audio_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(no[ _-]?audio|no[ _-]?sound|mute|muted|video[ _-]?only|silent|no[ _-]ac3|no[ _-]aac)\b").unwrap())
}

/// Byte-for-byte port of `detectAudioProfile`.
pub fn detect_audio_profile(name: &str) -> Option<&'static str> {
    if no_audio_regex().is_match(name) {
        return Some("No Audio");
    }
    let n = name.to_lowercase();
    Some(if n.contains("5.1") {
        "5.1"
    } else if n.contains("7.1") {
        "7.1"
    } else if n.contains("dts-hd") || n.contains("dtshd") {
        "DTS-HD"
    } else if n.contains("truehd") {
        "TrueHD"
    } else if n.contains("eac3") || n.contains("dd+") {
        "E-AC3"
    } else if n.contains("ac3") || n.contains("dolby") {
        "AC3"
    } else if n.contains("aac") {
        "AAC"
    } else if n.contains("mp3") {
        "MP3"
    } else if n.contains("2.0") || n.contains("stereo") {
        "2.0"
    } else if n.contains("mono") {
        "Mono"
    } else {
        return None;
    })
}

fn uhd_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(4k|uhd|2160p?|ultra\s*hd)\b").unwrap())
}

/// Byte-for-byte port of `isUhdChannel`.
pub fn is_uhd_channel(name: &str) -> bool {
    uhd_regex().is_match(name)
}

/// Byte-for-byte port of `getQualityScore`. Higher is better.
pub fn get_quality_score(name: &str, source_type: Option<&str>) -> i32 {
    let n = name.to_lowercase();
    let direct_tuner = is_direct_tuner_source(source_type);

    let mut score = if n.contains("8k") || n.contains("4320") {
        2000
    } else if n.contains("4k") || n.contains("uhd") || n.contains("2160") {
        1600
    } else if n.contains("fhd") || n.contains("1080") {
        1200
    } else if n.contains("hd") || n.contains("720") {
        if direct_tuner { 1200 } else { 800 }
    } else if n.contains("576") {
        600
    } else if n.contains("sd") || n.contains("480") {
        400
    } else if is_hdhomerun(source_type) {
        // HdHomeRunProvider appends " HD" to every channel the tuner flags HD, so an untagged
        // HDHomeRun name is the device saying SD (576i). Other tuners name nothing either way.
        600
    } else if direct_tuner {
        1200
    } else {
        700
    };

    if no_audio_regex().is_match(name) {
        score -= 1000;
    }

    let is_high_fps = n.contains("60fps")
        || n.contains("50fps")
        || n.contains("p60")
        || n.contains("p50")
        || n.contains("60hz")
        || n.contains("50hz");
    if is_high_fps {
        score += 160;
    }

    if n.contains("5.1") {
        score += 140;
    } else if n.contains("dts-hd") || n.contains("dtshd") || n.contains("truehd") {
        score += 120;
    } else if n.contains("eac3") || n.contains("dd+") {
        score += 80;
    } else if n.contains("ac3") || n.contains("dolby") {
        score += 50;
    } else if n.contains("aac") {
        score += 30;
    }

    let raw_direct_source = n.contains("raw") || n.contains("direct") || n.contains("source");
    if raw_direct_source || direct_tuner {
        score += 250;
    } else if raw_direct_source {
        // Unreachable: raw_direct_source is already false on this branch.
        // Preserved from the Kotlin source's redundant else-if, not fixed.
        score += 60;
    }

    if n.contains("hevc") || n.contains("h265") || n.contains("x265") {
        score += 35;
    }

    if n.contains("vip") || n.contains("premium") || n.contains("gold") {
        score += 25;
    }

    if n.contains("backup") || n.contains("low") || n.contains("secondary") {
        score -= 100;
    }

    if (n.contains("mono") || n.contains("2.0") || n.contains("stereo")) && !is_high_fps {
        score -= 10;
    }

    score
}

/// Byte-for-byte port of `detectRegionFromPostcode`. Note this is a
/// smaller, separate UK-only lookup table than `RegionalChannelFilter`'s --
/// it's missing Yorkshire/Anglia/Border/Channel Islands and a few prefix
/// variants, a pre-existing divergence between the two classes, not a
/// porting error.
pub fn detect_region_from_postcode(postcode: &str) -> &'static str {
    if postcode.trim().is_empty() {
        return "None";
    }
    let prefix: String = postcode
        .chars()
        .take_while(|c| c.is_alphabetic())
        .collect::<String>()
        .to_uppercase();
    match prefix.as_str() {
        "SW" | "SE" | "W" | "WC" | "N" | "E" | "EC" | "NW" => "London",
        "M" | "L" | "BL" | "BB" | "PR" | "SK" | "WA" | "WN" => "North West",
        "NE" | "SR" | "TS" | "DH" => "North East",
        "B" | "CV" | "DY" | "WS" | "WV" | "LE" | "DE" | "NG" | "ST" => "Midlands",
        "BS" | "BA" | "TA" | "EX" | "TQ" | "PL" | "TR" | "SN" | "SP" | "DT" => "South West",
        "BN" | "CT" | "GU" | "ME" | "PO" | "RH" | "SO" | "TN" | "MK" | "OX" | "SL" | "RG" => {
            "South East"
        }
        "G" | "EH" | "AB" | "DD" | "IV" | "KW" | "PA" | "PH" | "FK" | "KY" | "ML" | "TD" => {
            "Scotland"
        }
        "CF" | "SA" | "LL" | "LD" | "NP" => "Wales",
        "BT" => "Northern Ireland",
        _ => "None",
    }
}

/// Byte-for-byte port of `getRegionalBonus`, taking the
/// `RegionalChannelFilter.detectRegionInName(name)` result as a
/// caller-precomputed input instead of recomputing it here.
pub fn get_regional_bonus(tagged_region: Option<&str>, region: &str) -> i32 {
    if region == "None" {
        return 0;
    }
    let Some(tagged) = tagged_region else { return 0 };
    if tagged.eq_ignore_ascii_case(region) { 60 } else { 0 }
}

/// Byte-for-byte port of `regionalPreferenceTier`, taking
/// `RegionalChannelFilter.isForeignRegion`/`detectRegionInName`'s results as
/// caller-precomputed inputs. Lower is better; only breaks ties between
/// same-priority sources.
pub fn regional_preference_tier(
    is_foreign: bool,
    tagged_region: Option<&str>,
    user_region: Option<&str>,
) -> i32 {
    let Some(user_region) = user_region else { return 0 };
    if is_foreign {
        return 2;
    }
    let Some(tagged) = tagged_region else { return 1 };
    if tagged.eq_ignore_ascii_case(user_region) { 0 } else { 2 }
}

fn itv_wales_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bitv\s*wales\b").unwrap())
}

/// Byte-for-byte port of `normalizePreferredRegionalLabel`.
pub fn normalize_preferred_regional_label(name: &str, user_region: Option<&str>) -> String {
    let is_wales = matches!(user_region, Some(r) if r.eq_ignore_ascii_case("wales"));
    if !is_wales {
        return name.to_string();
    }
    if name.to_lowercase().contains("itv cymru wales") {
        return name.to_string();
    }
    let re = itv_wales_regex();
    if re.is_match(name) {
        re.replace(name, "ITV Cymru Wales").to_string()
    } else {
        name.to_string()
    }
}

/// Byte-for-byte port of `cymruNamingTier`. Lower is better.
pub fn cymru_naming_tier(channel_name: &str, user_region: Option<&str>) -> i32 {
    let is_wales = matches!(user_region, Some(r) if r.eq_ignore_ascii_case("wales"));
    if !is_wales {
        return 0;
    }
    let n = channel_name.to_lowercase();
    if n.contains("cymru") {
        0
    } else if n.contains("wales") {
        1
    } else {
        2
    }
}

// ── JNI boundary ──────────────────────────────────────────────────────────

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

fn opt(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelQualityScoreNativeBridge_nativeGetSourcePriority<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    source_type: JString<'local>,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &source_type);
        get_source_priority(&text)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelQualityScoreNativeBridge_nativeDetectAudioProfile<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    name: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &name);
        let result = detect_audio_profile(&text).unwrap_or("");
        env.new_string(result).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelQualityScoreNativeBridge_nativeIsUhdChannel<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    name: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &name);
        if is_uhd_channel(&text) { JNI_TRUE } else { JNI_FALSE }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
/// Empty `sourceType` stands in for Kotlin's `null`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelQualityScoreNativeBridge_nativeGetQualityScore<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    name: JString<'local>,
    source_type: JString<'local>,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
        let name_text = jstring_to_string(env, &name);
        let source_text = jstring_to_string(env, &source_type);
        get_quality_score(&name_text, opt(&source_text))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelQualityScoreNativeBridge_nativeDetectRegionFromPostcode<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    postcode: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &postcode);
        let result = detect_region_from_postcode(&text);
        env.new_string(result).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
/// Empty `taggedRegion` stands in for Kotlin's `null`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelQualityScoreNativeBridge_nativeGetRegionalBonus<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    tagged_region: JString<'local>,
    region: JString<'local>,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
        let tagged = jstring_to_string(env, &tagged_region);
        let region_text = jstring_to_string(env, &region);
        get_regional_bonus(opt(&tagged), &region_text)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
/// Empty `taggedRegion`/`userRegion` stand in for Kotlin's `null`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelQualityScoreNativeBridge_nativeRegionalPreferenceTier<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    is_foreign: jboolean,
    tagged_region: JString<'local>,
    user_region: JString<'local>,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
        let tagged = jstring_to_string(env, &tagged_region);
        let user = jstring_to_string(env, &user_region);
        regional_preference_tier(is_foreign == JNI_TRUE, opt(&tagged), opt(&user))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
/// Empty `userRegion` stands in for Kotlin's `null`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelQualityScoreNativeBridge_nativeNormalizePreferredRegionalLabel<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    name: JString<'local>,
    user_region: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let name_text = jstring_to_string(env, &name);
        let region_text = jstring_to_string(env, &user_region);
        let result = normalize_preferred_regional_label(&name_text, opt(&region_text));
        env.new_string(result).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
/// Empty `userRegion` stands in for Kotlin's `null`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelQualityScoreNativeBridge_nativeCymruNamingTier<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    channel_name: JString<'local>,
    user_region: JString<'local>,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
        let name_text = jstring_to_string(env, &channel_name);
        let region_text = jstring_to_string(env, &user_region);
        cymru_naming_tier(&name_text, opt(&region_text))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelQualityScoreNativeBridge_nativeEngineVersion<
    'local,
>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jint {
    ENGINE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_priority_orders_direct_tuners_first() {
        assert_eq!(get_source_priority("TVHeadend"), 1);
        assert_eq!(get_source_priority("hdhomerun"), 2);
        assert_eq!(get_source_priority("m3u"), 6);
        assert_eq!(get_source_priority("unknown-thing"), 10);
    }

    #[test]
    fn audio_profile_priority_order() {
        assert_eq!(detect_audio_profile("Channel 5.1 AAC"), Some("5.1"));
        assert_eq!(detect_audio_profile("Channel DTS-HD"), Some("DTS-HD"));
        assert_eq!(detect_audio_profile("Channel AAC"), Some("AAC"));
        assert_eq!(detect_audio_profile("Channel"), None);
    }

    #[test]
    fn uhd_detection_requires_word_boundary() {
        assert!(is_uhd_channel("Sky Sports 4K UHD"));
        assert!(is_uhd_channel("Channel 2160p"));
        assert!(!is_uhd_channel("Channel 21600"));
    }

    #[test]
    fn quality_score_resolution_tiers_dont_overlap() {
        let uhd = get_quality_score("Channel 4K", None);
        let fhd = get_quality_score("Channel FHD", None);
        let hd = get_quality_score("Channel HD", None);
        let sd = get_quality_score("Channel SD", None);
        assert!(uhd > fhd);
        assert!(fhd > hd);
        assert!(hd > sd);
    }

    #[test]
    fn quality_score_broadcast_hd_elevated_for_direct_tuners() {
        let hd_iptv = get_quality_score("Channel HD", Some("xtream"));
        let hd_tuner = get_quality_score("Channel HD", Some("tvheadend"));
        assert_eq!(hd_iptv, 800);
        // 1200 (elevated resolution tier) + 250 (direct-tuner source-fidelity
        // bonus -- the same isDirectTuner flag feeds both additions).
        assert_eq!(hd_tuner, 1450);
        assert!(hd_tuner > hd_iptv);
    }

    /// Mirrors `HdHomeRunUntaggedIsSdTest`. `HdHomeRunProvider` appends " HD" to every channel
    /// the tuner flags HD, so an untagged HDHomeRun name is the device saying SD (576i) — Sky
    /// News on Freeview, MPEG-2. It used to score as a 1080p tuner feed (1450) and lead.
    #[test]
    fn quality_score_untagged_hdhomerun_is_sd() {
        // 600 (576 tier) + 250 (direct-tuner source fidelity).
        assert_eq!(get_quality_score("Sky News", Some("hdhomerun")), 850);
        assert!(get_quality_score("Sky News", Some("hdhomerun")) < get_quality_score("UK: SKY NEWS FHD", Some("xtream")));
        // Tagged HD on the tuner is unchanged: 1200 + 250.
        assert_eq!(get_quality_score("BBC 1 Wales HD", Some("hdhomerun")), 1450);
        // Other tuners name nothing and still default to broadcast HD.
        assert_eq!(get_quality_score("Channel", Some("tvheadend")), 1450);
    }

    #[test]
    fn quality_score_high_fps_and_audio_bonuses_stack() {
        let base = get_quality_score("Channel HD", None);
        let boosted = get_quality_score("Channel HD 50fps 5.1", None);
        assert_eq!(boosted, base + 160 + 140);
    }

    #[test]
    fn quality_score_backup_penalty_applies() {
        let normal = get_quality_score("Channel HD", None);
        let backup = get_quality_score("Channel HD Backup", None);
        assert_eq!(backup, normal - 100);
    }

    #[test]
    fn detect_region_from_postcode_uses_its_own_smaller_table() {
        assert_eq!(detect_region_from_postcode("SW1A 1AA"), "London");
        assert_eq!(detect_region_from_postcode("CF10 1AA"), "Wales");
        // Missing from this table (present in RegionalChannelFilter's) --
        // Yorkshire prefixes fall through to "None" here, a pre-existing
        // divergence between the two classes.
        assert_eq!(detect_region_from_postcode("LS1 1AA"), "None");
        assert_eq!(detect_region_from_postcode(""), "None");
    }

    #[test]
    fn regional_bonus_only_applies_for_matching_tag() {
        assert_eq!(get_regional_bonus(Some("Wales"), "Wales"), 60);
        assert_eq!(get_regional_bonus(Some("Scotland"), "Wales"), 0);
        assert_eq!(get_regional_bonus(None, "Wales"), 0);
        assert_eq!(get_regional_bonus(Some("Wales"), "None"), 0);
    }

    #[test]
    fn regional_preference_tier_orders_own_over_untagged_over_foreign() {
        assert_eq!(regional_preference_tier(false, Some("Wales"), Some("Wales")), 0);
        assert_eq!(regional_preference_tier(false, None, Some("Wales")), 1);
        assert_eq!(regional_preference_tier(true, Some("Scotland"), Some("Wales")), 2);
        assert_eq!(regional_preference_tier(false, None, None), 0);
    }

    #[test]
    fn normalize_preferred_regional_label_only_for_wales() {
        assert_eq!(
            normalize_preferred_regional_label("ITV Wales HD", Some("Wales")),
            "ITV Cymru Wales HD"
        );
        assert_eq!(
            normalize_preferred_regional_label("ITV Cymru Wales HD", Some("Wales")),
            "ITV Cymru Wales HD"
        );
        assert_eq!(
            normalize_preferred_regional_label("ITV Wales HD", Some("Scotland")),
            "ITV Wales HD"
        );
        assert_eq!(normalize_preferred_regional_label("ITV Wales HD", None), "ITV Wales HD");
    }

    #[test]
    fn cymru_naming_tier_prefers_cymru_then_wales_then_neither() {
        assert_eq!(cymru_naming_tier("ITV Cymru Wales", Some("Wales")), 0);
        assert_eq!(cymru_naming_tier("ITV Wales", Some("Wales")), 1);
        assert_eq!(cymru_naming_tier("ITV1", Some("Wales")), 2);
        assert_eq!(cymru_naming_tier("ITV Wales", Some("Scotland")), 0);
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
