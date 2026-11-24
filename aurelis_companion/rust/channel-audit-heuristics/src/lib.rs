//! Native port of `ChannelCategoryAuditor`'s self-contained name-rescue/hint
//! heuristics (`core/domain` `ChannelCategoryAuditor.kt`).
//!
//! `ChannelCategoryAuditor.audit` is a pure diagnostic tool: it's invoked
//! on-demand for a single channel at a time (by AI or a human debugging why one
//! channel landed in the wrong tab), never in a hot loop over the full channel
//! list, so there is genuinely no CPU payoff from porting it -- ported anyway at
//! the user's explicit request for completeness.
//!
//! Only the pieces with zero dependency on other unported Kotlin objects are
//! ported: the two name-rescue approximations (`looksLikeNewsNameApprox`/
//! `looksLikeLocalNameApprox`), the per-step rescue classifier
//! (`detectWhichRescueFired`), and the final hint-list assembly. The real
//! category mapping (`ProviderGenreMapper.categorize`), country detection
//! (`CountryAffinity.detectForChannel`), and canonical-plan lookup
//! (`LocaleChannelPlan.rankFor`) all stay Kotlin-side -- this crate takes their
//! outputs as caller-precomputed primitives, mirroring `rust/channel-merge`'s
//! and `rust/epg-channel-sort`'s scope.
//!
//! Owns no long-lived state. Not wired in as the app's active auditor anywhere
//! -- `ChannelCategoryAuditor`'s own Kotlin implementation remains the only one
//! actually selected, mirroring `rust/channel-merge`'s dormant status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jstring, JNI_FALSE, JNI_TRUE};
use jni::Env;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

const NEWS_NAME_NEEDLES: [&str; 6] =
    ["sky news", "skynews", "bbc news", "itv news", "channel 4 news", "gb news"];

/// Byte-for-byte port of `looksLikeNewsNameApprox`.
pub fn looks_like_news_name(name: &str) -> bool {
    let n = name.to_lowercase();
    NEWS_NAME_NEEDLES.iter().any(|needle| n.contains(needle))
}

fn local_name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(bbc\s*(one|1|two|2|itv|channel\s*4|channel\s*5))\b").unwrap()
    })
}

/// Byte-for-byte port of `looksLikeLocalNameApprox`.
pub fn looks_like_local_name(name: &str) -> bool {
    let n = name.to_lowercase();
    local_name_regex().is_match(&n)
}

/// Byte-for-byte port of `detectWhichRescueFired`. `category` is the mapper
/// result's enum name (e.g. `"NEWS"`, `"OTHERS"`), or an empty string for `null`.
pub fn detect_rescue(category: &str, name: &str) -> Option<String> {
    if category.is_empty() {
        return None;
    }
    let n = name.to_lowercase();
    Some(match category {
        "NEWS" => if n.contains("news") { "newsName" } else { "genre-news" }.to_string(),
        "MUSIC" => {
            if n.contains("music") || n.contains("mtv") { "musicName" } else { "genre-music" }.to_string()
        }
        "LOCAL" => "localName".to_string(),
        "REGIONAL" => "regionalName".to_string(),
        "RADIO" => "radioName".to_string(),
        "ENTERTAINMENT" | "OTHERS" => "no-rescue".to_string(),
        _ => return None,
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HintsInput {
    pub arbiter_category: String,
    pub would_match_news: bool,
    pub genre_contains_music: bool,
    pub any_variant_entertainment_or_others: bool,
    pub country_filter_would_hide: bool,
    pub matches_canonical_news_plan: bool,
    pub matches_canonical_music_plan: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HintsOutput {
    pub hints: Vec<String>,
}

/// Byte-for-byte port of the six `hints +=` conditions at the end of `audit`.
pub fn build_diagnosis_hints(input: &HintsInput) -> Vec<String> {
    let mut hints = Vec::new();
    if input.arbiter_category != "NEWS" && input.would_match_news {
        hints.push(format!(
            "Name looks like news but final category is {} (rescue may have been overridden by a variant's genre)",
            input.arbiter_category
        ));
    }
    if input.arbiter_category != "MUSIC" && input.genre_contains_music {
        hints.push("Genre contains 'music' but rescue did not promote to MUSIC category".to_string());
    }
    if input.any_variant_entertainment_or_others {
        hints.push(
            "At least one source variant mapped to ENTERTAINMENT/OTHERS — this can pull the merged result away from NEWS/MUSIC"
                .to_string(),
        );
    }
    if input.country_filter_would_hide {
        hints.push("Country code mismatch may be hiding it from the selected country tab".to_string());
    }
    if input.matches_canonical_news_plan && input.arbiter_category != "NEWS" {
        hints.push(
            "Channel matches main UK provider NEWS plan (tvchannellists.com / Sky/Virgin/Freeview style) but was not assigned NEWS"
                .to_string(),
        );
    }
    if input.matches_canonical_music_plan && input.arbiter_category != "MUSIC" {
        hints.push("Channel matches main provider MUSIC plan but was not assigned MUSIC".to_string());
    }
    hints
}

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelAuditNativeBridge_nativeLooksLikeNewsName<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    name: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &name);
        if looks_like_news_name(&text) { JNI_TRUE } else { JNI_FALSE }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelAuditNativeBridge_nativeLooksLikeLocalName<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    name: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &name);
        if looks_like_local_name(&text) { JNI_TRUE } else { JNI_FALSE }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`. Returns
/// an empty string for `None` (Kotlin's `null` case).
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelAuditNativeBridge_nativeDetectRescue<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    category: JString<'local>,
    name: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let category = jstring_to_string(env, &category);
        let name = jstring_to_string(env, &name);
        let result = detect_rescue(&category, &name).unwrap_or_default();
        env.new_string(result).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelAuditNativeBridge_nativeBuildDiagnosisHints<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let raw = jstring_to_string(env, &input_json);
        let output = match serde_json::from_str::<HintsInput>(&raw) {
            Ok(input) => HintsOutput { hints: build_diagnosis_hints(&input) },
            Err(_) => HintsOutput { hints: Vec::new() },
        };
        let json = serde_json::to_string(&output).unwrap_or_else(|_| "{\"hints\":[]}".to_string());
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelAuditNativeBridge_nativeEngineVersion<
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
    fn detects_known_news_channel_names() {
        assert!(looks_like_news_name("Sky News HD"));
        assert!(looks_like_news_name("BBC NEWS"));
        assert!(!looks_like_news_name("Sky Sports Main Event"));
    }

    #[test]
    fn detects_bbc_prefixed_variants_as_local() {
        // The regex nests every alternative under a literal "bbc" prefix, so bare
        // "ITV"/"Channel 4" do NOT match on their own -- this mirrors the shipped
        // Kotlin regex exactly, not a corrected interpretation of it.
        assert!(looks_like_local_name("BBC One HD"));
        assert!(looks_like_local_name("BBC Two"));
        assert!(looks_like_local_name("BBC 1"));
        assert!(!looks_like_local_name("ITV"));
        assert!(!looks_like_local_name("Channel 4"));
        assert!(!looks_like_local_name("Sky News"));
    }

    #[test]
    fn detect_rescue_maps_news_by_name_vs_genre() {
        assert_eq!(detect_rescue("NEWS", "Sky News"), Some("newsName".to_string()));
        assert_eq!(detect_rescue("NEWS", "Some Channel"), Some("genre-news".to_string()));
    }

    #[test]
    fn detect_rescue_maps_music_by_name_vs_genre() {
        assert_eq!(detect_rescue("MUSIC", "MTV Hits"), Some("musicName".to_string()));
        assert_eq!(detect_rescue("MUSIC", "Some Channel"), Some("genre-music".to_string()));
    }

    #[test]
    fn detect_rescue_handles_other_categories_and_none() {
        assert_eq!(detect_rescue("LOCAL", "x"), Some("localName".to_string()));
        assert_eq!(detect_rescue("OTHERS", "x"), Some("no-rescue".to_string()));
        assert_eq!(detect_rescue("", "x"), None);
    }

    #[test]
    fn build_diagnosis_hints_covers_all_six_conditions() {
        let input = HintsInput {
            arbiter_category: "ENTERTAINMENT".to_string(),
            would_match_news: true,
            genre_contains_music: true,
            any_variant_entertainment_or_others: true,
            country_filter_would_hide: true,
            matches_canonical_news_plan: true,
            matches_canonical_music_plan: true,
        };
        let hints = build_diagnosis_hints(&input);
        assert_eq!(hints.len(), 6);
    }

    #[test]
    fn build_diagnosis_hints_returns_empty_when_nothing_flagged() {
        let input = HintsInput {
            arbiter_category: "NEWS".to_string(),
            would_match_news: true,
            genre_contains_music: false,
            any_variant_entertainment_or_others: false,
            country_filter_would_hide: false,
            matches_canonical_news_plan: true,
            matches_canonical_music_plan: false,
        };
        assert!(build_diagnosis_hints(&input).is_empty());
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
