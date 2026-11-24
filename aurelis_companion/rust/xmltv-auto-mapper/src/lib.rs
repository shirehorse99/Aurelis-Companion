//! Native channel→XMLTV-EPG-id auto-mapping helpers for Aurelis's `core/data`
//! `XmltvAutoMapper`.
//!
//! Ports the pure-compute pieces of `XmltvAutoMapper.kt` — `normalize` (from
//! `EpgChannelKeys`), `stripSuffixes`, `tokenize`, and `findBestMatch`'s 4-tier
//! matching pipeline (exact tvg-id, exact name, stripped name, token Jaccard
//! ≥ 0.6 with ≥ 2 intersection).
//!
//! The `run()` method (DB/DAO orchestration — `epgDao.getDistinctChannelIds()`,
//! `channelDao.getAllChannels()`, `channelDao.updateEpgId()`) stays Kotlin-side.
//! It serialises the `normEpgIds` list as a JSON array of `{"raw","norm"}`
//! objects to pass across the FFI boundary.
//!
//! Owns no long-lived state: each call is independent. Not wired in as the
//! app's active auto-mapper anywhere — `XmltvAutoMapper`'s own Kotlin
//! implementation remains the only one actually selected, mirroring every
//! other optional Rust crate's dormant status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::OnceLock;

/// Bump when the FFI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

/// Non-alphanumeric regex for `normalize` (Kotlin: `Regex("[^a-z0-9&]+")`).
fn non_alnum_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[^a-z0-9&]+").unwrap())
}

/// Suffix-strip regex (Kotlin: `SUFFIX_REGEX`).
fn strip_suffix_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)(hd|fhd|uhd|4k|sd|\+1|\+2|h265|hevc|uk|us|ca|de|fr|es|it|nl|au|nz|ie|pl|se|no|fi|dk|br|mx|ar|cl|co|pt|gr|tr|ru)+$").unwrap()
    })
}

/// Tokenize-split regex (Kotlin: `Regex("[^a-z0-9]+")`).
fn tokenize_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[^a-z0-9]+").unwrap())
}

fn is_suffix_token(tok: &str) -> bool {
    matches!(
        tok,
        "hd" | "fhd" | "uhd" | "4k" | "sd" | "+1" | "+2" | "h265" | "hevc"
            | "uk" | "us" | "ca" | "de" | "fr" | "es" | "it" | "nl" | "au" | "nz" | "ie"
            | "pl" | "se" | "no" | "fi" | "dk" | "br" | "mx" | "ar" | "cl" | "co" | "pt"
            | "gr" | "tr" | "ru"
    )
}

/// Byte-for-byte port of `XmltvAutoMapper.normalizeKey`: trim, lowercase,
/// replace `&amp;` with `&`, remove all non-alphanumeric separators.
pub fn normalize(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lower = trimmed.to_lowercase();
    let with_amp = lower.replace("&amp;", "&");
    non_alnum_re().replace_all(&with_amp, "").to_string()
}

/// Byte-for-byte port of `stripSuffixes`: strip trailing quality/country suffixes.
pub fn strip_suffixes(s: &str) -> String {
    strip_suffix_re()
        .replace_all(s, "")
        .trim()
        .to_string()
}

/// Byte-for-byte port of `tokenize`: split on `[^a-z0-9]+`, filter length ≥ 2 and not a suffix token.
pub fn tokenize(s: &str) -> Vec<String> {
    let lower = s.to_lowercase().replace("&amp;", "&");
    tokenize_re()
        .split(&lower)
        .filter(|part| part.chars().count() >= 2 && !is_suffix_token(part))
        .map(|s| s.to_string())
        .collect()
}

/// JSON entry for `find_best_match` — one normalised EPG id.
#[derive(Debug, Clone, Deserialize)]
pub struct EpgIdEntry {
    pub raw: String,
    pub norm: String,
}

/// Byte-for-byte port of `findBestMatch` — the 4-tier matching pipeline.
///
/// Tiers (descending priority):
/// 1. Exact tvg-id / epg_channel_id match (normalised existing epgId in the set).
/// 2. Exact channel name match (normalised name == normalised EPG id).
/// 3. Stripped name match (suffixes removed from both sides).
/// 4. Token Jaccard ≥ 0.6 with ≥ 2 token intersection.
///
/// `epg_ids_json` is a JSON array of `{"raw":"...","norm":"..."}` objects.
/// Returns the raw EPG id of the best match, or `None` if no match.
pub fn find_best_match(
    channel_name: &str,
    channel_epg_id: &str,
    epg_ids_json: &str,
) -> Option<String> {
    let epg_ids: Vec<EpgIdEntry> = serde_json::from_str(epg_ids_json).unwrap_or_default();
    if epg_ids.is_empty() {
        return None;
    }

    let epg_id_set: HashSet<&str> = epg_ids.iter().map(|e| e.norm.as_str()).collect();

    let norm_name = normalize(channel_name);
    let stripped = strip_suffixes(&norm_name);

    // Tier 1: existing epgId is already valid.
    let norm_existing = normalize(channel_epg_id);
    if !norm_existing.is_empty() && epg_id_set.contains(norm_existing.as_str()) {
        return Some(channel_epg_id.to_string());
    }

    // Tier 2: exact normalised name match.
    if let Some(entry) = epg_ids.iter().find(|e| e.norm == norm_name) {
        return Some(entry.raw.clone());
    }

    // Tier 3: stripped name match (both sides with suffixes removed).
    if !stripped.is_empty() {
        if let Some(entry) = epg_ids
            .iter()
            .find(|e| strip_suffixes(&e.norm) == stripped)
        {
            return Some(entry.raw.clone());
        }
    }

    // Tier 4: token Jaccard similarity.
    let name_tok_vec = tokenize(channel_name);
    let name_toks: HashSet<&str> = name_tok_vec.iter().map(|s| s.as_str()).collect();
    if name_toks.len() < 2 {
        return None;
    }

    let mut best_score = 0.0_f64;
    let mut best_raw: Option<String> = None;
    for entry in &epg_ids {
        let tok_vec = tokenize(&entry.raw);
        let toks: HashSet<&str> = tok_vec.iter().map(|s| s.as_str()).collect();
        if toks.len() < 2 {
            continue;
        }
        let intersection = name_toks.intersection(&toks).count();
        let union = name_toks.union(&toks).count();
        if union == 0 {
            continue;
        }
        let jaccard = intersection as f64 / union as f64;
        if jaccard > best_score && jaccard >= 0.6 && intersection >= 2 {
            best_score = jaccard;
            best_raw = Some(entry.raw.clone());
        }
    }
    best_raw
}

// ---------------------------------------------------------------------------
// JNI boundary
// ---------------------------------------------------------------------------

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

fn string_to_jstring(env: &mut Env, s: &str) -> jstring {
    env.new_string(s).unwrap_or_default().into_raw()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_XmltvAutoMapperNativeBridge_nativeNormalize<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    value: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let v = jstring_to_string(env, &value);
        let result = normalize(&v);
        string_to_jstring(env, &result)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_XmltvAutoMapperNativeBridge_nativeStripSuffixes<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    s: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let input = jstring_to_string(env, &s);
        let result = strip_suffixes(&input);
        string_to_jstring(env, &result)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_XmltvAutoMapperNativeBridge_nativeTokenize<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    s: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let input = jstring_to_string(env, &s);
        let tokens = tokenize(&input);
        let json = serde_json::to_string(&tokens).unwrap_or_else(|_| "[]".to_string());
        string_to_jstring(env, &json)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_XmltvAutoMapperNativeBridge_nativeFindBestMatch<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    channel_name: JString<'local>,
    channel_epg_id: JString<'local>,
    epg_ids_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let name = jstring_to_string(env, &channel_name);
        let epg_id = jstring_to_string(env, &channel_epg_id);
        let json = jstring_to_string(env, &epg_ids_json);
        let result = find_best_match(&name, &epg_id, &json).unwrap_or_default();
        string_to_jstring(env, &result)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_XmltvAutoMapperNativeBridge_nativeEngineVersion<
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

    // ---- normalize ----

    #[test]
    fn normalize_trims_and_lowercases() {
        assert_eq!(normalize("  BBC One HD  "), "bbconehd");
    }

    #[test]
    fn normalize_replaces_amp_entity() {
        assert_eq!(normalize("AT&T"), "at&t");
    }

    #[test]
    fn normalize_removes_all_non_alnum() {
        assert_eq!(normalize("Sky.Sports.News.HQ"), "skysportsnewshq");
    }

    #[test]
    fn normalize_empty_for_blank() {
        assert_eq!(normalize("   "), "");
        assert_eq!(normalize(""), "");
    }

    // ---- strip_suffixes ----

    #[test]
    fn strip_removes_hd_suffix() {
        assert_eq!(strip_suffixes("bbconehd"), "bbcone");
    }

    #[test]
    fn strip_removes_4k_suffix() {
        assert_eq!(strip_suffixes("channel4k"), "channel");
    }

    #[test]
    fn strip_removes_plus1_suffix() {
        assert_eq!(strip_suffixes("itv+1"), "itv");
    }

    #[test]
    fn strip_removes_uk_tld() {
        assert_eq!(strip_suffixes("bbconeuk"), "bbcone");
    }

    #[test]
    fn strip_no_change_without_suffix() {
        assert_eq!(strip_suffixes("bbcone"), "bbcone");
    }

    // ---- tokenize ----

    #[test]
    fn tokenize_splits_non_alnum() {
        assert_eq!(tokenize("bbc_one"), vec!["bbc", "one"]);
    }

    #[test]
    fn tokenize_filters_single_chars() {
        assert_eq!(tokenize("a-bb-ccc"), vec!["bb", "ccc"]);
    }

    #[test]
    fn tokenize_filters_suffix_tokens() {
        assert_eq!(tokenize("BBC One HD uk"), vec!["bbc", "one"]);
    }

    #[test]
    fn tokenize_empty_for_punctuation_only() {
        assert!(tokenize("---___").is_empty());
    }

    // ---- find_best_match ----

    fn epg_ids_json(entries: &[(&str, &str)]) -> String {
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(|(raw, norm)| {
                serde_json::json!({"raw": raw, "norm": norm})
            })
            .collect();
        serde_json::Value::Array(arr).to_string()
    }

    #[test]
    fn tier1_existing_epg_id_already_valid() {
        let json = epg_ids_json(&[
            ("bbc.one", "bbcone"),
            ("itv1", "itv1"),
        ]);
        let result = find_best_match("Random Channel", "bbcone", &json);
        assert_eq!(result, Some("bbcone".to_string()));
    }

    #[test]
    fn tier2_exact_name_match() {
        let json = epg_ids_json(&[
            ("cnn.international", "cnninternational"),
            ("bbc.news", "bbcnews"),
        ]);
        let result = find_best_match("BBC News", "", &json);
        assert_eq!(result, Some("bbc.news".to_string()));
    }

    #[test]
    fn tier3_stripped_name_match() {
        let json = epg_ids_json(&[
            ("BBC.One.uk", "bbconeuk"),
            ("Sky.Sports.Main.Event.uk", "skysportsmaineventuk"),
        ]);
        assert_eq!(find_best_match("BBC One HD", "", &json), Some("BBC.One.uk".to_string()));
        assert_eq!(find_best_match("Sky Sports Main Event FHD", "", &json), Some("Sky.Sports.Main.Event.uk".to_string()));
    }

    #[test]
    fn tier4_token_jaccard_match() {
        let json = epg_ids_json(&[
            ("discovery.science.channel.uk", "discoverysciencechanneluk"),
        ]);
        let result = find_best_match("Discovery Science", "", &json);
        assert_eq!(result, Some("discovery.science.channel.uk".to_string()));
    }

    #[test]
    fn tier4_rejects_low_jaccard() {
        let json = epg_ids_json(&[
            ("national.geographic.wild.channel.uk", "nationalgeographicwildchanneluk"),
        ]);
        let result = find_best_match("National Geographic", "", &json);
        assert_eq!(result, None);
    }

    #[test]
    fn no_match_returns_none() {
        let json = epg_ids_json(&[
            ("cnn", "cnn"),
        ]);
        let result = find_best_match("BBC News", "", &json);
        assert_eq!(result, None);
    }

    #[test]
    fn tier4_rejects_single_token_channel() {
        let json = epg_ids_json(&[
            ("skysports", "skysports"),
        ]);
        let result = find_best_match("Sky", "", &json);
        assert_eq!(result, None);
    }

    #[test]
    fn empty_epg_ids_returns_none() {
        let result = find_best_match("BBC News", "", "[]");
        assert_eq!(result, None);
    }

    #[test]
    fn invalid_json_returns_none() {
        let result = find_best_match("BBC News", "", "not valid json");
        assert_eq!(result, None);
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
