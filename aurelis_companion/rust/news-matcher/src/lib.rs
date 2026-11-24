//! Native news-channel matcher and breaking-news classifier for Aurelis's
//! `core/domain` news logic.
//!
//! Ports two pure-compute pieces:
//!  * `LocalNewsMatcher.findBestChannelMatch` — Jaccard token similarity +
//!    direct-substring bonus between a headline source name and candidate
//!    channel names, with a 0.4 confidence threshold.
//!  * `BreakingNewsClassifier.classify` — keyword-scoring (critical / major-
//!    event / high tiers) with a linear time-decay (full under 1h, → 0 at 6h).
//!
//! Owns no long-lived state: each call is independent. Not wired in as the
//! active matcher/classifier anywhere — the Kotlin implementations remain the
//! only ones actually selected, mirroring `rust/bitrate-calc`'s dormant status.

use std::collections::HashSet;
use std::sync::OnceLock;

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jlong, jstring};
use jni::Env;
use regex::Regex;
use serde::Serialize;

/// Bump when the FFI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

/// Words stripped during tokenisation — quality markers, country codes, and
/// generic filler that would inflate Jaccard similarity without carrying signal.
const IGNORED_WORDS: &[&str] = &[
    "hd", "sd", "fhd", "4k", "uk", "us", "news", "tv", "channel", "the", "and", "live",
];

static TOKENIZE_REGEX: OnceLock<Regex> = OnceLock::new();

/// Byte-for-byte port of `LocalNewsMatcher.tokenize`: split on whitespace or
/// non-alphanumeric runs, lowercase, drop blanks and ignored words, collect to
/// a `HashSet`.
fn tokenize(text: &str) -> HashSet<String> {
    let re = TOKENIZE_REGEX
        .get_or_init(|| Regex::new(r"\s+|[^a-zA-Z0-9]+").unwrap());
    let ignored: HashSet<&str> = IGNORED_WORDS.iter().copied().collect();
    re.split(text)
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty() && !ignored.contains(s.as_str()))
        .collect()
}

/// Byte-for-byte port of `LocalNewsMatcher.findBestChannelMatch`'s scoring
/// loop. Returns the index of the best-scoring channel, or `None` when no
/// channel reaches the 0.4 threshold.
///
/// Scoring = Jaccard similarity of token sets + 0.3 direct-substring bonus
/// (either string contains the other).
pub fn find_best_channel_match(source: &str, channel_names: &[String]) -> Option<usize> {
    let source_lower = source.to_lowercase();
    let source_trimmed = source_lower.trim();
    if source_trimmed.is_empty() {
        return None;
    }
    let source_tokens = tokenize(source_trimmed);
    if source_tokens.is_empty() {
        return None;
    }

    let mut best_idx: Option<usize> = None;
    let mut best_score: f32 = 0.0;

    for (idx, name) in channel_names.iter().enumerate() {
        let channel_lower = name.to_lowercase();
        let channel_trimmed = channel_lower.trim();
        let channel_tokens = tokenize(channel_trimmed);
        if channel_tokens.is_empty() {
            continue;
        }

        // Jaccard similarity
        let intersect = source_tokens.intersection(&channel_tokens).count();
        let union = source_tokens.union(&channel_tokens).count();
        let jaccard = if union > 0 {
            intersect as f32 / union as f32
        } else {
            0.0
        };

        // Direct substring bonus
        let direct_substring_bonus = if channel_trimmed.contains(source_trimmed)
            || source_trimmed.contains(channel_trimmed)
        {
            0.3
        } else {
            0.0
        };

        let final_score = jaccard + direct_substring_bonus;

        if final_score >= 0.4 && final_score > best_score {
            best_score = final_score;
            best_idx = Some(idx);
        }
    }

    best_idx
}

/// Byte-for-byte port of `BreakingNewsClassifier.classify`'s scoring logic.
/// Returns `(is_breaking, breaking_score)`.
///
/// Keyword tiers: critical = 0.9, major-event = 0.85, high = 0.8, else 0.0.
/// Time decay: full weight under 1h, linear decay to 0.0 at 6h.
/// `is_breaking = final_score >= 0.5`.
pub fn classify_breaking(title: &str, published_at_ms: i64, now_ms: i64) -> (bool, f32) {
    let title_upper = title.to_uppercase();

    let critical_keywords: &[&str] = &["CRITICAL", "URGENT", "JUST IN", "NEWS FLASH"];
    let major_event_keywords: &[&str] = &[
        "RESIGNS", "RESIGNATION", "STEPS DOWN", "OUSTED", "NO CONFIDENCE", "IMPEACH",
        "WINS ELECTION", "GENERAL ELECTION", "STATE OF EMERGENCY", "EARTHQUAKE",
        "EXPLOSION", "WILDFIRE", "EVACUAT", "CEASEFIRE", "INVASION", "OUTBREAK",
        "PANDEMIC", "HOSTAGE", "GUNMAN", "TERROR", "KILLED", "ASSASSINAT", "COLLAPSES",
        "RED WARNING", "SEVERE WARNING", "MAJOR INCIDENT",
    ];
    let high_keywords: &[&str] = &["BREAKING", "ALERT", "DEVELOPING"];

    let base_score: f32 = if critical_keywords.iter().any(|k| title_upper.contains(k)) {
        0.9
    } else if major_event_keywords.iter().any(|k| title_upper.contains(k)) {
        0.85
    } else if high_keywords.iter().any(|k| title_upper.contains(k)) {
        0.8
    } else {
        0.0
    };

    // Time-decay
    let age_ms = (now_ms - published_at_ms).max(0);
    let age_hours = age_ms as f32 / (60.0 * 60.0 * 1000.0);
    let decay_factor = if age_hours <= 1.0 {
        1.0
    } else if age_hours >= 6.0 {
        0.0
    } else {
        1.0 - ((age_hours - 1.0) / 5.0)
    };

    let final_score = (base_score * decay_factor).clamp(0.0, 1.0);
    let is_breaking = final_score >= 0.5;

    (is_breaking, final_score)
}

// ---------------------------------------------------------------------------
// JNI boundary
// ---------------------------------------------------------------------------

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// JSON output struct for `classify_breaking`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClassifyResult {
    is_breaking: bool,
    breaking_score: f32,
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
/// Returns the best-matching channel name, or an empty string when no match
/// reaches the threshold.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_news_NewsMatcherNativeBridge_nativeFindBestChannelMatch<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    source: JString<'local>,
    channel_names_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let source_str = jstring_to_string(env, &source);
        let json_str = jstring_to_string(env, &channel_names_json);

        let channel_names: Vec<String> =
            serde_json::from_str(&json_str).unwrap_or_default();

        let result = match find_best_channel_match(&source_str, &channel_names) {
            Some(idx) => channel_names.get(idx).cloned().unwrap_or_default(),
            None => String::new(),
        };
        env.new_string(result).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
/// Returns JSON `{"isBreaking":bool,"breakingScore":float}`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_news_NewsMatcherNativeBridge_nativeClassifyBreaking<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    title: JString<'local>,
    published_at_ms: jlong,
    now_ms: jlong,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let title_str = jstring_to_string(env, &title);
        let (is_breaking, breaking_score) = classify_breaking(&title_str, published_at_ms, now_ms);
        let result = ClassifyResult {
            is_breaking,
            breaking_score,
        };
        let json = serde_json::to_string(&result).unwrap_or_default();
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_news_NewsMatcherNativeBridge_nativeEngineVersion<
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

    // --- tokenize ---

    #[test]
    fn tokenize_splits_on_whitespace_and_non_alnum() {
        let tokens = tokenize("BBC News HD");
        assert!(tokens.contains("bbc"));
        assert!(!tokens.contains("news")); // ignored
        assert!(!tokens.contains("hd")); // ignored
    }

    #[test]
    fn tokenize_lowercases_all_tokens() {
        let tokens = tokenize("Sky NEWS");
        assert!(tokens.contains("sky"));
        assert!(!tokens.contains("news")); // ignored
    }

    #[test]
    fn tokenize_returns_empty_set_for_ignored_only_input() {
        let tokens = tokenize("HD SD 4K TV");
        assert!(tokens.is_empty());
    }

    #[test]
    fn tokenize_returns_empty_set_for_blank_input() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   ").is_empty());
    }

    // --- find_best_channel_match ---

    #[test]
    fn finds_exact_substring_channel_match() {
        let source = "BBC";
        let channels = vec![
            "ITV".to_string(),
            "BBC News".to_string(),
            "Sky News".to_string(),
        ];
        let idx = find_best_channel_match(source, &channels);
        assert_eq!(idx, Some(1)); // "BBC News" contains "BBC"
    }

    #[test]
    fn returns_none_when_no_channel_meets_threshold() {
        let source = "Reuters";
        let channels = vec![
            "ITV".to_string(),
            "Sky Sports".to_string(),
        ];
        let idx = find_best_channel_match(source, &channels);
        assert_eq!(idx, None);
    }

    #[test]
    fn returns_none_for_blank_source() {
        let channels = vec!["BBC".to_string()];
        assert_eq!(find_best_channel_match("", &channels), None);
        assert_eq!(find_best_channel_match("   ", &channels), None);
    }

    #[test]
    fn returns_none_for_empty_channel_list() {
        assert_eq!(find_best_channel_match("BBC", &[]), None);
    }

    #[test]
    fn returns_none_when_all_tokens_are_ignored() {
        // "HD News TV" → all tokens are ignored → source_tokens empty → None
        let channels = vec!["BBC".to_string()];
        assert_eq!(find_best_channel_match("HD News TV", &channels), None);
    }

    #[test]
    fn jaccard_similarity_picks_best_overlapping_channel() {
        let source = "CNN International";
        let channels = vec![
            "BBC One".to_string(),
            "CNN".to_string(),
            "Sky News".to_string(),
        ];
        let idx = find_best_channel_match(source, &channels);
        assert_eq!(idx, Some(1)); // "cnn" token overlaps
    }

    // --- classify_breaking ---

    #[test]
    fn classify_critical_keyword_full_score_under_1h() {
        let now = 1_000_000_000_000_i64;
        let published = now - 30 * 60 * 1000; // 30 min ago
        let (is_breaking, score) = classify_breaking("URGENT: Market crash", published, now);
        assert!(is_breaking);
        assert!((score - 0.9).abs() < 0.001);
    }

    #[test]
    fn classify_major_event_keyword_score() {
        let now = 1_000_000_000_000_i64;
        let published = now - 30 * 60 * 1000;
        let (is_breaking, score) = classify_breaking("PM resigns unexpectedly", published, now);
        assert!(is_breaking);
        assert!((score - 0.85).abs() < 0.001);
    }

    #[test]
    fn classify_high_keyword_score() {
        let now = 1_000_000_000_000_i64;
        let published = now - 30 * 60 * 1000;
        let (is_breaking, score) = classify_breaking("BREAKING: Update on story", published, now);
        assert!(is_breaking);
        assert!((score - 0.8).abs() < 0.001);
    }

    #[test]
    fn classify_no_keyword_returns_zero() {
        let now = 1_000_000_000_000_i64;
        let published = now - 30 * 60 * 1000;
        let (is_breaking, score) = classify_breaking("Weather forecast for today", published, now);
        assert!(!is_breaking);
        assert!((score - 0.0).abs() < 0.001);
    }

    #[test]
    fn classify_time_decay_full_under_1h() {
        let now = 1_000_000_000_000_i64;
        let published = now - 30 * 60 * 1000; // 30 min
        let (_, score) = classify_breaking("BREAKING: Test", published, now);
        assert!((score - 0.8).abs() < 0.001); // full weight
    }

    #[test]
    fn classify_time_decay_linear_between_1h_and_6h() {
        let now = 1_000_000_000_000_i64;
        let published = now - 3 * 60 * 60 * 1000; // 3 hours
        let (_, score) = classify_breaking("BREAKING: Test", published, now);
        // decay = 1 - (3-1)/5 = 1 - 0.4 = 0.6; score = 0.8 * 0.6 = 0.48
        assert!((score - 0.48).abs() < 0.001);
    }

    #[test]
    fn classify_time_decay_zero_after_6h() {
        let now = 1_000_000_000_000_i64;
        let published = now - 7 * 60 * 60 * 1000; // 7 hours
        let (is_breaking, score) = classify_breaking("BREAKING: Test", published, now);
        assert!(!is_breaking);
        assert!((score - 0.0).abs() < 0.001);
    }

    #[test]
    fn classify_is_breaking_threshold_at_0_5() {
        let now = 1_000_000_000_000_i64;
        // At ~2.75h, decay = 1 - (2.75-1)/5 = 1 - 0.35 = 0.65; score = 0.8*0.65 = 0.52 → breaking
        let published = now - (2 * 60 * 60 * 1000 + 45 * 60 * 1000); // 2h45m
        let (is_breaking, score) = classify_breaking("BREAKING: Test", published, now);
        assert!(is_breaking);
        assert!(score >= 0.5);
    }

    #[test]
    fn classify_negative_age_clamped_to_zero() {
        let now = 1_000_000_000_000_i64;
        let published = now + 60_000; // 1 min in the future
        let (is_breaking, score) = classify_breaking("BREAKING: Test", published, now);
        assert!(is_breaking);
        assert!((score - 0.8).abs() < 0.001); // full weight, age clamped to 0
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}
