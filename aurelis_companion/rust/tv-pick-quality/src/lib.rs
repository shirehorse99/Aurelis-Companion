//! TV pick quality scoring and title matching for "Today's TV" / TV Times picks.
//!
//! Ports the pure-compute pieces of `core/common`'s `TodayTvPickQuality`:
//! - `normalizeTitle` — lowercased, article/year stripped, punctuation flattened
//! - `titlesMatch` — conservative multi-word significant-token subset match
//! - `qualityScore` — heuristic desirability score (playable, provenance, art, rating)
//!
//! `cleanGenre` depends on `ProviderGenreMapper.mapGenericGroup` which stays
//! Kotlin-side, so it is NOT ported. `finalize` involves sorting a list of
//! domain objects and stays Kotlin — but it calls `qualityScore` internally,
//! which is Rust-delegated. The scoring logic is ported but takes primitive
//! inputs via JNI (the `TodayTvPick` domain object is decomposed by the bridge).
//!
//! Owns no long-lived state: each call is independent. Not wired in as the
//! app's active logic anywhere — the Kotlin implementation remains the only one
//! actually selected, mirroring the other dormant Rust engines.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jdouble, jint, jstring};
use jni::Env;
use regex::Regex;
use std::sync::LazyLock;

/// Bump when the FFI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

// ── Regex patterns (byte-for-byte from TodayTvPickQuality.kt) ────────────────

/// `Regex("^(the|a|an)\\s+")` — leading article + whitespace.
static LEADING_ARTICLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(the|a|an)\s+").unwrap());
/// `Regex("\\s*\\(?(19|20)\\d{2}\\)?\\s*$")` — trailing year, optionally wrapped
/// in parens. Note: the `(19|20)` capture group is irrelevant for `replace_all`.
static TRAILING_YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\(?(19|20)\d{2}\)?\s*$").unwrap());
/// `Regex("[^a-z0-9 ]")` — single non-alnum-non-space char (input is lowercased).
static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9 ]").unwrap());
/// `Regex("\\s+")` — whitespace runs.
static MULTISPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// Tokens too generic to anchor a match on their own.
const STOPWORDS: &[&str] = &["the", "a", "an", "of", "and", "to", "in", "on", "with", "&"];

// ── Pure functions ───────────────────────────────────────────────────────────

/// Byte-for-byte port of `TodayTvPickQuality.normalizeTitle`:
/// lowercase → strip trailing year → non-alnum to space → collapse multispace →
/// trim → strip leading article → trim.
pub fn normalize_title(raw: &str) -> String {
    let s = raw.to_lowercase();
    let s = TRAILING_YEAR.replace_all(&s, "");
    let s = NON_ALNUM.replace_all(&s, " ");
    let s = MULTISPACE.replace_all(&s, " ");
    let s = s.trim();
    let s = LEADING_ARTICLE.replace_all(s, "");
    s.trim().to_string()
}

fn significant_tokens(normalized: &str) -> Vec<&str> {
    normalized
        .split(' ')
        .filter(|w| !w.is_empty() && !STOPWORDS.contains(w))
        .collect()
}

/// Byte-for-byte port of `TodayTvPickQuality.titlesMatch`:
/// conservative title match — equal normalised titles, or one title's
/// significant tokens are a complete multi-word subset of the other's.
/// Single-word titles must match exactly (handled by the `small.size < 2` guard).
pub fn titles_match(a: &str, b: &str) -> bool {
    let na = normalize_title(a);
    let nb = normalize_title(b);
    if na.trim().is_empty() || nb.trim().is_empty() {
        return false;
    }
    if na == nb {
        return true;
    }

    let ta = significant_tokens(&na);
    let tb = significant_tokens(&nb);
    if ta.is_empty() || tb.is_empty() {
        return false;
    }

    let (small, big) = if ta.len() <= tb.len() {
        (&ta, &tb)
    } else {
        (&tb, &ta)
    };
    if small.len() < 2 {
        return false;
    }
    let big_set: std::collections::HashSet<&str> = big.iter().copied().collect();
    small.iter().all(|w| big_set.contains(w))
}

/// Byte-for-byte port of `TodayTvPickQuality.qualityScore`:
/// heuristic desirability score. Each `isNullOrBlank()` check maps to
/// `!s.trim().is_empty()` (empty string from JNI = null/blank).
pub fn quality_score(
    channel_id: Option<&str>,
    source_article_url: Option<&str>,
    backdrop_url: Option<&str>,
    poster_url: Option<&str>,
    rating: Option<&str>,
    ai_blurb: Option<&str>,
) -> f64 {
    let mut score = 0.0;
    if !is_null_or_blank(channel_id) {
        score += 100.0;
    }
    if !is_null_or_blank(source_article_url) {
        score += 40.0;
    }
    if !is_null_or_blank(backdrop_url) {
        score += 30.0;
    }
    if !is_null_or_blank(poster_url) {
        score += 8.0;
    }
    let rating_val = rating
        .map(|s| s.trim().parse::<f64>().ok())
        .flatten()
        .unwrap_or(0.0);
    score += rating_val * 5.0;
    if !is_null_or_blank(ai_blurb) {
        score += 4.0;
    }
    score
}

/// Threshold for considering an S01E01 series opener critically acclaimed or well-received
/// based on online reviews (IMDb, Rotten Tomatoes, Metacritic, or TMDB).
pub const ACCLAIMED_SERIES_OPENER_THRESHOLD: f64 = 7.0;

/// Extracts an effective online review score on a 0.0 to 10.0 scale from available
/// ratings: IMDb, Rotten Tomatoes (converted from percentage), Metacritic (converted from 0-100),
/// or TMDB vote average.
pub fn effective_review_score(
    rating: Option<&str>,
    omdb_imdb: Option<&str>,
    omdb_rt: Option<&str>,
    omdb_meta: Option<&str>,
) -> Option<f64> {
    if let Some(s) = omdb_imdb {
        if let Ok(v) = s.trim().parse::<f64>() {
            return Some(v);
        }
    }
    if let Some(s) = omdb_rt {
        let cleaned = s.trim().trim_end_matches('%').trim();
        if let Ok(v) = cleaned.parse::<f64>() {
            return Some(v / 10.0);
        }
    }
    if let Some(s) = omdb_meta {
        if let Ok(v) = s.trim().parse::<f64>() {
            return Some(v / 10.0);
        }
    }
    if let Some(s) = rating {
        if let Ok(v) = s.trim().parse::<f64>() {
            return Some(v);
        }
    }
    None
}

/// Evaluates whether an S01E01 series premiere qualifies as one of the "better/best ones"
/// based on positive online reviews (rating >= 7.0) or press critic coverage.
pub fn is_acclaimed_series_opener(
    is_series_opener: bool,
    source_article_url: Option<&str>,
    rating: Option<&str>,
    omdb_imdb: Option<&str>,
    omdb_rt: Option<&str>,
    omdb_meta: Option<&str>,
) -> bool {
    if !is_series_opener {
        return false;
    }
    if !is_null_or_blank(source_article_url) {
        return true;
    }
    match effective_review_score(rating, omdb_imdb, omdb_rt, omdb_meta) {
        Some(score) => score >= ACCLAIMED_SERIES_OPENER_THRESHOLD,
        None => false,
    }
}

/// Kotlin `String?.isNullOrBlank()` — true if None or trim is empty.
fn is_null_or_blank(s: Option<&str>) -> bool {
    match s {
        None => true,
        Some(v) => v.trim().is_empty(),
    }
}

// ── JNI helpers ──────────────────────────────────────────────────────────────

fn read_jstring(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

fn make_jstring(env: &mut Env, s: &str) -> jstring {
    env.new_string(s).unwrap_or_else(|_| env.new_string("").unwrap()).into_raw()
}

// ── JNI exports ──────────────────────────────────────────────────────────────

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TvPickQualityNativeBridge_nativeNormalizeTitle<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    raw: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let s = read_jstring(env, &raw);
        make_jstring(env, &normalize_title(&s))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with valid `JNIEnv`/`JClass`/`JString` args.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TvPickQualityNativeBridge_nativeTitlesMatch<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    a: JString<'local>,
    b: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let sa = read_jstring(env, &a);
        let sb = read_jstring(env, &b);
        titles_match(&sa, &sb)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI. Empty strings = null/blank. Rating is parsed
/// as double (empty/unparseable = 0.0).
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TvPickQualityNativeBridge_nativeQualityScore<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    channel_id: JString<'local>,
    source_article_url: JString<'local>,
    backdrop_url: JString<'local>,
    poster_url: JString<'local>,
    rating: JString<'local>,
    ai_blurb: JString<'local>,
) -> jdouble {
    env.with_env(|env| -> std::result::Result<jdouble, jni::errors::Error> {
        Ok({
        let channel_id_s = read_jstring(env, &channel_id);
        let source_article_url_s = read_jstring(env, &source_article_url);
        let backdrop_url_s = read_jstring(env, &backdrop_url);
        let poster_url_s = read_jstring(env, &poster_url);
        let rating_s = read_jstring(env, &rating);
        let ai_blurb_s = read_jstring(env, &ai_blurb);

        quality_score(
            if channel_id_s.is_empty() { None } else { Some(&channel_id_s) },
            if source_article_url_s.is_empty() { None } else { Some(&source_article_url_s) },
            if backdrop_url_s.is_empty() { None } else { Some(&backdrop_url_s) },
            if poster_url_s.is_empty() { None } else { Some(&poster_url_s) },
            if rating_s.is_empty() { None } else { Some(&rating_s) },
            if ai_blurb_s.is_empty() { None } else { Some(&ai_blurb_s) },
        )
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TvPickQualityNativeBridge_nativeEngineVersion<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
) -> jint {
    ENGINE_VERSION
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalize_title ──────────────────────────────────────────────────────

    #[test]
    fn normalize_title_strips_leading_article() {
        assert_eq!(normalize_title("The Bear"), "bear");
        assert_eq!(normalize_title("A Quiet Place"), "quiet place");
        assert_eq!(normalize_title("An Education"), "education");
    }

    #[test]
    fn normalize_title_strips_trailing_year() {
        assert_eq!(normalize_title("Dune (2021)"), "dune");
        assert_eq!(normalize_title("Dune 2021"), "dune");
        assert_eq!(normalize_title("Inception 2010"), "inception");
    }

    #[test]
    fn normalize_title_flattens_punctuation() {
        assert_eq!(normalize_title("Spider-Man: No Way Home"), "spider man no way home");
    }

    #[test]
    fn normalize_title_empty_string() {
        assert_eq!(normalize_title(""), "");
    }

    #[test]
    fn normalize_title_article_after_year_strip() {
        // "The 2021" → year stripped → "the" → LEADING_ARTICLE needs trailing
        // whitespace (^the\s+), so bare "the" is NOT stripped.
        assert_eq!(normalize_title("The 2021"), "the");
    }

    // ── titles_match ─────────────────────────────────────────────────────────

    #[test]
    fn titles_match_equal_normalized() {
        assert!(titles_match("The Bear", "Bear"));
        assert!(titles_match("Dune (2021)", "Dune 2021"));
    }

    #[test]
    fn titles_match_subset_multi_word() {
        assert!(titles_match("Happy Valley", "Happy Valley: The Finale"));
        assert!(titles_match("Happy Valley: The Finale", "Happy Valley"));
    }

    #[test]
    fn titles_match_single_word_does_not_match_longer() {
        // "Lost" must NOT match "Lost in Space" — single-word guard
        assert!(!titles_match("Lost", "Lost in Space"));
        assert!(!titles_match("News", "Newsnight"));
    }

    #[test]
    fn titles_match_blank_returns_false() {
        assert!(!titles_match("", "Something"));
        assert!(!titles_match("Something", ""));
        assert!(!titles_match("   ", "Something"));
    }

    #[test]
    fn titles_match_completely_different() {
        assert!(!titles_match("The Bear", "The Office"));
    }

    #[test]
    fn titles_match_both_single_word_equal() {
        assert!(titles_match("The Bear", "Bear"));
    }

    // ── quality_score ────────────────────────────────────────────────────────

    #[test]
    fn quality_score_all_fields_present() {
        let score = quality_score(
            Some("ch1"),
            Some("https://example.com/article"),
            Some("https://example.com/backdrop.jpg"),
            Some("https://example.com/poster.jpg"),
            Some("8.5"),
            Some("Great pick"),
        );
        // 100 + 40 + 30 + 8 + 8.5*5 + 4 = 100 + 40 + 30 + 8 + 42.5 + 4 = 224.5
        assert!((score - 224.5).abs() < 0.001);
    }

    #[test]
    fn quality_score_all_fields_empty() {
        let score = quality_score(None, None, None, None, None, None);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn quality_score_blank_strings_treated_as_null() {
        let score = quality_score(Some("   "), Some(""), Some("  "), None, Some(""), Some(" "));
        assert_eq!(score, 0.0);
    }

    #[test]
    fn quality_score_only_channel() {
        let score = quality_score(Some("ch1"), None, None, None, None, None);
        assert_eq!(score, 100.0);
    }

    #[test]
    fn quality_score_rating_parse_failure_is_zero() {
        let score = quality_score(None, None, None, None, Some("not-a-number"), None);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn quality_score_rating_max() {
        let score = quality_score(None, None, None, None, Some("10.0"), None);
        assert_eq!(score, 50.0);
    }

    // ── effective_review_score & is_acclaimed_series_opener ──────────────────

    #[test]
    fn effective_review_score_prefers_imdb_then_rt_then_meta_then_rating() {
        assert_eq!(
            effective_review_score(Some("7.0"), Some("8.5"), Some("90%"), Some("80")),
            Some(8.5)
        );
        assert_eq!(
            effective_review_score(Some("7.0"), None, Some("85%"), Some("80")),
            Some(8.5)
        );
        assert_eq!(
            effective_review_score(Some("7.0"), None, None, Some("75")),
            Some(7.5)
        );
        assert_eq!(
            effective_review_score(Some("7.2"), None, None, None),
            Some(7.2)
        );
        assert_eq!(effective_review_score(None, None, None, None), None);
    }

    #[test]
    fn is_acclaimed_series_opener_filters_unreviewed_or_poor_openers() {
        // Not a series opener -> false
        assert!(!is_acclaimed_series_opener(false, None, Some("9.0"), None, None, None));

        // Series opener with critic article -> true (editorial review endorsement)
        assert!(is_acclaimed_series_opener(true, Some("https://theguardian.com/tv-review"), None, None, None, None));

        // Series opener with high rating (>= 7.0) -> true
        assert!(is_acclaimed_series_opener(true, None, Some("8.2"), None, None, None));
        assert!(is_acclaimed_series_opener(true, None, None, Some("7.5"), None, None));
        assert!(is_acclaimed_series_opener(true, None, None, None, Some("80%"), None));

        // Series opener with low rating (< 7.0) -> false
        assert!(!is_acclaimed_series_opener(true, None, Some("5.8"), None, None, None));
        assert!(!is_acclaimed_series_opener(true, None, None, Some("6.2"), None, None));

        // Series opener with unreviewed / null rating -> false
        assert!(!is_acclaimed_series_opener(true, None, None, None, None, None));
    }

    // ── engine version ───────────────────────────────────────────────────────

    #[test]
    fn engine_version_is_stable() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
