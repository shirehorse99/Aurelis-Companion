//! Title normalisation, cache keys, and edition group keys for catalogue matching.
//!
//! Ports the pure string operations from `core/common`:
//! - `TitleMatcher` — `normalise`, `posterTitle`, `posterYear`, `cacheLookupKeys`
//! - `CatalogueTitleIndex.titleSearchToken` — distinctive token for SQL LIKE pre-filter
//! - `EditionGroupKeys` — `movie` / `series` stable grouping keys
//!
//! All quirks documented in the Kotlin source are preserved, not "fixed". Notably
//! `TitleMatcher.posterYear`'s `yearTag.find(title)?.groupValues?.getOrNull(1)`
//! line is dead code in Kotlin (the `\(\d{4}\)` pattern has no capture group), and
//! it is replicated here as a no-op so behaviour is identical.
//!
//! Owns no long-lived state: each call is independent.
//!
//! **LIVE, not dormant — corrected 2026-09-09.** This header used to say the Kotlin
//! implementations were "the only ones actually selected". They are not:
//! `TitleMatcher.normalise`, `posterTitle`, `posterYear` and `cacheLookupKeys` each test
//! `TitleMatcherNativeBridge.isAvailable` and return the native result before their Kotlin
//! body runs, and so do `EditionGroupKeys.movie`/`series` and
//! `CatalogueTitleIndex.titleSearchToken`. So **a normalisation fix applied only to the
//! Kotlin file changes nothing on an ARM device**, and the JVM tests will pass while it
//! does — they exercise the Kotlin path only. Port both sides, keep `cargo test` and
//! `:app:testDebugUnitTest` green, and rebuild with
//! `./gradlew :core:common:buildRustAurelisCore`; the shipped `.so` is committed to git.
//!
//! This is the third engine whose doc claimed dormancy while running on device
//! (see the `locale-channel-plan` and `epg-channel-sort` corrections in AGENTS.md).

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use regex::Regex;
use std::sync::LazyLock;

/// Bump when the FFI surface changes so Kotlin can refuse a stale .so.
/// 2 — `nativePosterTitle(name)` became `nativePosterTitleForYear(name, year)`.
pub const ENGINE_VERSION: i32 = 2;

// ── Regex patterns (byte-for-byte from TitleMatcher.kt) ──────────────────────

/// `Regex("\\(\\d{4}\\)")` — matches `(YYYY)` with NO capture group.
static YEAR_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(\d{4}\)").unwrap());
/// `Regex("\\s+\\d{4}$")` — whitespace + 4 digits at end.
static TRAILING_YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+\d{4}$").unwrap());
/// `Regex("""\s*[(\[][0-9]{4}[)\]]\s*$""")` — optional ws, `(YYYY)` or `[YYYY]`, optional ws.
static YEAR_IN_BRACKETS_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[(\[][0-9]{4}[)\]]\s*$").unwrap());
/// `Regex("""\s+[-–—|]\s*[0-9]{4}\s*$""")` — an IPTV provider's ` - YYYY` suffix.
static SEPARATOR_YEAR_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+[-–—|]\s*[0-9]{4}\s*$").unwrap());
/// `Regex("""\s+([0-9]{4})\s*$""")` — a bare trailing four-digit number, captured.
static BARE_YEAR_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+([0-9]{4})\s*$").unwrap());
/// `Regex("""\s+\d{4}\s*$""")` — ws + 4 digits + ws at end.
static TRAILING_YEAR_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+\d{4}\s*$").unwrap());
/// `Regex("[^a-z0-9]+")` — non-alphanumeric runs (input is already lowercased).
static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());

// ── CatalogueTitleIndex search stop words ────────────────────────────────────

const SEARCH_STOP_WORDS: &[&str] = &["the", "and", "for", "with", "from"];

// ── Pure functions ───────────────────────────────────────────────────────────

/// Byte-for-byte port of `TitleMatcher.normalise`:
/// lowercase → strip `(YYYY)` → collapse non-alnum to single spaces → trim →
/// strip trailing bare `YYYY` → trim.
pub fn normalise(value: &str) -> String {
    let s = value.to_lowercase();
    let s = YEAR_TAG.replace_all(&s, "");
    let s = NON_ALNUM.replace_all(&s, " ");
    let s = s.trim();
    let s = TRAILING_YEAR.replace_all(&s, "");
    s.trim().to_string()
}

/// Byte-for-byte port of `TitleMatcher.posterTitle(name, year)`: remove a bracketed year and
/// a provider's ` - YYYY` suffix always, and a bare trailing year only when it is the item's
/// own `year` — otherwise it is part of the title (*Blade Runner 2049*). Falls back to the
/// trimmed input if nothing is left.
pub fn poster_title(name: &str, year: Option<&str>) -> String {
    let trimmed = name.trim();
    let stripped = YEAR_IN_BRACKETS_END.replace_all(trimmed, "");
    let stripped = SEPARATOR_YEAR_END.replace_all(&stripped, "");
    let mut stripped: String = stripped.trim().to_string();
    let own_year = year.map(str::trim).filter(|y| {
        y.len() >= 4 && y.is_char_boundary(4) && y[..4].bytes().all(|b| b.is_ascii_digit())
    });
    if let Some(own) = own_year.map(|y| &y[..4]) {
        let cut = BARE_YEAR_END
            .captures(&stripped)
            .filter(|caps| caps.get(1).map(|m| m.as_str()) == Some(own))
            .and_then(|caps| caps.get(0).map(|m| m.start()));
        if let Some(cut) = cut {
            stripped = stripped[..cut].trim().to_string();
        }
    }
    if stripped.is_empty() {
        trimmed.to_string()
    } else {
        stripped
    }
}

/// Byte-for-byte port of `TitleMatcher.posterYear`:
/// four-digit year from metadata, else parsed from the catalogue title.
///
/// Note: the `yearTag.find(title)?.groupValues?.getOrNull(1)` line is dead code
/// in Kotlin (the `\(\d{4}\)` regex has no capture group 1), faithfully
/// replicated here as a no-op.
pub fn poster_year(metadata_year: Option<&str>, title: &str) -> Option<String> {
    let raw = metadata_year.map(|s| s.trim()).unwrap_or_default();
    if raw.len() >= 4 {
        let four: String = raw.chars().take(4).collect();
        if four.chars().all(|c| c.is_ascii_digit()) {
            return Some(four);
        }
    }
    // Dead-code line: YEAR_TAG has no capture group 1, so this always yields None.
    if let Some(caps) = YEAR_TAG.captures(title) {
        if let Some(g1) = caps.get(1) {
            return Some(g1.as_str().to_string());
        }
    }
    if let Some(m) = TRAILING_YEAR_END.find(title) {
        let trimmed = m.as_str().trim();
        if trimmed.len() == 4 && trimmed.chars().all(|c| c.is_ascii_digit()) {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Byte-for-byte port of `TitleMatcher.cacheLookupKeys`:
/// primary normalised key + with-year variant, deduped, non-blank.
pub fn cache_lookup_keys(name: &str) -> Vec<String> {
    let primary = normalise(name);
    let with_year = {
        let s = name.to_lowercase();
        let s = YEAR_TAG.replace_all(&s, "");
        let s = NON_ALNUM.replace_all(&s, " ");
        s.trim().to_string()
    };
    let mut result = vec![primary, with_year];
    // distinct() preserving order
    let mut seen = std::collections::HashSet::new();
    result.retain(|s| {
        let keep = !s.trim().is_empty() && seen.insert(s.clone());
        keep
    });
    result
}

/// Byte-for-byte port of `CatalogueTitleIndex.titleSearchToken`:
/// distinctive token for bounded SQL LIKE pre-filter (skips articles/stopwords,
/// picks first word >= 3 chars not in stop words, else longest word, else first
/// 4 chars of the normalised title).
pub fn title_search_token(title: &str) -> String {
    let norm = normalise(title);
    let words: Vec<&str> = norm.split(' ').filter(|w| !w.is_empty()).collect();
    let word = words
        .iter()
        .find(|w| w.len() >= 3 && !SEARCH_STOP_WORDS.contains(&w.to_lowercase().as_str()))
        .or_else(|| {
            // Kotlin's maxByOrNull returns the FIRST maximum element;
            // Rust's max_by_key returns the last, so we iterate manually.
            let mut best: Option<&&str> = None;
            for w in &words {
                match best {
                    None => best = Some(w),
                    Some(b) if w.len() > b.len() => best = Some(w),
                    _ => {}
                }
            }
            best
        });
    if let Some(w) = word {
        return (*w).to_string();
    }
    let four: String = norm.chars().take(4).collect();
    if !four.is_empty() {
        four
    } else {
        norm
    }
}

/// Byte-for-byte port of `EditionGroupKeys.movie`:
/// `"$sourceId|${TitleMatcher.normalise(displayName)}|$year"` where year is the
/// first 4 chars of trimmed year (no digit check — differs from series).
pub fn edition_movie_key(source_id: &str, display_name: &str, year: Option<&str>) -> String {
    let y: String = year
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(4).collect())
        .unwrap_or_default();
    format!("{}|{}|{}", source_id, normalise(display_name), y)
}

/// Byte-for-byte port of `EditionGroupKeys.series`:
/// `"$sourceId|${TitleMatcher.normalise(displayName)}|$year"` where year is the
/// first 4 chars only if all 4 are digits, else empty.
pub fn edition_series_key(source_id: &str, display_name: &str, release_date: Option<&str>) -> String {
    let y = series_year_token(release_date);
    format!("{}|{}|{}", source_id, normalise(display_name), y)
}

fn series_year_token(release_date: Option<&str>) -> String {
    let s = release_date.map(|s| s.trim()).unwrap_or_default();
    if s.len() >= 4 {
        let four: String = s.chars().take(4).collect();
        if four.chars().all(|c| c.is_ascii_digit()) {
            return four;
        }
    }
    String::new()
}

// ── Household cache keys (HouseholdContentKey / PressSlateCarry) ─────────────
//
// Two televisions must name the same programme the same way, or Checkpoint Charlie
// and the share server have nothing to hand over. Title+year, not sourceId.
// Fingerprints use Java's `String.hashCode` (31 * h + UTF-16 unit) so the Kotlin
// fallback and this engine collide when one box has the .so and the other does not.

/// Java `String.hashCode` over UTF-16 code units, returned as unsigned decimal.
fn java_unsigned_string_hash(s: &str) -> String {
    let mut h: i32 = 0;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    (h as u32).to_string()
}

/// Byte-for-byte port of `HouseholdContentKey.titleYear`.
pub fn title_year(title: &str, year: Option<&str>) -> String {
    let t = title.trim().to_lowercase();
    let y: String = year.unwrap_or("").trim().chars().take(4).collect();
    if y.is_empty() {
        t
    } else {
        format!("{t}|{y}")
    }
}

/// Byte-for-byte port of `HouseholdContentKey.titlesFingerprint`.
/// `titles_json` is a JSON array of strings.
pub fn titles_fingerprint_json(titles_json: &str) -> String {
    let titles: Vec<String> = serde_json::from_str(titles_json).unwrap_or_default();
    titles_fingerprint(titles.iter().map(|s| s.as_str()))
}

pub fn titles_fingerprint<'a, I>(titles: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let mut v: Vec<String> = titles
        .into_iter()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    v.sort();
    if v.is_empty() {
        return "empty".to_string();
    }
    java_unsigned_string_hash(&v.join("\u{0001}"))
}

/// Byte-for-byte port of `PressSlateCarry.factSignature`.
pub fn fact_signature_json(ids_json: &str) -> String {
    let ids: Vec<String> = serde_json::from_str(ids_json).unwrap_or_default();
    fact_signature(ids.iter().map(|s| s.as_str()))
}

pub fn fact_signature<'a, I>(ids: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let mut v: Vec<String> = ids
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    v.sort();
    if v.is_empty() {
        return String::new();
    }
    java_unsigned_string_hash(&v.join("|"))
}

// ── Screen copy (Kotlin `ScreenCopy`) ────────────────────────────────────────
//
// Facts already on the television. No JNI: Charlie does not share these strings,
// and a native hop on the D-pad path would cost the floor. Cargo tests hold the
// twin so a Kotlin edit cannot drift unnoticed.

pub fn radio_tooltip(station_name: &str, programme_name: Option<&str>) -> Option<String> {
    let station = station_name.trim();
    let programme = programme_name
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    if station.is_empty() && programme.is_none() {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    if !station.is_empty() {
        parts.push(station);
    }
    if let Some(p) = programme {
        parts.push(p);
    }
    let joined = parts.join(" — ");
    let taken: String = joined.chars().take(80).collect();
    if taken.trim().is_empty() {
        None
    } else {
        Some(taken)
    }
}

pub fn conflict_advice(
    requested_title: &str,
    requested_channel: &str,
    conflicting_title: &str,
    conflicting_channel: &str,
) -> String {
    let want = {
        let t = requested_title.trim();
        if t.is_empty() {
            "That programme"
        } else {
            t
        }
    };
    let want_ch = {
        let t = requested_channel.trim();
        if t.is_empty() {
            "this channel"
        } else {
            t
        }
    };
    let have = {
        let t = conflicting_title.trim();
        if t.is_empty() {
            "another recording"
        } else {
            t
        }
    };
    let have_ch = {
        let t = conflicting_channel.trim();
        if t.is_empty() {
            "another channel"
        } else {
            t
        }
    };
    format!("{want} on {want_ch} overlaps {have} on {have_ch}. Pick which to keep.")
}

pub fn recording_summary(synopsis: Option<&str>) -> Option<String> {
    let raw = synopsis.map(|s| s.trim()).filter(|s| !s.is_empty())?;
    let first = raw
        .split(|c| c == '.' || c == '!' || c == '?')
        .next()
        .unwrap_or(raw)
        .trim();
    let first = if first.is_empty() { raw } else { first };
    let words: Vec<&str> = first.split_whitespace().filter(|w| !w.is_empty()).take(25).collect();
    let out = words.join(" ");
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub struct RadioMood {
    pub title: String,
    pub description: String,
    pub search_tags: Vec<String>,
}

pub fn radio_mood(time_of_day: &str, recent_names: &[&str], recent_tags: &[&str]) -> Option<RadioMood> {
    if recent_names.is_empty() && recent_tags.is_empty() {
        return None;
    }
    let mut tags: Vec<String> = Vec::new();
    for raw in recent_tags {
        for part in raw.split(|c| c == ',' || c == ' ' || c == ';' || c == '/') {
            let t = part.trim().to_lowercase();
            if t.len() >= 2 && !tags.contains(&t) {
                tags.push(t);
            }
            if tags.len() == 3 {
                break;
            }
        }
        if tags.len() == 3 {
            break;
        }
    }
    let names: Vec<&str> = recent_names
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .take(3)
        .collect();
    let slot = {
        let t = time_of_day.trim();
        if t.is_empty() {
            "Now"
        } else {
            t
        }
    };
    let desc_src = if !names.is_empty() {
        format!("From {}", names.join(", "))
    } else {
        slot.to_string()
    };
    let description: String = desc_src.chars().take(120).collect();
    let search_tags = if tags.is_empty() {
        vec![slot.to_lowercase()]
    } else {
        tags
    };
    Some(RadioMood {
        title: format!("{slot} radio"),
        description,
        search_tags,
    })
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
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativeNormalise<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let s = read_jstring(env, &input);
        make_jstring(env, &normalise(&s))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI. `year` is empty when the item has none.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativePosterTitleForYear<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input: JString<'local>,
    year: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let s = read_jstring(env, &input);
        let y = read_jstring(env, &year);
        let y_opt = if y.is_empty() { None } else { Some(y.as_str()) };
        make_jstring(env, &poster_title(&s, y_opt))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI. Returns empty string when the year is null.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativePosterYear<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    metadata_year: JString<'local>,
    title: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let my = read_jstring(env, &metadata_year);
        let my_opt = if my.is_empty() { None } else { Some(my.as_str()) };
        let title_s = read_jstring(env, &title);
        let result = poster_year(my_opt, &title_s).unwrap_or_default();
        make_jstring(env, &result)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI. Returns a JSON array string.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativeCacheLookupKeys<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    name: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let s = read_jstring(env, &name);
        let keys = cache_lookup_keys(&s);
        let json = serde_json::to_string(&keys).unwrap_or_else(|_| "[]".to_string());
        make_jstring(env, &json)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativeTitleSearchToken<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    title: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let s = read_jstring(env, &title);
        make_jstring(env, &title_search_token(&s))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI. Empty year string = null.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativeEditionMovieKey<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    source_id: JString<'local>,
    display_name: JString<'local>,
    year: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let sid = read_jstring(env, &source_id);
        let dn = read_jstring(env, &display_name);
        let y = read_jstring(env, &year);
        let y_opt = if y.is_empty() { None } else { Some(y.as_str()) };
        make_jstring(env, &edition_movie_key(&sid, &dn, y_opt))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI. Empty releaseDate string = null.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativeEditionSeriesKey<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    source_id: JString<'local>,
    display_name: JString<'local>,
    release_date: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let sid = read_jstring(env, &source_id);
        let dn = read_jstring(env, &display_name);
        let rd = read_jstring(env, &release_date);
        let rd_opt = if rd.is_empty() { None } else { Some(rd.as_str()) };
        make_jstring(env, &edition_series_key(&sid, &dn, rd_opt))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativeEngineVersion<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
) -> jint {
    ENGINE_VERSION
}

/// # Safety
/// Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativeTitleYear<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    title: JString<'local>,
    year: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let t = read_jstring(env, &title);
        let y = read_jstring(env, &year);
        let y_opt = if y.is_empty() { None } else { Some(y.as_str()) };
        make_jstring(env, &title_year(&t, y_opt))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI. [titles_json] is a JSON array of strings.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativeTitlesFingerprint<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    titles_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let json = read_jstring(env, &titles_json);
        make_jstring(env, &titles_fingerprint_json(&json))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI. [ids_json] is a JSON array of strings.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_TitleMatcherNativeBridge_nativeFactSignature<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    ids_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let json = read_jstring(env, &ids_json);
        make_jstring(env, &fact_signature_json(&json))
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalise ────────────────────────────────────────────────────────────

    #[test]
    fn normalise_strips_year_tag_and_collapses_non_alnum() {
        assert_eq!(normalise("Sherlock-Holmes (2009)"), "sherlock holmes");
        assert_eq!(normalise("sherlock holmes"), "sherlock holmes");
    }

    #[test]
    fn normalise_strips_trailing_bare_year() {
        assert_eq!(normalise("Dune 2021"), "dune");
        assert_eq!(normalise("Inception 2010"), "inception");
    }

    #[test]
    fn normalise_handles_empty_and_punctuation() {
        assert_eq!(normalise(""), "");
        assert_eq!(normalise("!!!"), "");
        assert_eq!(normalise("A.I. Artificial Intelligence"), "a i artificial intelligence");
    }

    #[test]
    fn normalise_lowercases_and_collapses_multiple_separators() {
        assert_eq!(normalise("THE  DARK___KNIGHT"), "the dark knight");
    }

    // ── poster_title ─────────────────────────────────────────────────────────

    // Mirrors app/src/test/.../TitleMatcherPosterTitleTest.kt — the device runs this twin.

    #[test]
    fn poster_title_strips_a_bracketed_year() {
        assert_eq!(poster_title("Sherlock Holmes (2009)", None), "Sherlock Holmes");
        assert_eq!(poster_title("Sherlock Holmes [2009]", None), "Sherlock Holmes");
    }

    #[test]
    fn poster_title_strips_a_providers_separator_year_and_its_dash() {
        assert_eq!(poster_title("Moana - 2026", None), "Moana");
        assert_eq!(poster_title("Moana – 2026", None), "Moana");
        assert_eq!(poster_title("50 First Dates - 2004", Some("2004")), "50 First Dates");
    }

    #[test]
    fn poster_title_strips_a_bare_year_only_when_it_is_the_items_own() {
        assert_eq!(poster_title("Dune 2021", Some("2021")), "Dune");
        assert_eq!(poster_title("Dune 2021", Some("2021-10-22")), "Dune");
        assert_eq!(poster_title("Dune 2021", None), "Dune 2021");
    }

    #[test]
    fn poster_title_keeps_a_number_that_is_part_of_the_title() {
        assert_eq!(poster_title("Blade Runner 2049", Some("2017")), "Blade Runner 2049");
        assert_eq!(poster_title("Death Race 2000", Some("1975")), "Death Race 2000");
        assert_eq!(poster_title("Class of 1984", Some("1982")), "Class of 1984");
        assert_eq!(poster_title("Blade Runner 2049 (2017)", Some("2017")), "Blade Runner 2049");
    }

    #[test]
    fn poster_title_falls_back_to_trimmed_when_blank() {
        assert_eq!(poster_title("  (2009)  ", None), "(2009)");
        assert_eq!(poster_title("  2021  ", Some("2021")), "2021");
    }

    #[test]
    fn poster_title_preserves_title_without_year() {
        assert_eq!(poster_title("The Matrix", Some("1999")), "The Matrix");
    }

    // ── poster_year ──────────────────────────────────────────────────────────

    #[test]
    fn poster_year_uses_metadata_year_when_valid() {
        assert_eq!(poster_year(Some("2009-05-01"), "Sherlock Holmes"), Some("2009".to_string()));
        assert_eq!(poster_year(Some("2021"), "Dune"), Some("2021".to_string()));
    }

    #[test]
    fn poster_year_ignores_non_digit_metadata_year() {
        assert_eq!(poster_year(Some("abcd"), "Dune 2021"), Some("2021".to_string()));
    }

    #[test]
    fn poster_year_extracts_trailing_bare_year_from_title() {
        assert_eq!(poster_year(None, "Dune 2021"), Some("2021".to_string()));
        assert_eq!(poster_year(None, "Inception 2010"), Some("2010".to_string()));
    }

    #[test]
    fn poster_year_returns_none_when_no_year_anywhere() {
        assert_eq!(poster_year(None, "The Matrix"), None);
        assert_eq!(poster_year(Some(""), "The Matrix"), None);
    }

    #[test]
    fn poster_year_metadata_short_string_falls_through() {
        assert_eq!(poster_year(Some("20"), "Dune 2021"), Some("2021".to_string()));
    }

    // ── cache_lookup_keys ────────────────────────────────────────────────────

    #[test]
    fn cache_lookup_keys_primary_and_with_year() {
        let keys = cache_lookup_keys("Sherlock Holmes 2009");
        assert_eq!(keys, vec!["sherlock holmes".to_string(), "sherlock holmes 2009".to_string()]);
    }

    #[test]
    fn cache_lookup_keys_dedupes_when_no_trailing_year() {
        let keys = cache_lookup_keys("Sherlock Holmes");
        assert_eq!(keys, vec!["sherlock holmes".to_string()]);
    }

    #[test]
    fn cache_lookup_keys_strips_year_tag_in_both_variants() {
        let keys = cache_lookup_keys("Dune (2021)");
        assert_eq!(keys, vec!["dune".to_string()]);
    }

    #[test]
    fn cache_lookup_keys_empty_string_returns_empty_vec() {
        assert!(cache_lookup_keys("").is_empty());
    }

    // ── title_search_token ───────────────────────────────────────────────────

    #[test]
    fn title_search_token_picks_first_long_word_skipping_stopwords() {
        assert_eq!(title_search_token("The Lord of the Rings"), "lord");
    }

    #[test]
    fn title_search_token_picks_longest_when_all_short_or_stopwords() {
        assert_eq!(title_search_token("the and for"), "the");
    }

    #[test]
    fn title_search_token_falls_back_to_first_four_chars() {
        assert_eq!(title_search_token(""), "");
        assert_eq!(title_search_token("ab"), "ab");
    }

    #[test]
    fn title_search_token_skips_articles() {
        assert_eq!(title_search_token("A Beautiful Mind"), "beautiful");
    }

    // ── edition_movie_key ────────────────────────────────────────────────────

    #[test]
    fn edition_movie_key_with_year() {
        assert_eq!(
            edition_movie_key("emby", "Dune (2021)", Some("2021")),
            "emby|dune|2021"
        );
    }

    #[test]
    fn edition_movie_key_null_year() {
        assert_eq!(
            edition_movie_key("emby", "Dune", None),
            "emby|dune|"
        );
    }

    #[test]
    fn edition_movie_key_takes_first_four_chars_no_digit_check() {
        // movie() does NOT check digits — differs from series()
        assert_eq!(
            edition_movie_key("emby", "Dune", Some("abcd-efgh")),
            "emby|dune|abcd"
        );
    }

    // ── edition_series_key ───────────────────────────────────────────────────

    #[test]
    fn edition_series_key_with_release_date() {
        assert_eq!(
            edition_series_key("emby", "Breaking Bad", Some("2008-01-20")),
            "emby|breaking bad|2008"
        );
    }

    #[test]
    fn edition_series_key_null_release_date() {
        assert_eq!(
            edition_series_key("emby", "Breaking Bad", None),
            "emby|breaking bad|"
        );
    }

    #[test]
    fn edition_series_key_non_digit_date_yields_empty_year() {
        assert_eq!(
            edition_series_key("emby", "Breaking Bad", Some("abcd")),
            "emby|breaking bad|"
        );
    }

    // ── engine version ───────────────────────────────────────────────────────

    #[test]
    fn engine_version_is_stable() {
        // Must equal TitleMatcherNativeBridge.EXPECTED_ENGINE_VERSION on the Kotlin side.
        assert_eq!(ENGINE_VERSION, 2);
    }

    #[test]
    fn title_year_strips_container_ids() {
        assert_eq!(title_year("  HEAT  ", Some("1995-06-01")), "heat|1995");
        assert_eq!(title_year("Heat", None), "heat");
    }

    #[test]
    fn titles_fingerprint_matches_java_unsigned_string_hash() {
        assert_eq!(
            titles_fingerprint(["EastEnders", "Match of the Day"]),
            "2353380728"
        );
        assert_eq!(
            titles_fingerprint(["Match of the Day", "EastEnders", "  "]),
            "2353380728"
        );
        assert_eq!(titles_fingerprint(Vec::<&str>::new()), "empty");
    }

    #[test]
    fn fact_signature_is_order_stable_and_java_hashed() {
        assert_eq!(fact_signature(["b", "a"]), "97159");
        assert_eq!(fact_signature(["a", "b"]), "97159");
        assert_eq!(fact_signature(Vec::<&str>::new()), "");
    }

    #[test]
    fn radio_tooltip_is_station_and_programme() {
        assert_eq!(
            radio_tooltip("Jazz FM", Some("Late Lounge")).as_deref(),
            Some("Jazz FM — Late Lounge")
        );
        assert_eq!(radio_tooltip("Jazz FM", Some("  ")).as_deref(), Some("Jazz FM"));
        assert_eq!(radio_tooltip("  ", None), None);
    }

    #[test]
    fn conflict_advice_names_both_recordings() {
        let line = conflict_advice("Match of the Day", "BBC One", "EastEnders", "BBC One");
        assert!(line.contains("Match of the Day"));
        assert!(line.contains("EastEnders"));
        assert!(line.contains("Pick which to keep"));
    }

    #[test]
    fn recording_summary_is_first_sentence() {
        assert_eq!(
            recording_summary(Some(
                "A detective investigates a disappearance in a coastal town. More later."
            ))
            .as_deref(),
            Some("A detective investigates a disappearance in a coastal town")
        );
        assert_eq!(recording_summary(Some("  ")), None);
        assert_eq!(recording_summary(None), None);
    }

    #[test]
    fn radio_mood_is_time_of_day_plus_tags_on_recents() {
        let mood = radio_mood(
            "Evening",
            &["Jazz FM", "Classic FM"],
            &["jazz, piano", "classical"],
        )
        .unwrap();
        assert_eq!(mood.title, "Evening radio");
        assert_eq!(mood.description, "From Jazz FM, Classic FM");
        assert_eq!(mood.search_tags, vec!["jazz", "piano", "classical"]);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
