//! Native JSON body parse engine for Aurelis's `core/data` `GeneralNewsRepository`.
//!
//! Ports `parseNewsDataIoBody`/`parseTheNewsApiBody` line-for-line: given an
//! already-fetched HTTP response body (as a plain string) plus `nowMs` and a
//! caller-supplied category tag, parses/filters/dedupes headlines exactly as the
//! Kotlin functions do. The HTTP fetch itself, API-quota bookkeeping
//! (`canSpendApiCall`/`recordApiCall`), the on-disk response cache, and
//! `BreakingNewsClassifier.classify`'s breaking-news scoring all stay Kotlin-side --
//! this crate only implements the pure "raw JSON string in, headline list out" step,
//! mirroring `rust/xtream-parser`'s scope for Xtream's JSON payloads.
//!
//! Date parsing (`parseFlexibleDatetime`) is a deliberately simplified fixed-offset
//! reader of the `YYYY-MM-DD[T ]HH:MM:SS` prefix, assuming UTC and ignoring any
//! fractional-second/timezone suffix -- these headlines are only ever used for
//! 24h/72h cutoff filtering and freshness sorting, never displayed with sub-second
//! precision, so the few-hundred-millisecond loss from dropping fractional seconds
//! is immaterial. This mirrors `rust/xmltv-parser`'s icon-URL-encoding simplification.
//!
//! Owns no long-lived state: one call receives one response body and returns the
//! parsed headline list. Not wired in as the app's active parser anywhere --
//! `GeneralNewsRepository`'s own `parseNewsDataIoBody`/`parseTheNewsApiBody` remain
//! the only implementations actually selected, mirroring `rust/xtream-parser`'s
//! dormant status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

const HOUR_MS: i64 = 3_600_000;
const NEWSDATA_WINDOW_MS: i64 = 72 * HOUR_MS;
/// Mirrors `GeneralNewsMixer.WINDOW_MS`.
const THENEWSAPI_WINDOW_MS: i64 = 24 * HOUR_MS;
const NEWSDATA_TAKE: usize = 15;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlineOut {
    pub id: String,
    pub title: String,
    pub url: String,
    pub source_name: String,
    pub category: String,
    pub published_at_ms: i64,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlineListOutput {
    pub headlines: Vec<HeadlineOut>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsDataInput {
    pub body: String,
    pub now_ms: i64,
    pub category_tag: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TheNewsApiInput {
    pub body: String,
    pub now_ms: i64,
}

/// Howard Hinnant's `days_from_civil` (proleptic Gregorian) -- days since 1970-01-01,
/// reused verbatim from `rust/xmltv-parser` to keep this crate dependency-free for dates.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Simplified port of `parseNewsDataPublishedAt` (GeneralNewsRepository.kt): reads the
/// fixed `YYYY-MM-DD[T ]HH:MM:SS` prefix as UTC, ignoring fractional seconds and any
/// timezone suffix (see module doc for why that's immaterial here).
fn parse_flexible_datetime(value: &str) -> Option<i64> {
    let v = value.trim();
    if v.len() < 19 {
        return None;
    }
    let b = v.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year: i64 = v.get(0..4)?.parse().ok()?;
    let month: i64 = v.get(5..7)?.parse().ok()?;
    let day: i64 = v.get(8..10)?.parse().ok()?;
    let hour: i64 = v.get(11..13)?.parse().ok()?;
    let min: i64 = v.get(14..16)?.parse().ok()?;
    let sec: i64 = v.get(17..19)?.parse().ok()?;
    let days = days_from_civil(year, month, day);
    Some(days * 86_400_000 + hour * 3_600_000 + min * 60_000 + sec * 1000)
}

/// Byte-for-byte port of Java's `String.hashCode()` over UTF-16 code units, used to
/// reproduce the Kotlin `.hashCode()` call baked into each headline's stable `id`.
fn java_string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for unit in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(unit as i32);
    }
    h
}

fn normalize_title(title: &str) -> String {
    let mut result = String::with_capacity(title.len());
    let mut in_ws = false;
    for c in title.to_lowercase().chars() {
        if c.is_whitespace() {
            if !in_ws {
                result.push(' ');
                in_ws = true;
            }
        } else {
            result.push(c);
            in_ws = false;
        }
    }
    result
}

fn dedupe_by_normalized_title(items: Vec<HeadlineOut>, take: Option<usize>) -> Vec<HeadlineOut> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let key = normalize_title(&item.title);
        if seen.insert(key) {
            out.push(item);
            if let Some(limit) = take {
                if out.len() >= limit {
                    break;
                }
            }
        }
    }
    out
}

fn opt_str<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(|v| v.as_str())
}

fn opt_str_trimmed_nonblank<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
    opt_str(obj, key).map(str::trim).filter(|s| !s.is_empty())
}

/// Byte-for-byte port of `parseNewsDataIoBody` (GeneralNewsRepository.kt).
pub fn parse_newsdata_io_body(input: &NewsDataInput) -> Vec<HeadlineOut> {
    let root: Value = match serde_json::from_str(&input.body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let results = match root.get("results").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let cutoff = input.now_ms - NEWSDATA_WINDOW_MS;

    let mut out = Vec::new();
    for item in results {
        let Some(title) = opt_str_trimmed_nonblank(item, "title") else { continue };
        let Some(link) = opt_str_trimmed_nonblank(item, "link") else { continue };
        let published_at_ms = opt_str(item, "pubDate")
            .and_then(parse_flexible_datetime)
            .unwrap_or(input.now_ms);
        if !(cutoff..=input.now_ms).contains(&published_at_ms) {
            continue;
        }
        let source_name = opt_str_trimmed_nonblank(item, "source_name")
            .unwrap_or("NewsData.io")
            .to_string();
        // Mirrors org.json's `optString(key, default)`: falls back to `link` only when
        // `article_id` is missing entirely, not when it's present-but-blank.
        let article_id = opt_str(item, "article_id").unwrap_or(link);
        let image_url = opt_str_trimmed_nonblank(item, "image_url").map(String::from);

        out.push(HeadlineOut {
            id: format!("newsdata:{}", java_string_hash(article_id)),
            title: title.to_string(),
            url: link.to_string(),
            source_name,
            category: input.category_tag.clone(),
            published_at_ms,
            image_url,
        });
    }
    dedupe_by_normalized_title(out, Some(NEWSDATA_TAKE))
}

fn thenewsapi_category_tag(cat_str: &str) -> &'static str {
    match cat_str {
        "politics" => "MAJOR_NATIONAL",
        "business" => "NATIONAL",
        "entertainment" => "FUN",
        _ => "MAJOR_COUNTRY",
    }
}

/// Byte-for-byte port of `parseTheNewsApiBody` (GeneralNewsRepository.kt).
pub fn parse_thenewsapi_body(input: &TheNewsApiInput) -> Vec<HeadlineOut> {
    let root: Value = match serde_json::from_str(&input.body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let data = match root.get("data").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let cutoff = input.now_ms - THENEWSAPI_WINDOW_MS;

    let mut out = Vec::new();
    for item in data {
        let Some(title) = opt_str_trimmed_nonblank(item, "title") else { continue };
        let Some(link) = opt_str_trimmed_nonblank(item, "url") else { continue };
        let published_at_ms = opt_str(item, "published_at")
            .and_then(parse_flexible_datetime)
            .unwrap_or(input.now_ms);
        if !(cutoff..=input.now_ms).contains(&published_at_ms) {
            continue;
        }
        let cat_str = item
            .get("categories")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let category_tag = thenewsapi_category_tag(cat_str);
        // No trim here -- mirrors the Kotlin call site, which skips `.trim()` for `source`
        // (unlike every other string field in this function).
        let source_name = opt_str(item, "source")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("TheNewsAPI")
            .to_string();
        let uuid = opt_str(item, "uuid").unwrap_or(link);
        let image_url = opt_str_trimmed_nonblank(item, "image_url").map(String::from);

        out.push(HeadlineOut {
            id: format!("thenewsapi:{}", java_string_hash(uuid)),
            title: title.to_string(),
            url: link.to_string(),
            source_name,
            category: category_tag.to_string(),
            published_at_ms,
            image_url,
        });
    }
    dedupe_by_normalized_title(out, None)
}

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_generalnews_NewsParserNativeBridge_nativeParseNewsDataIoBody<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let raw = jstring_to_string(env, &input_json);
        let output = match serde_json::from_str::<NewsDataInput>(&raw) {
            Ok(input) => HeadlineListOutput { headlines: parse_newsdata_io_body(&input) },
            Err(_) => HeadlineListOutput { headlines: Vec::new() },
        };
        let json = serde_json::to_string(&output).unwrap_or_else(|_| "{\"headlines\":[]}".to_string());
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_generalnews_NewsParserNativeBridge_nativeParseTheNewsApiBody<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let raw = jstring_to_string(env, &input_json);
        let output = match serde_json::from_str::<TheNewsApiInput>(&raw) {
            Ok(input) => HeadlineListOutput { headlines: parse_thenewsapi_body(&input) },
            Err(_) => HeadlineListOutput { headlines: Vec::new() },
        };
        let json = serde_json::to_string(&output).unwrap_or_else(|_| "{\"headlines\":[]}".to_string());
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_generalnews_NewsParserNativeBridge_nativeEngineVersion<
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
    fn parses_newsdata_io_results_within_window() {
        let now_ms = 1_700_100_000_000i64;
        let fresh_time = "2023-11-15 12:00:00";
        let body = format!(
            r#"{{"results":[{{"title":"Budget announced","link":"https://a.example/1","pubDate":"{fresh_time}","source_name":"BBC","article_id":"art1","image_url":"https://a.example/img.jpg"}}]}}"#,
        );
        let input = NewsDataInput { body, now_ms, category_tag: "MAJOR_COUNTRY".to_string() };
        let out = parse_newsdata_io_body(&input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source_name, "BBC");
        assert_eq!(out[0].id, format!("newsdata:{}", java_string_hash("art1")));
    }

    #[test]
    fn drops_newsdata_items_outside_the_72h_window() {
        let now_ms = 1_700_100_000_000i64;
        let stale = "2020-01-01 00:00:00";
        let body = format!(
            r#"{{"results":[{{"title":"Old story","link":"https://a.example/2","pubDate":"{stale}"}}]}}"#,
        );
        let input = NewsDataInput { body, now_ms, category_tag: "MAJOR_COUNTRY".to_string() };
        assert!(parse_newsdata_io_body(&input).is_empty());
    }

    #[test]
    fn newsdata_missing_title_or_link_is_skipped() {
        let now_ms = 1_700_100_000_000i64;
        let body = r#"{"results":[{"title":"","link":"https://a.example/3"},{"title":"No link"}]}"#.to_string();
        let input = NewsDataInput { body, now_ms, category_tag: "MAJOR_COUNTRY".to_string() };
        assert!(parse_newsdata_io_body(&input).is_empty());
    }

    #[test]
    fn newsdata_dedupes_by_normalized_title_and_caps_at_15() {
        let now_ms = 1_700_100_000_000i64;
        let fresh_time = "2023-11-15 12:00:00";
        let mut results = Vec::new();
        for i in 0..20 {
            results.push(format!(
                r#"{{"title":"Story {i}","link":"https://a.example/{i}","pubDate":"{fresh_time}"}}"#
            ));
        }
        // Duplicate of "Story 0" with different whitespace -- must dedupe away.
        results.push(format!(
            r#"{{"title":"Story  0","link":"https://a.example/dup","pubDate":"{fresh_time}"}}"#
        ));
        let body = format!(r#"{{"results":[{}]}}"#, results.join(","));
        let input = NewsDataInput { body, now_ms, category_tag: "MAJOR_COUNTRY".to_string() };
        let out = parse_newsdata_io_body(&input);
        assert_eq!(out.len(), 15);
    }

    #[test]
    fn parses_thenewsapi_body_and_maps_category() {
        let now_ms = 1_700_100_000_000i64;
        let fresh_time = "2023-11-15T11:00:00.000000Z";
        let body = format!(
            r#"{{"data":[{{"title":"Election update","url":"https://b.example/1","published_at":"{fresh_time}","categories":["politics"],"source":"Reuters","uuid":"abc-123"}}]}}"#,
        );
        let input = TheNewsApiInput { body, now_ms };
        let out = parse_thenewsapi_body(&input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].category, "MAJOR_NATIONAL");
        assert_eq!(out[0].source_name, "Reuters");
        assert_eq!(out[0].id, format!("thenewsapi:{}", java_string_hash("abc-123")));
    }

    #[test]
    fn thenewsapi_defaults_category_when_missing() {
        let now_ms = 1_700_100_000_000i64;
        let fresh_time = "2023-11-15T11:00:00Z";
        let body = format!(
            r#"{{"data":[{{"title":"Generic story","url":"https://b.example/2","published_at":"{fresh_time}"}}]}}"#,
        );
        let input = TheNewsApiInput { body, now_ms };
        let out = parse_thenewsapi_body(&input);
        assert_eq!(out[0].category, "MAJOR_COUNTRY");
        assert_eq!(out[0].source_name, "TheNewsAPI");
    }

    #[test]
    fn drops_thenewsapi_items_outside_the_24h_window() {
        let now_ms = 1_700_100_000_000i64;
        let stale = "2020-01-01T00:00:00Z";
        let body = format!(
            r#"{{"data":[{{"title":"Old story","url":"https://b.example/3","published_at":"{stale}"}}]}}"#,
        );
        let input = TheNewsApiInput { body, now_ms };
        assert!(parse_thenewsapi_body(&input).is_empty());
    }

    #[test]
    fn malformed_json_returns_empty_list() {
        let input = NewsDataInput { body: "not json".to_string(), now_ms: 0, category_tag: "MAJOR_COUNTRY".to_string() };
        assert!(parse_newsdata_io_body(&input).is_empty());
    }

    #[test]
    fn parse_flexible_datetime_handles_space_and_t_separators() {
        assert_eq!(parse_flexible_datetime("2023-11-15 12:00:00"), parse_flexible_datetime("2023-11-15T12:00:00"));
    }

    #[test]
    fn java_string_hash_matches_known_values() {
        // Known Java String.hashCode() values for simple ASCII strings.
        assert_eq!(java_string_hash(""), 0);
        assert_eq!(java_string_hash("a"), 97);
        assert_eq!(java_string_hash("abc"), 96354);
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
