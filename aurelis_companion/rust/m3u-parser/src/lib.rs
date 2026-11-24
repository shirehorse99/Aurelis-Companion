//! Native M3U playlist parse engine for Aurelis's `core/data` `M3uParser`.
//!
//! Mirrors `M3uParser.kt` line-for-line (the 7 live attribute regexes,
//! `stableUrlDigest` SHA-1 id fallback, M3U-URL credential extraction/
//! injection, `parseInfoLine`'s catchup logic) so this engine and the Kotlin
//! reference produce identical [ChannelOut] rows for the same input. `radio`
//! is confirmed dead code in the Kotlin source (regex defined, never
//! consulted) and is omitted here. `CountryAffinity.detectForChannel` stays
//! Kotlin-side post-processing (see `RustM3uParser.kt`) -- a separate
//! ~40-country keyword table with no performance justification for porting.
//!
//! Owns no long-lived state: one call receives the whole playlist text, the
//! source id, and (when parsing from a fetched URL rather than raw content)
//! the M3U's own URL for credential extraction, matching `M3uParser.parse`/
//! `parseChunked`'s per-call `m3uUrlCredentials`/`m3uUrlHost` instance fields
//! reset in their `finally` blocks.
//!
//! Not wired in as the app's active parser anywhere -- the existing
//! `M3uParser` class remains the only implementation actually selected,
//! mirroring `rust/epg-grid`'s and `rust/xmltv-parser`'s status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::sync::LazyLock;

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseInput {
    pub content: String,
    pub source_id: String,
    /// Present only when parsing a fetched playlist (`parse`/`parseChunked` in Kotlin);
    /// `None` for the pure `parseContent` seam, matching Kotlin's own behavior where
    /// `parseContent` never touches `m3uUrlCredentials`/`m3uUrlHost` at all.
    pub m3u_url: Option<String>,
}

/// Mirrors the subset of `Channel` (core/common/.../model/Channel.kt) this parser
/// populates. `countryCode` is deliberately excluded -- filled in by Kotlin via
/// `CountryAffinity.detectForChannel` after this call returns.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelOut {
    pub id: String,
    pub source_id: String,
    pub name: String,
    pub genre: String,
    pub logo_url: String,
    pub stream_url: String,
    pub has_catchup: bool,
    pub catchup_days: Option<i32>,
    /// `catchup-source=` URL template ("shift"-style catch-up). Mirrors `Channel.catchupSource`.
    pub catchup_source: Option<String>,
    pub playlist_order: i32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseOutput {
    pub channels: Vec<ChannelOut>,
    pub epg_url: Option<String>,
}

// 7 live regexes from M3uParser.kt (RADIO_REGEX is confirmed dead code, omitted).
static TVG_URL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:x-tvg-url|url-tvg)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^"'\s,]+))"#).unwrap()
});
static TVG_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)tvg-id\s*=\s*(?:"([^"]*)"|'([^']*)'|([^"'\s,]+))"#).unwrap()
});
static TVG_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)tvg-name\s*=\s*(?:"([^"]*)"|'([^']*)'|([^"'\s,]+))"#).unwrap()
});
static TVG_LOGO_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:tvg-logo|logo|image)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^"'\s,]+))"#).unwrap()
});
static GROUP_TITLE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:group-title|group)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^"'\s,]+))"#).unwrap()
});
static CATCHUP_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:catchup|tv-archive)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^"'\s,]+))"#).unwrap()
});
static CATCHUP_DAYS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:catchup-days|tv-archive-duration)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^"'\s,]+))"#).unwrap()
});
// "shift"-style catch-up: a per-channel URL template, same shape as Xtream's catchup_source.
// Mirrors M3uParser.kt's CATCHUP_SOURCE_REGEX (18 Jul 2026, c8858a696 -- missed here until 8 Oct).
static CATCHUP_SOURCE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)catchup-source\s*=\s*(?:"([^"]*)"|'([^']*)'|([^"'\s,]+))"#).unwrap()
});
/// What `catchup="shift"` means when the playlist gives no `catchup-source`.
const SHIFT_TEMPLATE: &str = "?utc={utc}&lutc={lutc}";
static CREDS_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:https?|rtsp)://([^:/@]+):([^/@]+)@([^/]+)").unwrap());

/// Mirrors `extractValue` (M3uParser.kt): first non-empty capture group among the
/// double-quoted/single-quoted/unquoted alternatives.
fn extract_value(re: &Regex, line: &str) -> Option<String> {
    let caps = re.captures(line)?;
    caps.get(1)
        .or_else(|| caps.get(2))
        .or_else(|| caps.get(3))
        .map(|m| m.as_str().to_string())
}

/// Mirrors `stableUrlDigest` (M3uParser.kt): SHA-1, first 10 bytes, hex.
fn stable_url_digest(url: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(url.as_bytes());
    let digest = hasher.finalize();
    digest[..10].iter().map(|b| format!("{b:02x}")).collect()
}

/// Mirrors `extractM3uUrlCredentials` (M3uParser.kt): `scheme://user:pass@host...`.
fn extract_m3u_url_credentials(url: &str) -> Option<(String, String, String)> {
    let caps = CREDS_REGEX.captures(url)?;
    Some((
        caps.get(1)?.as_str().to_string(),
        caps.get(2)?.as_str().to_string(),
        caps.get(3)?.as_str().to_string(),
    ))
}

/// Case-insensitive first-occurrence replace, matching Kotlin's
/// `String.replaceFirst(oldValue, newValue, ignoreCase = true)` for the ASCII
/// hostnames this is used on (byte-length-stable under lowercasing).
fn replace_first_ignore_case(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let haystack_lower = haystack.to_lowercase();
    let needle_lower = needle.to_lowercase();
    match haystack_lower.find(&needle_lower) {
        Some(pos) => {
            let end = pos + needle.len();
            format!("{}{}{}", &haystack[..pos], replacement, &haystack[end..])
        }
        None => haystack.to_string(),
    }
}

/// Mirrors `applyM3uCredentialsToStream` (M3uParser.kt). Both wrote a literal `"!!"` after the
/// host until 9 Oct 2026 -- a Kotlin null-assertion typed inside a string template, ported here
/// byte for byte and flagged. Every address from a credentialed playlist had a host ending "!!".
fn apply_m3u_credentials_to_stream(
    stream_url: &str,
    creds: Option<&(String, String, String)>,
) -> String {
    let Some((user, pass, host)) = creds else {
        return stream_url.to_string();
    };
    if host.trim().is_empty() {
        return stream_url.to_string();
    }
    if stream_url.contains('@') {
        return stream_url.to_string();
    }
    let stream_lower = stream_url.to_lowercase();
    let host_lower = host.to_lowercase();
    if !stream_lower.contains(&host_lower) {
        return stream_url.to_string();
    }
    let replacement = format!("{user}:{pass}@{host}");
    replace_first_ignore_case(stream_url, host, &replacement)
}

struct Info {
    name: String,
    tvg_id: String,
    tvg_name: String,
    logo_url: String,
    genre: String,
    has_catchup: bool,
    catchup_days: Option<i32>,
    catchup_source: Option<String>,
}

/// Mirrors `parseInfoLine` (M3uParser.kt).
fn parse_info_line(line: &str) -> Info {
    let name = match line.rfind(',') {
        Some(idx) => line[idx + 1..].trim().to_string(),
        None => line.trim().to_string(),
    };
    let tvg_id = extract_value(&TVG_ID_REGEX, line).unwrap_or_default();
    let tvg_name = extract_value(&TVG_NAME_REGEX, line).unwrap_or_default();
    let logo_url = extract_value(&TVG_LOGO_REGEX, line).unwrap_or_default();
    let genre = extract_value(&GROUP_TITLE_REGEX, line).unwrap_or_else(|| "Uncategorized".to_string());

    let catchup_type = extract_value(&CATCHUP_REGEX, line);
    let catchup_days = extract_value(&CATCHUP_DAYS_REGEX, line).and_then(|s| s.parse::<i32>().ok());
    let normalized_catchup = catchup_type.as_deref().map(|s| s.trim().to_lowercase());
    // A bare catchup="shift" has a template too -- the standard one, added to the channel's own
    // address by XtreamCatchupUrls. Mirrors M3uParser.kt's SHIFT_TEMPLATE.
    let catchup_source = extract_value(&CATCHUP_SOURCE_REGEX, line)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| (normalized_catchup.as_deref() == Some("shift")).then(|| SHIFT_TEMPLATE.to_string()));
    let has_catchup = matches!(
        normalized_catchup.as_deref(),
        Some("1") | Some("true") | Some("default") | Some("append") | Some("shift")
    ) || catchup_source.is_some()
        || catchup_days.unwrap_or(0) > 0;

    Info {
        name,
        tvg_id,
        tvg_name,
        logo_url,
        genre,
        has_catchup,
        catchup_days,
        catchup_source,
    }
}

/// Mirrors `parseWithReader` (M3uParser.kt) end to end.
pub fn parse_m3u(content: &str, source_id: &str, m3u_url: Option<&str>) -> ParseOutput {
    let creds = m3u_url.and_then(extract_m3u_url_credentials);

    let mut channels = Vec::new();
    let mut current_info: Option<Info> = None;
    let mut playlist_order = 0i32;
    let mut epg_url: Option<String> = None;

    for raw_line in content.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("#EXTM3U") {
            epg_url = extract_value(&TVG_URL_REGEX, trimmed);
        } else if trimmed.starts_with("#EXTINF:") {
            current_info = Some(parse_info_line(trimmed));
        } else if trimmed.starts_with("http") || trimmed.starts_with("rtsp") || trimmed.starts_with("rtmp") {
            if let Some(info) = current_info.take() {
                let stable_id = if !info.tvg_id.is_empty() {
                    info.tvg_id.clone()
                } else if !info.tvg_name.is_empty() {
                    info.tvg_name.clone()
                } else {
                    format!("{source_id}:{}", stable_url_digest(trimmed))
                };
                let stream_url_with_auth = apply_m3u_credentials_to_stream(trimmed, creds.as_ref());
                channels.push(ChannelOut {
                    id: stable_id,
                    source_id: source_id.to_string(),
                    name: info.name,
                    genre: info.genre,
                    logo_url: info.logo_url,
                    stream_url: stream_url_with_auth,
                    has_catchup: info.has_catchup,
                    catchup_days: info.catchup_days,
                    catchup_source: info.catchup_source,
                    playlist_order,
                });
                playlist_order += 1;
            }
        }
    }

    ParseOutput { channels, epg_url }
}

fn parse_from_json(input_json: &str) -> String {
    match serde_json::from_str::<ParseInput>(input_json) {
        Ok(input) => {
            let output = parse_m3u(&input.content, &input.source_id, input.m3u_url.as_deref());
            serde_json::to_string(&output)
                .unwrap_or_else(|_| "{\"channels\":[],\"epgUrl\":null}".to_string())
        }
        // Malformed input must never crash a sync -- return an empty result and let
        // RustM3uParser.kt's caller treat it like a parse that found nothing, not a crash.
        Err(_) => "{\"channels\":[],\"epgUrl\":null}".to_string(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_M3uNativeBridge_nativeParseContent(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let input_str: String = match env.get_string(&input_json) {
            Ok(s) => s.into(),
            Err(_) => return Ok(env.new_string("{\"channels\":[],\"epgUrl\":null}").unwrap().into_raw()),
        };
        let output_json = parse_from_json(&input_str);
        env.new_string(output_json)
            .unwrap_or_else(|_| env.new_string("{\"channels\":[],\"epgUrl\":null}").unwrap())
            .into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_M3uNativeBridge_nativeEngineVersion(
    _env: EnvUnowned,
    _class: JClass,
) -> jint {
    ENGINE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(m3u: &str) -> ParseOutput {
        parse_m3u(m3u, "test-source", None)
    }

    // Mirrors M3uParserTest.`parseContent reads a catchup-source template and shift catch-up`.
    // The Kotlin parser learned this on 18 Jul 2026; this twin did not until 8 Oct, so on every
    // ARM box an M3U channel's catch-up template never reached playback.
    #[test]
    fn parse_content_reads_catchup_source_template_and_shift() {
        let m3u = concat!(
            "#EXTM3U\n",
            "#EXTINF:-1 tvg-id=\"bbc1\" catchup=\"shift\" catchup-days=\"7\" catchup-source=\"http://p.example/bbc1/{utc}/{lutc}.m3u8\",BBC One\n",
            "http://stream.com/bbc1\n",
            "#EXTINF:-1 tvg-id=\"itv\" catchup-source=\" http://p.example/itv/{utc} \",ITV\n",
            "http://stream.com/itv\n",
            "#EXTINF:-1 tvg-id=\"c4\" catchup=\"shift\",Channel 4\n",
            "http://stream.com/c4\n",
            "#EXTINF:-1 tvg-id=\"c5\" catchup-source=\"  \",Channel 5\n",
            "http://stream.com/c5\n",
        );
        let ch = parse(m3u).channels;
        assert_eq!(ch[0].catchup_source.as_deref(), Some("http://p.example/bbc1/{utc}/{lutc}.m3u8"));
        assert!(ch[0].has_catchup);
        assert_eq!(ch[0].catchup_days, Some(7));
        assert_eq!(ch[1].catchup_source.as_deref(), Some("http://p.example/itv/{utc}"));
        assert!(ch[1].has_catchup, "a template alone means catch-up");
        assert!(ch[2].has_catchup, "catchup=\"shift\" means catch-up");
        assert_eq!(ch[2].catchup_source.as_deref(), Some("?utc={utc}&lutc={lutc}"), "shift alone gets the standard template");
        assert_eq!(ch[3].catchup_source, None, "a blank template is no template");
        assert!(!ch[3].has_catchup);
    }

    #[test]
    fn catchup_source_is_serialised_for_the_kotlin_wrapper() {
        let out = parse("#EXTINF:-1 tvg-id=\"a\" catchup-source=\"http://x/{utc}\",A\nhttp://s/a\n");
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"catchupSource\":\"http://x/{utc}\""), "{json}");
    }

    // Mirrors M3uParserTest.`parseContent with valid M3U returns channels`
    #[test]
    fn parse_content_with_valid_m3u_returns_channels() {
        let m3u = concat!(
            "#EXTM3U\n",
            "#EXTINF:-1 tvg-id=\"bbc1\" tvg-logo=\"http://logo.com/bbc1.png\" group-title=\"Entertainment\",BBC One\n",
            "http://stream.com/bbc1\n",
            "#EXTINF:-1 tvg-id=\"sky-sports\" group-title=\"Sports\",Sky Sports\n",
            "http://stream.com/skysports\n",
        );
        let output = parse(m3u);
        assert_eq!(output.channels.len(), 2);

        let bbc = &output.channels[0];
        assert_eq!(bbc.id, "bbc1");
        assert_eq!(bbc.name, "BBC One");
        assert_eq!(bbc.logo_url, "http://logo.com/bbc1.png");
        assert_eq!(bbc.genre, "Entertainment");
        assert_eq!(bbc.stream_url, "http://stream.com/bbc1");
        assert_eq!(bbc.source_id, "test-source");

        let sky = &output.channels[1];
        assert_eq!(sky.id, "sky-sports");
        assert_eq!(sky.name, "Sky Sports");
        assert_eq!(sky.genre, "Sports");
    }

    // Mirrors M3uParserTest.`parseContent with missing tags uses defaults`
    #[test]
    fn parse_content_with_missing_tags_uses_defaults() {
        let m3u = "#EXTM3U\n#EXTINF:-1,Generic Channel\nhttp://stream.com/generic\n";
        let output = parse(m3u);
        assert_eq!(output.channels.len(), 1);
        let ch = &output.channels[0];
        assert_eq!(ch.name, "Generic Channel");
        assert!(!ch.id.is_empty());
        assert_eq!(ch.genre, "Uncategorized");
        assert_eq!(ch.logo_url, "");
    }

    // Mirrors M3uParserTest.`parseContent handles case insensitive tags`
    #[test]
    fn parse_content_handles_case_insensitive_tags() {
        let m3u = "#EXTM3U\n#EXTINF:-1 TVG-ID=\"caps-id\" GROUP-TITLE=\"CAPS\",Caps Channel\nhttp://stream.com/caps\n";
        let output = parse(m3u);
        assert_eq!(output.channels.len(), 1);
        assert_eq!(output.channels[0].id, "caps-id");
        assert_eq!(output.channels[0].genre, "CAPS");
    }

    // Mirrors M3uParserTest.`parseContent handles unquoted and spaced attributes`
    #[test]
    fn parse_content_handles_unquoted_and_spaced_attributes() {
        let m3u = "#EXTM3U\n#EXTINF:-1 tvg-id=unquoted-id group-title = \"Spaced Category\" tvg-logo='single-quote.png',Mixed Channel\nhttp://stream.com/mixed\n";
        let output = parse(m3u);
        assert_eq!(output.channels.len(), 1);
        assert_eq!(output.channels[0].id, "unquoted-id");
        assert_eq!(output.channels[0].genre, "Spaced Category");
        assert_eq!(output.channels[0].logo_url, "single-quote.png");
    }

    // Mirrors M3uParserTest.`channels without tvg-id get stable non-empty ids derived from url`
    #[test]
    fn channels_without_tvg_id_get_stable_hex_fallback_ids() {
        let m3u = "#EXTM3U\n#EXTINF:-1,No ID Channel\nhttp://stream.com/noid\n";
        let output = parse(m3u);
        assert_eq!(output.channels.len(), 1);
        let id = &output.channels[0].id;
        assert!(!id.is_empty());
        let hash_part = id.rsplit(':').next().unwrap();
        assert!(hash_part.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    }

    // Mirrors M3uParserTest.`two channels with different urls get different stable ids`
    #[test]
    fn different_urls_get_different_stable_ids() {
        let m3u = "#EXTM3U\n#EXTINF:-1,Channel A\nhttp://stream.com/channelA\n#EXTINF:-1,Channel B\nhttp://stream.com/channelB\n";
        let output = parse(m3u);
        assert_eq!(output.channels.len(), 2);
        assert_ne!(output.channels[0].id, output.channels[1].id);
    }

    // Mirrors M3uParserTest.`same url produces the same stable id on repeated parses`
    #[test]
    fn same_url_produces_deterministic_id() {
        let m3u = "#EXTM3U\n#EXTINF:-1,Repeatable Channel\nhttp://stream.com/stable\n";
        let first = parse(m3u).channels[0].id.clone();
        let second = parse(m3u).channels[0].id.clone();
        assert_eq!(first, second);
    }

    #[test]
    fn credentials_are_injected_when_m3u_url_provided() {
        let m3u = "#EXTM3U\n#EXTINF:-1,Auth Channel\nhttp://host.example.com/stream\n";
        let output = parse_m3u(m3u, "test-source", Some("http://user:pass@host.example.com/playlist.m3u"));
        assert_eq!(output.channels.len(), 1);
        assert_eq!(output.channels[0].stream_url, "http://user:pass@host.example.com/stream");
    }

    #[test]
    fn credentials_are_not_injected_for_parse_content() {
        let m3u = "#EXTM3U\n#EXTINF:-1,No Auth Channel\nhttp://host.example.com/stream\n";
        let output = parse_m3u(m3u, "test-source", None);
        assert_eq!(output.channels[0].stream_url, "http://host.example.com/stream");
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
