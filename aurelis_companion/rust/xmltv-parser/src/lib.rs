//! Native XMLTV EPG parse engine for Aurelis's `core/data` `XmltvParser`.
//!
//! Mirrors `XmltvParser.kt` line-for-line (attribute extraction, the
//! allocation-free `parseTimestampFast` timestamp routine, `sanitizeIconUrl`,
//! `EpgChannelKeys.normalize`) so this engine and the Kotlin reference produce
//! identical `LocalEpgEntry` rows for the same input. Owns no long-lived
//! state: one call receives the whole XML document and source id, and
//! returns a JSON result that Kotlin (`RustXmltvParser.kt`) maps back to real
//! `LocalEpgEntry` objects and chunks for Room exactly as `XmltvParser.kt`
//! does today.
//!
//! Not wired in as the app's active parser anywhere -- the existing
//! `XmltvParser` class remains the only implementation actually selected.
//! This exists so a future profiling pass on real constrained hardware can
//! swap it in behind a flag, mirroring `rust/epg-grid`'s status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// How many programmes carried each fact the guide would like to mark them with.
///
/// The guide wants to say "New series", "episode 3 of 6", "1985 · 15" — and the plan assumed
/// those facts were already being fetched and simply never read, which is this project's
/// signature defect. They are not: BOTH parsers, Kotlin and Rust, read exactly four things from
/// a `<programme>` — title, desc, icon, catchup — and drop everything else on the floor.
///
/// So before writing an ingest, a schema migration and a set of badges for facts that may not
/// exist in these feeds at all, count them. Counting is nearly free (a comparison and an
/// increment per element already being walked), stores nothing, and answers the only question
/// that matters: does the data exist? A badge built on a tag no provider sends would be a
/// permanently empty feature, and absence beats fiction.
pub static SEEN_PROGRAMMES: AtomicU64 = AtomicU64::new(0);
pub static SEEN_EPISODE_NUM: AtomicU64 = AtomicU64::new(0);
pub static SEEN_DATE: AtomicU64 = AtomicU64::new(0);
pub static SEEN_RATING: AtomicU64 = AtomicU64::new(0);
pub static SEEN_STAR_RATING: AtomicU64 = AtomicU64::new(0);
pub static SEEN_NEW: AtomicU64 = AtomicU64::new(0);
pub static SEEN_PREMIERE: AtomicU64 = AtomicU64::new(0);
pub static SEEN_SUB_TITLE: AtomicU64 = AtomicU64::new(0);
pub static SEEN_CATEGORY: AtomicU64 = AtomicU64::new(0);

/// Bumps the tally for one child element name. Called for both Start and Empty events.
fn tally_child(name: &str) {
    let counter = match name {
        "episode-num" => &SEEN_EPISODE_NUM,
        "date" => &SEEN_DATE,
        "rating" => &SEEN_RATING,
        "star-rating" => &SEEN_STAR_RATING,
        "new" => &SEEN_NEW,
        "premiere" => &SEEN_PREMIERE,
        "sub-title" => &SEEN_SUB_TITLE,
        "category" => &SEEN_CATEGORY,
        _ => return,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// "programmes 108008; episode-num 0; date 0; rating 0; …" — the yield, for the log.
pub fn field_tally() -> String {
    format!(
        "programmes {}; episode-num {}; date {}; rating {}; star-rating {}; new {}; \
premiere {}; sub-title {}; category {}",
        SEEN_PROGRAMMES.load(Ordering::Relaxed),
        SEEN_EPISODE_NUM.load(Ordering::Relaxed),
        SEEN_DATE.load(Ordering::Relaxed),
        SEEN_RATING.load(Ordering::Relaxed),
        SEEN_STAR_RATING.load(Ordering::Relaxed),
        SEEN_NEW.load(Ordering::Relaxed),
        SEEN_PREMIERE.load(Ordering::Relaxed),
        SEEN_SUB_TITLE.load(Ordering::Relaxed),
        SEEN_CATEGORY.load(Ordering::Relaxed),
    )
}

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

/// SHA-256 (64 lowercase hex chars) of this file's source as compiled, written by `build.rs`.
/// Two builds with the same value parse identically; any edit to this file changes it.
pub const LOGIC_VERSION: &str = env!("XMLTV_PARSER_LOGIC");

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseInput {
    pub xml: String,
    pub source_id: String,
}

/// Mirrors `LocalEpgEntry` (core/common/.../data/local/LocalEpgEntry.kt) field-for-field.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryOut {
    pub id: String,
    pub channel_id: String,
    pub source_id: String,
    pub title: String,
    pub description: Option<String>,
    pub image_url: Option<String>,
    pub start_time: i64,
    pub end_time: i64,
    pub has_catchup: bool,
    pub episode_number: Option<String>,
    pub airing_kind: Option<&'static str>,
    pub episode_title: Option<String>,
    pub programme_year: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseOutput {
    pub entries: Vec<EntryOut>,
    pub channel_icons: HashMap<String, String>,
}

/// Mirrors `EpgChannelKeys.normalize` (core/common/.../EpgChannelKeys.kt).
fn normalize_channel_key(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed
        .to_lowercase()
        .replace("&amp;", "&")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Howard Hinnant's `days_from_civil` (proleptic Gregorian) -- days since 1970-01-01
/// for a 1-indexed (year, month, day). Used instead of a date/time crate to keep this
/// crate's dependency footprint minimal, mirroring the Kotlin routine's own
/// "allocation-free, no SimpleDateFormat" intent.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Port of `stripGluedNewMarker` (XmltvParser.kt): one provider glues a "new episode" marker onto
/// the title ("NewClickbait Clinic"). "New" run straight into a capital letter is dropped; titles
/// such as "Newsnight", "NewsWatch" and "New Tricks" are left alone.
fn strip_glued_new_marker(title: String) -> String {
    let mut chars = title.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    match chars.next() {
        Some(c) if prefix == "New" && c.is_uppercase() => title[3..].to_string(),
        _ => title,
    }
}

/// Byte-for-byte port of `parseTimestampFast` (XmltvParser.kt). Format:
/// `yyyyMMddHHmmss [+/-HHMM]`. The offset sign math below looks inverted from
/// normal UTC-offset convention -- that's the existing, tested Kotlin
/// behavior, preserved exactly rather than "fixed" during this port.
fn parse_timestamp_fast(raw: &str) -> Option<i64> {
    let t = raw.trim();
    if t.len() < 14 {
        return None;
    }
    let year: i64 = t.get(0..4)?.parse().ok()?;
    let month: i64 = t.get(4..6)?.parse().ok()?;
    let day: i64 = t.get(6..8)?.parse().ok()?;
    let hour: i64 = t.get(8..10)?.parse().ok()?;
    let min: i64 = t.get(10..12)?.parse().ok()?;
    let sec: i64 = t.get(12..14)?.parse().ok()?;

    let days = days_from_civil(year, month, day);
    let mut millis = days * 86_400_000 + hour * 3_600_000 + min * 60_000 + sec * 1000;

    if t.len() >= 20 {
        let bytes = t.as_bytes();
        let sign_char = *bytes.get(15)?;
        let sign: i64 = if sign_char == b'+' { -1 } else { 1 };
        let off_hour: i64 = t.get(16..18)?.parse().ok()?;
        let off_min: i64 = t.get(18..20)?.parse().ok()?;
        millis += sign * (off_hour * 3_600_000 + off_min * 60_000);
    }
    Some(millis)
}

/// Byte-for-byte port of `sanitizeIconUrl` (XmltvParser.kt), minus the final
/// `URI.toASCIIString()` percent-encoding step for non-ASCII characters --
/// real-world icon URLs are ASCII already, and adding a `url`/IDNA crate just
/// for that one edge case isn't worth the dependency, matching epg-grid's
/// minimal-deps style. Known, deliberate simplification, not an oversight.
fn sanitize_icon_url(raw: Option<&str>) -> Option<String> {
    let trimmed = raw
        .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\''))
        .unwrap_or("");
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case("data:") {
        return None;
    }
    let with_scheme = if let Some(rest) = trimmed.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        trimmed.to_string()
    };
    let candidate = with_scheme.replace("&amp;", "&").replace(' ', "%20");

    let scheme_end = candidate.find("://")?;
    let scheme = candidate[..scheme_end].to_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = &candidate[scheme_end + 3..];
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.trim().is_empty() {
        return None;
    }
    Some(candidate)
}

fn get_attr(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes().filter_map(|a| a.ok()).find_map(|a| {
        if a.key.as_ref() == name {
            a.unescape_value().ok().map(|c| c.into_owned())
        } else {
            None
        }
    })
}

/// Reads all text/CDATA content of a leaf element (title/desc), stopping at
/// its own End event, then trims -- mirrors `readText` for the common
/// flat-text case real XMLTV always uses.
fn read_text(reader: &mut Reader<&[u8]>) -> String {
    let mut buf = Vec::new();
    let mut text = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(e)) => {
                let raw = e.into_inner().into_owned();
                match quick_xml::escape::unescape(&raw) {
                    Ok(s) => text.push_str(s.as_ref()),
                    Err(_) => text.push_str(&raw),
                }
            }
            Ok(Event::CData(e)) => {
                text.push_str(&e.into_inner());
            }
            // quick-xml 0.42 reports `&amp;`, `&#233;` etc. as their own events, never inside
            // Event::Text. Dropping them lost every ampersand, apostrophe and accent.
            Ok(Event::GeneralRef(r)) => {
                let name = r.as_ref().to_string();
                match resolve_reference(&name) {
                    Some(c) => text.push(c),
                    None => {
                        text.push('&');
                        text.push_str(&name);
                        text.push(';');
                    }
                }
            }
            Ok(Event::End(_)) | Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    text.trim().to_string()
}

/// Resolves the text between `&` and `;`: the five predefined entities and decimal/hex
/// character references. `None` for anything else (unknown name, invalid code point), which
/// the caller keeps literally.
fn resolve_reference(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let digits = name.strip_prefix('#')?;
            let code = match digits.strip_prefix('x').or_else(|| digits.strip_prefix('X')) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse::<u32>().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Mirrors the `<channel>` child walk in `parseChannelElement`: last valid
/// `<icon src>` wins.
fn parse_channel_children(reader: &mut Reader<&[u8]>) -> String {
    let mut icon_url = String::new();
    let mut buf = Vec::new();
    let mut depth = 1i32;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                depth += 1;
                if e.name().as_ref() == "icon" {
                    if let Some(src) = get_attr(&e, "src") {
                        if let Some(s) = sanitize_icon_url(Some(&src)) {
                            icon_url = s;
                        }
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                if e.name().as_ref() == "icon" {
                    if let Some(src) = get_attr(&e, "src") {
                        if let Some(s) = sanitize_icon_url(Some(&src)) {
                            icon_url = s;
                        }
                    }
                }
            }
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    icon_url
}

/// Mirrors the `<programme>` child walk in `parseProgrammeElement`: last
/// valid `<title>`/`<desc>`/`<icon>`/`<catchup>` wins.
///
/// `<previously-shown>` deliberately does NOT set `has_catchup`. It is XMLTV's
/// *repeat* marker — "this has aired before" — and says nothing about whether the
/// provider's archive can serve it back. Treating it as catch-up would light the
/// Catch Up button on channels with no archive at all. Availability comes from the
/// channel (Xtream `tv_archive` / M3U `catchup=`), resolved onto
/// `Channel.hasCatchup` by `ChannelArbiter`. Kotlin twin: `XmltvParser`.
#[derive(Default)]
struct ProgrammeChildren {
    title: String,
    description: String,
    image_url: String,
    has_catchup: bool,
    episode_number: Option<String>,
    airing_kind: Option<&'static str>,
    episode_title: Option<String>,
    programme_year: Option<String>,
}

fn parse_programme_children(reader: &mut Reader<&[u8]>) -> ProgrammeChildren {
    let mut title = String::new();
    let mut description = String::new();
    let mut image_url = String::new();
    let mut has_catchup = false;
    let mut episode_number = None;
    let mut airing_kind = None;
    let mut episode_title = None;
    let mut programme_year = None;
    let mut buf = Vec::new();
    let mut depth = 1i32;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = e.name();
                match local.as_ref() {
                    "title" => {
                        title = strip_glued_new_marker(read_text(reader));
                    }
                    "desc" => {
                        description = read_text(reader);
                    }
                    "date" => {
                        tally_child("date");
                        let date = read_text(reader);
                        let date = date.trim();
                        programme_year = date.get(..4).filter(|year| {
                            matches!(date.len(), 4 | 6 | 8)
                                && date.bytes().all(|byte| byte.is_ascii_digit())
                                && (year.starts_with("19") || year.starts_with("20"))
                        }).map(str::to_owned);
                    }
                    "episode-num" => {
                        tally_child("episode-num");
                        let system = get_attr(&e, "system");
                        let number = read_text(reader);
                        if system.as_deref() == Some("xmltv_ns") && !number.trim().is_empty() {
                            episode_number = Some(number.trim().to_string());
                        }
                    }
                    "sub-title" => {
                        tally_child("sub-title");
                        let subtitle = read_text(reader);
                        if !subtitle.trim().is_empty() {
                            episode_title = Some(subtitle.trim().to_string());
                        }
                    }
                    "icon" => {
                        depth += 1;
                        if let Some(src) = get_attr(&e, "src") {
                            if let Some(s) = sanitize_icon_url(Some(&src)) {
                                image_url = s;
                            }
                        }
                    }
                    "catchup" => {
                        depth += 1;
                        let enabled = get_attr(&e, "enabled");
                        has_catchup = matches!(
                            enabled.as_deref(),
                            Some(v) if v.eq_ignore_ascii_case("yes") || v == "1"
                        );
                    }
                    // See the note above: a repeat marker is not an archive.
                    "previously-shown" => {
                        depth += 1;
                        if airing_kind != Some("new") {
                            airing_kind = Some("repeat");
                        }
                    }
                    "new" => {
                        tally_child("new");
                        depth += 1;
                        airing_kind = Some("new");
                    }
                    other => {
                        tally_child(other);
                        depth += 1;
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                let local = e.name();
                match local.as_ref() {
                    "icon" => {
                        if let Some(src) = get_attr(&e, "src") {
                            if let Some(s) = sanitize_icon_url(Some(&src)) {
                                image_url = s;
                            }
                        }
                    }
                    "catchup" => {
                        let enabled = get_attr(&e, "enabled");
                        has_catchup = matches!(
                            enabled.as_deref(),
                            Some(v) if v.eq_ignore_ascii_case("yes") || v == "1"
                        );
                    }
                    // See the note above: a repeat marker is not an archive.
                    "previously-shown" => {
                        if airing_kind != Some("new") {
                            airing_kind = Some("repeat");
                        }
                    }
                    "new" => {
                        tally_child("new");
                        airing_kind = Some("new");
                    }
                    other => tally_child(other),
                }
            }
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    ProgrammeChildren {
        title,
        description,
        image_url,
        has_catchup,
        episode_number,
        airing_kind,
        episode_title,
        programme_year,
    }
}

/// Mirrors `XmltvParser.parseChunked`'s document walk end to end (minus
/// chunking, which stays a Kotlin-side concern over the returned list).
pub fn parse_xmltv(xml: &str, source_id: &str) -> ParseOutput {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut entries = Vec::new();
    let mut channel_icons: HashMap<String, String> = HashMap::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Err(_) => break,
            Ok(Event::Start(e)) => {
                let local = e.name();
                match local.as_ref() {
                    "channel" => {
                        if let Some(channel_id) = get_attr(&e, "id") {
                            let normalized = normalize_channel_key(&channel_id);
                            if !normalized.is_empty() {
                                let icon_url = parse_channel_children(&mut reader);
                                if !icon_url.trim().is_empty() {
                                    channel_icons.insert(normalized, icon_url);
                                }
                            }
                        }
                    }
                    "programme" => {
                        SEEN_PROGRAMMES.fetch_add(1, Ordering::Relaxed);
                        let channel_id = get_attr(&e, "channel");
                        let start_raw = get_attr(&e, "start");
                        let stop_raw = get_attr(&e, "stop");
                        if let (Some(channel_id), Some(start_raw), Some(stop_raw)) =
                            (channel_id, start_raw, stop_raw)
                        {
                            let start_ms = parse_timestamp_fast(&start_raw);
                            let end_ms = parse_timestamp_fast(&stop_raw);
                            if let (Some(start_ms), Some(end_ms)) = (start_ms, end_ms) {
                                if end_ms > start_ms {
                                    let ProgrammeChildren {
                                        title,
                                        description,
                                        image_url,
                                        has_catchup,
                                        episode_number,
                                        airing_kind,
                                        episode_title,
                                        programme_year,
                                    } = parse_programme_children(&mut reader);
                                    if !title.trim().is_empty() {
                                        let normalized_channel_id =
                                            normalize_channel_key(&channel_id);
                                        let resolved_image = if !image_url.trim().is_empty() {
                                            Some(image_url)
                                        } else {
                                            channel_icons.get(&normalized_channel_id).cloned()
                                        }
                                        .filter(|s| !s.trim().is_empty());
                                        entries.push(EntryOut {
                                            id: format!(
                                                "{source_id}:{channel_id}:{start_ms}"
                                            ),
                                            channel_id,
                                            source_id: source_id.to_string(),
                                            title,
                                            description: if description.trim().is_empty() {
                                                None
                                            } else {
                                                Some(description)
                                            },
                                            image_url: resolved_image,
                                            start_time: start_ms,
                                            end_time: end_ms,
                                            has_catchup,
                                            episode_number,
                                            airing_kind,
                                            episode_title,
                                            programme_year,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        buf.clear();
    }

    ParseOutput { entries, channel_icons }
}

fn parse_from_json(input_json: &str) -> String {
    match serde_json::from_str::<ParseInput>(input_json) {
        Ok(input) => {
            let output = parse_xmltv(&input.xml, &input.source_id);
            serde_json::to_string(&output)
                .unwrap_or_else(|_| "{\"entries\":[],\"channelIcons\":{}}".to_string())
        }
        // Malformed input must never crash a sync -- return an empty result and let
        // RustXmltvParser.kt's caller treat it like a parse that found nothing, not a crash.
        Err(_) => "{\"entries\":[],\"channelIcons\":{}}".to_string(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_XmltvNativeBridge_nativeParse(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let input_str: String = match env.get_string(&input_json) {
            Ok(s) => s.into(),
            Err(_) => return Ok(env.new_string("{\"entries\":[],\"channelIcons\":{}}").unwrap().into_raw()),
        };
        let output_json = parse_from_json(&input_str);
        env.new_string(output_json)
            .unwrap_or_else(|_| env.new_string("{\"entries\":[],\"channelIcons\":{}}").unwrap())
            .into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// The tally of facts seen while parsing, so Kotlin can print it.
///
/// Cumulative for the life of the process, which is what is wanted: one refresh may be a single
/// small source, and the question is whether ANY provider in this household sends these tags.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_XmltvNativeBridge_nativeFieldTally(
    mut env: EnvUnowned,
    _class: JClass,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        env.new_string(field_tally())
            .unwrap_or_else(|_| env.new_string("").unwrap())
            .into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_XmltvNativeBridge_nativeEngineVersion(
    _env: EnvUnowned,
    _class: JClass,
) -> jint {
    ENGINE_VERSION
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_XmltvNativeBridge_nativeLogicVersion(
    mut env: EnvUnowned,
    _class: JClass,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok(env.new_string(LOGIC_VERSION)?.into_raw())
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logic_version_is_the_source_hash() {
        use sha2::{Digest, Sha256};
        assert_eq!(LOGIC_VERSION.len(), 64);
        assert!(LOGIC_VERSION.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        let hex: String = Sha256::digest(include_bytes!("lib.rs"))
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(LOGIC_VERSION, hex);
    }

    // The question E3b actually turns on: do these feeds carry the facts at all? A counter that
    // cannot count is worse than no counter, because it answers "no" convincingly.
    #[test]
    fn field_tally_counts_the_facts_the_guide_wants_to_mark() {
        let before = SEEN_EPISODE_NUM.load(Ordering::Relaxed);
        let before_date = SEEN_DATE.load(Ordering::Relaxed);
        let before_new = SEEN_NEW.load(Ordering::Relaxed);
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
            <tv>
              <programme channel="c1" start="20260904200000 +0000" stop="20260904210000 +0000">
                <title>Ripper Street</title>
                <episode-num system="xmltv_ns">2.4.</episode-num>
                <date>2013</date>
                <rating><value>15</value></rating>
                <new />
                <sub-title>The Weight of One Man's Heart</sub-title>
              </programme>
            </tv>"#;
        let out = parse_xmltv(xml, "src");
        assert_eq!(out.entries.len(), 1, "the programme must still parse");
        assert_eq!(out.entries[0].episode_number.as_deref(), Some("2.4."));
        assert_eq!(out.entries[0].airing_kind, Some("new"));
        assert_eq!(out.entries[0].programme_year.as_deref(), Some("2013"));
        assert_eq!(out.entries[0].episode_title.as_deref(), Some("The Weight of One Man's Heart"));
        assert_eq!(SEEN_EPISODE_NUM.load(Ordering::Relaxed), before + 1);
        assert_eq!(SEEN_DATE.load(Ordering::Relaxed), before_date + 1);
        assert_eq!(
            SEEN_NEW.load(Ordering::Relaxed),
            before_new + 1,
            "self-closing <new /> counts"
        );
        assert!(field_tally().contains("episode-num"));
    }

    fn one_programme(inner: &str) -> LocalEpgEntryProbe {
        let xml = format!(
            r#"<tv><programme start="20260505120000 +0000" stop="20260505130000 +0000" channel="c1">{inner}</programme></tv>"#
        );
        let out = parse_xmltv(&xml, "s");
        assert_eq!(out.entries.len(), 1);
        let e = &out.entries[0];
        (e.title.clone(), e.description.clone(), e.episode_title.clone())
    }
    type LocalEpgEntryProbe = (String, Option<String>, Option<String>);

    // Same vector as XmltvParserTest.`entities and character references in text are kept`.
    #[test]
    fn entities_in_title_and_desc_are_kept() {
        let (t, d, _) = one_programme(
            r#"<title lang="en">Tom &amp; Jerry&apos;s &#233;t&#xE9;</title><desc>A &lt;b&gt; desc</desc>"#,
        );
        assert_eq!(t, "Tom & Jerry's \u{e9}t\u{e9}");
        assert_eq!(d.as_deref(), Some("A <b> desc"));
    }

    #[test]
    fn numeric_references_decimal_and_hex() {
        let (t, _, st) = one_programme(
            r#"<title>&#65;&#x42;&#x1F600;&quot;x&quot;</title><sub-title>&#x43; &amp;&amp; D</sub-title>"#,
        );
        assert_eq!(t, "AB\u{1F600}\"x\"");
        assert_eq!(st.as_deref(), Some("C && D"));
    }

    #[test]
    fn unknown_entity_is_kept_literally() {
        let (t, _, _) = one_programme(r#"<title>A &foo; B &#xD800; C</title>"#);
        assert_eq!(t, "A &foo; B &#xD800; C");
    }

    #[test]
    fn cdata_is_unchanged() {
        let (t, d, _) = one_programme(
            r#"<title><![CDATA[Tom & <Jerry>]]></title><desc>x <![CDATA[&amp;]]> &amp; y</desc>"#,
        );
        assert_eq!(t, "Tom & <Jerry>");
        assert_eq!(d.as_deref(), Some("x &amp; & y"));
    }

    // Mirrors XmltvParserTest.`parse with valid XMLTV returns entries`
    #[test]
    fn parse_with_valid_xmltv_returns_entries() {
        let xml = r#"
            <?xml version="1.0" encoding="UTF-8"?>
            <tv>
                <programme start="20260505120000 +0000" stop="20260505130000 +0000" channel="bbc1">
                    <title>News at Noon</title>
                    <desc>Daily news program.</desc>
                </programme>
                <programme start="20260505130000 +0000" stop="20260505140000 +0000" channel="bbc1">
                    <title>Afternoon Show</title>
                </programme>
            </tv>
        "#;
        let output = parse_xmltv(xml, "source1");
        assert_eq!(output.entries.len(), 2);
        let first = &output.entries[0];
        assert_eq!(first.channel_id, "bbc1");
        assert_eq!(first.title, "News at Noon");
        assert_eq!(first.description.as_deref(), Some("Daily news program."));
        assert_eq!(first.source_id, "source1");
        assert!(first.start_time > 0);
        assert_eq!(first.end_time - first.start_time, 3_600_000);
    }

    // Mirrors XmltvParserTest.`a repeat marker is not a catch-up archive`
    #[test]
    fn previously_shown_is_a_repeat_marker_not_a_catchup_archive() {
        // `<previously-shown>` says the programme has aired before. It says nothing about
        // whether the provider keeps a recording, and treating it as catch-up offered a Catch
        // Up button on channels with no archive — discoverable only by pressing it.
        let xml = r#"
            <tv>
                <programme start="20260505120000 +0000" stop="20260505130000 +0000" channel="bbc1">
                    <title>A Repeat</title>
                    <previously-shown start="20250101000000 +0000" />
                </programme>
                <programme start="20260505130000 +0000" stop="20260505140000 +0000" channel="bbc1">
                    <title>Also A Repeat</title>
                    <previously-shown></previously-shown>
                </programme>
                <programme start="20260505140000 +0000" stop="20260505150000 +0000" channel="bbc1">
                    <title>Actually Catch-up</title>
                    <catchup enabled="yes" />
                </programme>
            </tv>
        "#;
        let output = parse_xmltv(xml, "source1");
        assert_eq!(output.entries.len(), 3);
        assert!(!output.entries[0].has_catchup, "self-closing repeat marker");
        assert!(!output.entries[1].has_catchup, "paired repeat marker");
        assert_eq!(output.entries[0].airing_kind, Some("repeat"));
        assert_eq!(output.entries[1].airing_kind, Some("repeat"));
        // The genuine per-programme extension still counts.
        assert!(
            output.entries[2].has_catchup,
            "<catchup enabled=yes> is a real signal"
        );
    }

    // Mirrors XmltvParserTest.`parse handles sequential tags without skipping`
    #[test]
    fn parse_handles_sequential_tags_without_skipping() {
        let xml = r#"
            <tv>
                <programme start="20260505120000 +0000" stop="20260505130000 +0000" channel="bbc1">
                    <title>Full Title</title>
                    <desc>Full Description</desc>
                    <icon src="http://logo.com/icon.png" />
                </programme>
            </tv>
        "#;
        let output = parse_xmltv(xml, "source1");
        assert_eq!(output.entries.len(), 1);
        assert_eq!(output.entries[0].title, "Full Title");
        assert_eq!(output.entries[0].description.as_deref(), Some("Full Description"));
        assert_eq!(output.entries[0].image_url.as_deref(), Some("http://logo.com/icon.png"));
    }

    #[test]
    fn programme_falls_back_to_channel_icon_when_no_own_icon() {
        let xml = r#"
            <tv>
                <channel id="bbc1">
                    <icon src="http://logo.com/channel.png" />
                </channel>
                <programme start="20260505120000 +0000" stop="20260505130000 +0000" channel="bbc1">
                    <title>No Own Icon</title>
                </programme>
            </tv>
        "#;
        let output = parse_xmltv(xml, "source1");
        assert_eq!(output.entries.len(), 1);
        assert_eq!(
            output.entries[0].image_url.as_deref(),
            Some("http://logo.com/channel.png")
        );
        assert_eq!(
            output.channel_icons.get("bbc1").map(|s| s.as_str()),
            Some("http://logo.com/channel.png")
        );
    }

    #[test]
    fn glued_new_marker_is_stripped_and_real_titles_kept() {
        assert_eq!(strip_glued_new_marker("NewClickbait Clinic".into()), "Clickbait Clinic");
        assert_eq!(strip_glued_new_marker("NewThis City Is Ours".into()), "This City Is Ours");
        assert_eq!(strip_glued_new_marker("Newsnight".into()), "Newsnight");
        assert_eq!(strip_glued_new_marker("NewsWatch".into()), "NewsWatch");
        assert_eq!(strip_glued_new_marker("New Tricks".into()), "New Tricks");
        assert_eq!(strip_glued_new_marker("New".into()), "New");
    }

    #[test]
    fn missing_offset_falls_back_to_utc() {
        let xml = r#"
            <tv>
                <programme start="20260505120000" stop="20260505130000" channel="bbc1">
                    <title>No Offset</title>
                </programme>
            </tv>
        "#;
        let output = parse_xmltv(xml, "source1");
        assert_eq!(output.entries.len(), 1);
        assert_eq!(output.entries[0].end_time - output.entries[0].start_time, 3_600_000);
    }

    #[test]
    fn blank_title_is_rejected() {
        let xml = r#"
            <tv>
                <programme start="20260505120000 +0000" stop="20260505130000 +0000" channel="bbc1">
                    <title></title>
                </programme>
            </tv>
        "#;
        let output = parse_xmltv(xml, "source1");
        assert!(output.entries.is_empty());
    }

    #[test]
    fn end_before_or_equal_start_is_rejected() {
        let xml = r#"
            <tv>
                <programme start="20260505130000 +0000" stop="20260505130000 +0000" channel="bbc1">
                    <title>Zero Duration</title>
                </programme>
                <programme start="20260505140000 +0000" stop="20260505130000 +0000" channel="bbc1">
                    <title>Negative Duration</title>
                </programme>
            </tv>
        "#;
        let output = parse_xmltv(xml, "source1");
        assert!(output.entries.is_empty());
    }

    #[test]
    fn sanitize_icon_url_rejects_data_and_relative_urls() {
        assert_eq!(sanitize_icon_url(Some("data:image/png;base64,abcd")), None);
        assert_eq!(sanitize_icon_url(Some("/relative/path.png")), None);
        assert_eq!(sanitize_icon_url(Some("")), None);
        assert_eq!(
            sanitize_icon_url(Some("//cdn.example.com/logo.png")),
            Some("https://cdn.example.com/logo.png".to_string())
        );
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
