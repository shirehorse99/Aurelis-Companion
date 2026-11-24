//! Native Xtream Codes JSON parse engine for Aurelis's `core/data` `XtreamParser`.
//!
//! Mirrors `XtreamParser.kt`'s four parse entry points (`parseCategories`,
//! `parseStreams`, `parseVods`, `parseSeries`) and its `extractYear`/
//! `resolveImageUrl` helpers. `CountryAffinity.detectForChannel` and
//! `TitleMetadataParser.parse(name).year` stay Kotlin-side post-processing
//! (see `RustXtreamParser.kt`), matching `rust/m3u-parser`'s precedent of
//! leaving Aurelis-specific keyword/heuristic tables in Kotlin.
//!
//! One simplification vs. the byte-for-byte Kotlin behavior: Kotlin's
//! `JsonReader` resolves alias fields (e.g. `stream_id`/`num`/`id`, or
//! `container_extension`/`container`) by whichever key appears FIRST in the
//! JSON object's own field order, with a quirky re-overwrite rule for the
//! `container`/`container_extension` pair specifically. This port instead
//! takes the first non-blank value among a fixed name-priority list. The two
//! behaviors coincide whenever a provider sends only one alias per object
//! (the overwhelmingly common case, and the only case any existing test
//! exercises) and only diverge if a provider sends multiple conflicting
//! aliases for the same field in one object.
//!
//! Owns no long-lived state: one call receives one JSON response and returns
//! the parsed rows. Not wired in as the app's active parser anywhere -- the
//! existing `XtreamParser` class remains the only implementation actually
//! selected, mirroring `rust/epg-grid`'s, `rust/xmltv-parser`'s,
//! `rust/m3u-parser`'s, and `rust/channel-merge`'s status.

use jni::{EnvUnowned};
use jni::objects::{JByteBuffer, JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
/// v2: added `nativeParseStreamsBuf` (zero-copy direct-buffer streams path, WS-JNI).
/// v3: `SeriesOut.genre`, the provider's raw series genre string.
pub const ENGINE_VERSION: i32 = 3;

static ISO_YEAR_PREFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{4})-").unwrap());
static YEAR_SUFFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{4})$").unwrap());

// ── Value coercion helpers, mirroring XtreamParser.kt's readAsString/readBoolean/readInt ──

fn value_as_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn value_as_bool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0).unwrap_or_else(|| n.as_f64().map(|f| f != 0.0).unwrap_or(false)),
        Some(Value::String(s)) => {
            let t = s.trim();
            t == "1" || t.eq_ignore_ascii_case("true")
        }
        _ => false,
    }
}

fn value_as_int(v: Option<&Value>) -> i32 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .map(|i| i as i32)
            .or_else(|| n.as_f64().map(|f| f as i32))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i32>().unwrap_or(0),
        _ => 0,
    }
}

/// First non-blank value among a name-priority list of candidate keys (see the
/// module-level doc comment for how this differs from Kotlin's JSON-order resolution).
fn first_non_blank(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        let s = value_as_string(obj.get(*key));
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

/// Mirrors `extractYear` (XtreamParser.kt).
fn extract_year(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    if raw.len() == 4 && raw.chars().all(|c| c.is_ascii_digit()) {
        return Some(raw.to_string());
    }
    if let Some(caps) = ISO_YEAR_PREFIX_REGEX.captures(raw) {
        return Some(caps[1].to_string());
    }
    if let Some(caps) = YEAR_SUFFIX_REGEX.captures(raw) {
        return Some(caps[1].to_string());
    }
    None
}

/// Mirrors `resolveImageUrl` (XtreamParser.kt).
fn resolve_image_url(base_url: &str, path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let lower = path.to_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return path.to_string();
    }
    format!("{}/{}", base_url.trim_end_matches('/'), path.trim_start_matches('/'))
}

fn parse_json_array(json: &str) -> Vec<serde_json::Map<String, Value>> {
    match serde_json::from_str::<Value>(json) {
        Ok(Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| match v {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .collect(),
        // Malformed/null response -- return no rows, matching the Kotlin
        // `if (reader.peek() == JsonToken.NULL) return` / caught-exception paths.
        _ => Vec::new(),
    }
}

// ── Categories ───────────────────────────────────────────────────────────────

/// Mirrors `parseCategories` (XtreamParser.kt).
pub fn parse_categories(json: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for obj in parse_json_array(json) {
        let id = value_as_string(obj.get("category_id"));
        let name = value_as_string(obj.get("category_name"));
        if !id.is_empty() {
            map.insert(id, name);
        }
    }
    map
}

// ── Live streams ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamsInput {
    pub json: String,
    pub source_id: String,
    pub base_url: String,
    pub root: String,
    pub live_path: String,
    pub ext: String,
    pub encoded_user: String,
    pub encoded_pass: String,
    pub category_map: HashMap<String, String>,
}

/// Mirrors the subset of `LocalChannel` this parser populates. `countryCode` is
/// deliberately excluded -- filled in by Kotlin via `CountryAffinity.detectForChannel`
/// after this call returns.
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
    pub catchup_source: Option<String>,
    pub epg_id: Option<String>,
    pub playlist_order: i32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamsOutput {
    pub channels: Vec<ChannelOut>,
}

/// The non-catalogue metadata for a `parseStreams` call, borrowed so both the
/// JSON-wrapped path ([`StreamsInput`]) and the zero-copy direct-buffer path
/// ([`StreamsMetaInput`]) can share one parse core without copying.
pub struct StreamsMeta<'a> {
    pub source_id: &'a str,
    pub base_url: &'a str,
    pub root: &'a str,
    pub live_path: &'a str,
    pub ext: &'a str,
    pub encoded_user: &'a str,
    pub encoded_pass: &'a str,
    pub category_map: &'a HashMap<String, String>,
}

/// Same metadata as [`StreamsInput`] minus the (multi-MB) `json` catalogue,
/// which the zero-copy path reads straight from a direct `ByteBuffer` instead.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamsMetaInput {
    pub source_id: String,
    pub base_url: String,
    pub root: String,
    pub live_path: String,
    pub ext: String,
    pub encoded_user: String,
    pub encoded_pass: String,
    pub category_map: HashMap<String, String>,
}

impl StreamsMetaInput {
    fn as_meta(&self) -> StreamsMeta<'_> {
        StreamsMeta {
            source_id: &self.source_id,
            base_url: &self.base_url,
            root: &self.root,
            live_path: &self.live_path,
            ext: &self.ext,
            encoded_user: &self.encoded_user,
            encoded_pass: &self.encoded_pass,
            category_map: &self.category_map,
        }
    }
}

/// Mirrors `parseStreams` (XtreamParser.kt).
pub fn parse_streams(input: &StreamsInput) -> StreamsOutput {
    parse_streams_core(
        &input.json,
        &StreamsMeta {
            source_id: &input.source_id,
            base_url: &input.base_url,
            root: &input.root,
            live_path: &input.live_path,
            ext: &input.ext,
            encoded_user: &input.encoded_user,
            encoded_pass: &input.encoded_pass,
            category_map: &input.category_map,
        },
    )
}

fn parse_streams_core(catalog_json: &str, m: &StreamsMeta) -> StreamsOutput {
    let mut channels = Vec::new();
    let mut playlist_order = 0i32;

    for obj in parse_json_array(catalog_json) {
        let stream_id = value_as_string(obj.get("stream_id"));
        let name = value_as_string(obj.get("name"));
        if stream_id.is_empty() || name.is_empty() {
            continue;
        }
        let icon = value_as_string(obj.get("stream_icon"));
        let epg_id = value_as_string(obj.get("epg_channel_id"));
        let cat_id = value_as_string(obj.get("category_id"));
        let archive = value_as_bool(obj.get("tv_archive"));
        let archive_days = value_as_int(obj.get("tv_archive_duration"));
        let catchup_source = value_as_string(obj.get("catchup_source"));

        let genre = match m.category_map.get(&cat_id) {
            Some(g) if !g.is_empty() => g.clone(),
            // Same honesty as M3U: missing category is Uncategorized, never "General".
            _ => "Uncategorized".to_string(),
        };
        let url = format!(
            "{}/{}/{}/{}/{}.{}",
            m.root, m.live_path, m.encoded_user, m.encoded_pass, stream_id, m.ext
        );

        channels.push(ChannelOut {
            id: format!("{}:{}", m.source_id, stream_id),
            source_id: m.source_id.to_string(),
            name,
            genre,
            logo_url: resolve_image_url(m.base_url, &icon),
            stream_url: url,
            has_catchup: archive,
            catchup_days: if archive && archive_days > 0 { Some(archive_days) } else { None },
            catchup_source: if archive && !catchup_source.is_empty() { Some(catchup_source) } else { None },
            epg_id: if epg_id.is_empty() { None } else { Some(epg_id) },
            playlist_order,
        });
        playlist_order += 1;
    }

    StreamsOutput { channels }
}

// ── VOD streams ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VodsInput {
    pub json: String,
    pub source_id: String,
    pub base_url: String,
}

/// Mirrors the subset of `LocalVod` this parser populates. `year` is only
/// `extractYear`'s result -- the `TitleMetadataParser.parse(name).year` title
/// fallback is applied Kotlin-side (see module doc comment).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VodOut {
    pub id: String,
    pub source_id: String,
    pub name: String,
    pub stream_icon: Option<String>,
    pub rating: Option<String>,
    pub year: Option<String>,
    pub category_id: Option<String>,
    pub stream_id: Option<String>,
    pub container_extension: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VodsOutput {
    pub vods: Vec<VodOut>,
}

/// Mirrors `parseVods` (XtreamParser.kt).
pub fn parse_vods(input: &VodsInput) -> VodsOutput {
    let mut vods = Vec::new();

    for obj in parse_json_array(&input.json) {
        let stream_id = first_non_blank(&obj, &["stream_id", "num", "id"]);
        let name = first_non_blank(&obj, &["name", "title"]);
        if stream_id.is_empty() || name.is_empty() {
            continue;
        }
        let icon = first_non_blank(&obj, &["stream_icon", "movie_image"]);
        let rating = first_non_blank(&obj, &["rating", "rating_5based"]);
        let year_field = first_non_blank(&obj, &["year", "releaseDate", "release_date"]);
        let cat_id = value_as_string(obj.get("category_id"));
        let ext = {
            let e = first_non_blank(&obj, &["container_extension", "container"]);
            if e.is_empty() { "mp4".to_string() } else { e }
        };

        vods.push(VodOut {
            id: format!("{}:{}", input.source_id, stream_id),
            source_id: input.source_id.clone(),
            name,
            stream_icon: {
                let resolved = resolve_image_url(&input.base_url, &icon);
                if resolved.is_empty() { None } else { Some(resolved) }
            },
            rating: if rating.is_empty() { None } else { Some(rating) },
            year: extract_year(&year_field),
            category_id: if cat_id.is_empty() { None } else { Some(cat_id) },
            stream_id: Some(stream_id),
            container_extension: ext,
        });
    }

    VodsOutput { vods }
}

// ── Series ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesInput {
    pub json: String,
    pub source_id: String,
    pub base_url: String,
}

/// Mirrors the subset of `LocalSeries` this parser populates. `releaseDate`'s
/// `TitleMetadataParser.parse(name).year` title fallback is applied Kotlin-side.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesOut {
    pub id: String,
    pub source_id: String,
    pub name: String,
    pub cover: Option<String>,
    pub rating: Option<String>,
    pub release_date: Option<String>,
    pub category_id: Option<String>,
    pub series_id: Option<String>,
    /// The provider's genre names as sent, comma-separated ("Documentary, Crime"). Kotlin maps
    /// them to TMDB ids (`xtreamSeriesGenreIds`); this side passes the raw string through.
    pub genre: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesOutput {
    pub series: Vec<SeriesOut>,
}

/// Mirrors `parseSeries` (XtreamParser.kt).
pub fn parse_series(input: &SeriesInput) -> SeriesOutput {
    let mut series = Vec::new();

    for obj in parse_json_array(&input.json) {
        let series_id = first_non_blank(&obj, &["series_id", "id"]);
        let name = first_non_blank(&obj, &["name", "title"]);
        if series_id.is_empty() || name.is_empty() {
            continue;
        }
        let cover = first_non_blank(&obj, &["cover", "cover_big"]);
        let rating = first_non_blank(&obj, &["rating", "rating_5based"]);
        let release_date = first_non_blank(&obj, &["releaseDate", "release_date", "first_air_date"]);
        let genre = first_non_blank(&obj, &["genre"]);
        let cat_id = value_as_string(obj.get("category_id"));

        series.push(SeriesOut {
            id: format!("{}:{}", input.source_id, series_id),
            source_id: input.source_id.clone(),
            name,
            cover: {
                let resolved = resolve_image_url(&input.base_url, &cover);
                if resolved.is_empty() { None } else { Some(resolved) }
            },
            rating: if rating.is_empty() { None } else { Some(rating) },
            release_date: if release_date.is_empty() { None } else { Some(release_date) },
            category_id: if cat_id.is_empty() { None } else { Some(cat_id) },
            series_id: Some(series_id),
            genre: if genre.is_empty() { None } else { Some(genre) },
        });
    }

    SeriesOutput { series }
}

// ── JNI exports ──────────────────────────────────────────────────────────────

fn read_jstring(env: &mut Env, s: &JString) -> Option<String> {
    env.get_string(s).ok().map(|s| s.into())
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_XtreamNativeBridge_nativeParseCategories(
    mut env: EnvUnowned,
    _class: JClass,
    json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let json_str = read_jstring(env, &json).unwrap_or_default();
        let categories = parse_categories(&json_str);
        let output_json = serde_json::to_string(&categories).unwrap_or_else(|_| "{}".to_string());
        env.new_string(output_json).unwrap_or_else(|_| env.new_string("{}").unwrap()).into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// What every JNI entry point returns when it cannot parse.
///
/// It used to be `{"channels":[]}` / `{"vods":[]}` / `{"series":[]}` — a successful parse of a
/// provider that happens to carry nothing. The two were indistinguishable on the Kotlin side,
/// which did `output.vods.orEmpty()`, so a malformed catalogue, a serde failure or a bad
/// direct-buffer read all read as "this provider has no films".
///
/// That is not a cosmetic difference. XtreamProvider.streamLibrary calls `sink.beginFull`
/// BEFORE parsing, so the rows are cleared first: a parse failure therefore emptied the
/// library, reported success, and let `lastRefreshed` advance — after which every later pass
/// was incremental and the catalogue never came back.
///
/// Carrying no data key at all makes the two cases different: Kotlin sees `vods == null` and
/// raises, instead of quietly writing nothing over everything.
const PARSE_FAILED: &str = "{\"error\":\"xtream native parse failed\"}";

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_XtreamNativeBridge_nativeParseStreams(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let empty = PARSE_FAILED;
        let input_str = match read_jstring(env, &input_json) {
            Some(s) => s,
            None => return Ok(env.new_string(empty).unwrap().into_raw()),
        };
        let output_json = match serde_json::from_str::<StreamsInput>(&input_str) {
            Ok(input) => serde_json::to_string(&parse_streams(&input)).unwrap_or_else(|_| empty.to_string()),
            Err(_) => empty.to_string(),
        };
        env.new_string(output_json).unwrap_or_else(|_| env.new_string(empty).unwrap()).into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// Zero-copy streams path (WS-JNI). The (small) metadata is passed as JSON in
/// `meta_json`; the (multi-MB) raw catalogue JSON is borrowed straight out of a
/// direct `ByteBuffer` via `jni_cache::direct_buffer_bytes` — no `get_string`
/// UTF-8 copy and no double-JSON-encoding of the catalogue on the Kotlin side.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_XtreamNativeBridge_nativeParseStreamsBuf(
    mut env: EnvUnowned,
    _class: JClass,
    meta_json: JString,
    catalog: JByteBuffer,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let empty = PARSE_FAILED;
        let meta_str = match read_jstring(env, &meta_json) {
            Some(s) => s,
            None => return Ok(env.new_string(empty).unwrap().into_raw()),
        };
        let meta: StreamsMetaInput = match serde_json::from_str(&meta_str) {
            Ok(m) => m,
            Err(_) => return Ok(env.new_string(empty).unwrap().into_raw()),
        };
        // SAFETY: `catalog` is alive for this call; the borrow does not outlive it.
        let catalog_bytes = match unsafe { jni_cache::direct_buffer_bytes(&env, &catalog) } {
            Some(b) => b,
            None => return Ok(env.new_string(empty).unwrap().into_raw()),
        };
        let output_json = match std::str::from_utf8(catalog_bytes) {
            Ok(catalog_json) => serde_json::to_string(&parse_streams_core(catalog_json, &meta.as_meta()))
                .unwrap_or_else(|_| empty.to_string()),
            Err(_) => empty.to_string(),
        };
        env.new_string(output_json).unwrap_or_else(|_| env.new_string(empty).unwrap()).into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_XtreamNativeBridge_nativeParseVods(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let empty = PARSE_FAILED;
        let input_str = match read_jstring(env, &input_json) {
            Some(s) => s,
            None => return Ok(env.new_string(empty).unwrap().into_raw()),
        };
        let output_json = match serde_json::from_str::<VodsInput>(&input_str) {
            Ok(input) => serde_json::to_string(&parse_vods(&input)).unwrap_or_else(|_| empty.to_string()),
            Err(_) => empty.to_string(),
        };
        env.new_string(output_json).unwrap_or_else(|_| env.new_string(empty).unwrap()).into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_XtreamNativeBridge_nativeParseSeries(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let empty = PARSE_FAILED;
        let input_str = match read_jstring(env, &input_json) {
            Some(s) => s,
            None => return Ok(env.new_string(empty).unwrap().into_raw()),
        };
        let output_json = match serde_json::from_str::<SeriesInput>(&input_str) {
            Ok(input) => serde_json::to_string(&parse_series(&input)).unwrap_or_else(|_| empty.to_string()),
            Err(_) => empty.to_string(),
        };
        env.new_string(output_json).unwrap_or_else(|_| env.new_string(empty).unwrap()).into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_XtreamNativeBridge_nativeEngineVersion(
    _env: EnvUnowned,
    _class: JClass,
) -> jint {
    ENGINE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors XtreamParserTest.`parseCategories handles mixed value types`
    #[test]
    fn parse_categories_handles_mixed_value_types() {
        let json = r#"[
            {"category_id": 10, "category_name": "Movies"},
            {"category_id": "11", "category_name": null},
            {"category_id": null, "category_name": "Skip"}
        ]"#;
        let categories = parse_categories(json);
        assert_eq!(categories.len(), 2);
        assert_eq!(categories.get("10"), Some(&"Movies".to_string()));
        assert_eq!(categories.get("11"), Some(&"".to_string()));
    }

    fn streams_input(json: &str, category_map: HashMap<String, String>) -> StreamsInput {
        StreamsInput {
            json: json.to_string(),
            source_id: "source-1".to_string(),
            base_url: "http://example.com/".to_string(),
            root: "http://example.com".to_string(),
            live_path: "live".to_string(),
            ext: "ts".to_string(),
            encoded_user: "u".to_string(),
            encoded_pass: "p".to_string(),
            category_map,
        }
    }

    #[test]
    fn parse_streams_builds_stream_url_and_applies_catchup_rules() {
        let json = r#"[
            {"stream_id": 1, "name": "Sky News", "category_id": "5", "tv_archive": 1, "tv_archive_duration": 3, "catchup_source": "tmpl"},
            {"stream_id": 2, "name": "BBC One", "category_id": "9"}
        ]"#;
        let mut category_map = HashMap::new();
        category_map.insert("5".to_string(), "News".to_string());
        let output = parse_streams(&streams_input(json, category_map));

        assert_eq!(output.channels.len(), 2);
        let sky = &output.channels[0];
        assert_eq!(sky.id, "source-1:1");
        assert_eq!(sky.stream_url, "http://example.com/live/u/p/1.ts");
        assert_eq!(sky.genre, "News");
        assert!(sky.has_catchup);
        assert_eq!(sky.catchup_days, Some(3));
        assert_eq!(sky.catchup_source, Some("tmpl".to_string()));

        let bbc = &output.channels[1];
        assert_eq!(bbc.genre, "Uncategorized");
        assert!(!bbc.has_catchup);
        assert_eq!(bbc.catchup_days, None);
        assert_eq!(bbc.playlist_order, 1);
    }

    #[test]
    fn parse_streams_skips_blank_id_or_name() {
        let json = r#"[{"stream_id": "", "name": "No Id"}, {"stream_id": "3", "name": ""}]"#;
        let output = parse_streams(&streams_input(json, HashMap::new()));
        assert!(output.channels.is_empty());
    }

    // Mirrors XtreamParserTest.`parseVods does not abort on null and numeric fields`
    #[test]
    fn parse_vods_does_not_abort_on_null_and_numeric_fields() {
        let json = r#"[
            {"stream_id": 101, "name": "Movie One", "stream_icon": null, "rating": 7.1, "year": 2024, "category_id": 5, "container_extension": "mp4"},
            {"stream_id": "102", "name": "Movie Two", "stream_icon": "img/two.jpg", "rating": null, "year": null, "category_id": null, "container_extension": null}
        ]"#;
        let output = parse_vods(&VodsInput {
            json: json.to_string(),
            source_id: "source-1".to_string(),
            base_url: "http://example.com/".to_string(),
        });

        assert_eq!(output.vods.len(), 2);
        assert_eq!(output.vods[0].id, "source-1:101");
        assert_eq!(output.vods[0].rating, Some("7.1".to_string()));
        assert_eq!(output.vods[0].year, Some("2024".to_string()));
        assert_eq!(output.vods[0].category_id, Some("5".to_string()));
        assert_eq!(output.vods[0].stream_icon, None);
        assert_eq!(output.vods[1].stream_icon, Some("http://example.com/img/two.jpg".to_string()));
    }

    // Mirrors XtreamParserTest.`parseVods supports alternate provider field names`
    #[test]
    fn parse_vods_supports_alternate_provider_field_names() {
        let json = r#"[
            {"id": 201, "title": "Alt Movie", "movie_image": "posters/alt.jpg", "rating": "6.9", "release_date": "2021-09-10", "category_id": "17", "container": "mkv"}
        ]"#;
        let output = parse_vods(&VodsInput {
            json: json.to_string(),
            source_id: "source-1".to_string(),
            base_url: "http://example.com/".to_string(),
        });

        assert_eq!(output.vods.len(), 1);
        let vod = &output.vods[0];
        assert_eq!(vod.id, "source-1:201");
        assert_eq!(vod.name, "Alt Movie");
        assert_eq!(vod.stream_icon, Some("http://example.com/posters/alt.jpg".to_string()));
        assert_eq!(vod.year, Some("2021".to_string()));
        assert_eq!(vod.container_extension, "mkv");
    }

    // Mirrors XtreamParserTest.`parseSeries supports release_date and mixed types`
    #[test]
    fn parse_series_supports_release_date_and_mixed_types() {
        let json = r#"[
            {"series_id": 501, "name": "Series One", "cover": null, "rating": 8.3, "release_date": "2023-11-01", "category_id": 2},
            {"series_id": "502", "name": "Series Two", "cover": "covers/two.jpg", "rating": null, "releaseDate": "2022-01-20", "category_id": null}
        ]"#;
        let output = parse_series(&SeriesInput {
            json: json.to_string(),
            source_id: "source-1".to_string(),
            base_url: "http://example.com/".to_string(),
        });

        assert_eq!(output.series.len(), 2);
        assert_eq!(output.series[0].id, "source-1:501");
        assert_eq!(output.series[0].release_date, Some("2023-11-01".to_string()));
        assert_eq!(output.series[0].category_id, Some("2".to_string()));
        assert_eq!(output.series[0].cover, None);
        assert_eq!(output.series[1].cover, Some("http://example.com/covers/two.jpg".to_string()));
        assert_eq!(output.series[1].release_date, Some("2022-01-20".to_string()));
    }

    // Mirrors XtreamParserTest.`parseSeries supports alternate provider field names`
    #[test]
    fn parse_series_supports_alternate_provider_field_names() {
        let json = r#"[
            {"id": 901, "title": "Alt Series", "cover_big": "art/series.jpg", "rating_5based": 4.2, "first_air_date": "2020-04-01", "category_id": 9}
        ]"#;
        let output = parse_series(&SeriesInput {
            json: json.to_string(),
            source_id: "source-1".to_string(),
            base_url: "http://example.com/".to_string(),
        });

        assert_eq!(output.series.len(), 1);
        let s = &output.series[0];
        assert_eq!(s.id, "source-1:901");
        assert_eq!(s.name, "Alt Series");
        assert_eq!(s.cover, Some("http://example.com/art/series.jpg".to_string()));
        assert_eq!(s.release_date, Some("2020-04-01".to_string()));
        assert_eq!(s.category_id, Some("9".to_string()));
    }

    // Mirrors the genre line of XtreamParserTest.`parseSeries maps the provider genre to TMDB ids`
    #[test]
    fn parse_series_carries_the_provider_genre() {
        let json = r#"[
            {"series_id": 701, "name": "Field Notes", "genre": "Documentary, Crime"},
            {"series_id": 702, "name": "Plain"}
        ]"#;
        let output = parse_series(&SeriesInput {
            json: json.to_string(),
            source_id: "source-1".to_string(),
            base_url: "http://example.com/".to_string(),
        });

        assert_eq!(output.series.len(), 2);
        assert_eq!(output.series[0].genre, Some("Documentary, Crime".to_string()));
        assert_eq!(output.series[1].genre, None);
    }

    #[test]
    fn extract_year_handles_bare_and_iso_and_trailing_years() {
        assert_eq!(extract_year("2024"), Some("2024".to_string()));
        assert_eq!(extract_year("2023-11-01"), Some("2023".to_string()));
        assert_eq!(extract_year("15/07/2023"), Some("2023".to_string()));
        assert_eq!(extract_year(""), None);
        assert_eq!(extract_year("n/a"), None);
    }

    #[test]
    fn resolve_image_url_handles_absolute_and_relative_paths() {
        assert_eq!(resolve_image_url("http://example.com/", ""), "");
        assert_eq!(
            resolve_image_url("http://example.com/", "http://cdn.com/x.jpg"),
            "http://cdn.com/x.jpg"
        );
        assert_eq!(resolve_image_url("http://example.com/", "img/x.jpg"), "http://example.com/img/x.jpg");
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 3);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
