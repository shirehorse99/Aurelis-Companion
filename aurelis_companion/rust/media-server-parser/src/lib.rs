//! Native Emby/Jellyfin media-server catalogue parse engine for Aurelis.
//!
//! Mirrors the items-response → domain-row mapping that `EmbyProvider` and
//! `JellyfinProvider` do with `org.json` today. One call receives one JSON
//! response (the paginated `Items` envelope) and returns parsed movies, series,
//! or live-TV channels. Kotlin keeps HTTP, auth/session, paging, and DB writes.
//!
//! Deliberately preserves Kotlin quirks:
//! - Emby/Jellyfin image URL query-param names (`X-Emby-Token` vs `api_key`).
//! - Poster/wide-art type fallbacks (Primary/Thumb, then Thumb/Backdrop/Banner/Art).
//! - `OfficialRating` country-prefix stripping (`GB-PG` → `PG`, but `TV-Y7` kept).
//! - Every genre name (`Genres`, else `GenreItems[].Name`) returned as `genres`; Kotlin
//!   maps them to TMDB ids (`MediaServerGenres`) and leaves `categoryId` empty.
//! - `ProductionYear` / `PremiereDate` passed through as strings.
//! - `videoQualityLabel` extracted from `MediaSources` / `MediaStreams` / `Path` / `Name`.
//! - `libraryName` is **not** resolved here; `ParentId` and `Path` are returned so
//!   Kotlin can still run `EmbyJellyfinCatalogueMapper.resolveLibraryName` with its
//!   `libraryViewMap` + `KidsLibraryDetector.libraryNameFromPath` fallback.
//! - `editionGroupKey` and `countryCode` are also left Kotlin-side, matching the
//!   existing Rust parser precedent of keeping Aurelis-specific keyed heuristics in Kotlin.
//!
//! The parser accepts the full Emby/Jellyfin `Items` envelope and iterates the
//! `Items` array. If the input is itself an array, it is iterated directly.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 2;

static ISO_YEAR_PREFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{4})-").unwrap());
static YEAR_SUFFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{4})$").unwrap());

// ── Value coercion helpers, mirroring the Kotlin org.json `optString` behaviour ──

fn value_as_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn value_as_i64(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

// ── Shared helpers ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthType {
    Emby,
    Jellyfin,
}

impl AuthType {
    fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "emby" => Some(AuthType::Emby),
            "jellyfin" => Some(AuthType::Jellyfin),
            _ => None,
        }
    }

    fn token_param(&self) -> &'static str {
        match self {
            AuthType::Emby => "X-Emby-Token",
            AuthType::Jellyfin => "api_key",
        }
    }
}

/// Mirrors `EmbyJellyfinCatalogueMapper.normaliseOfficialRating`.
fn normalise_official_rating(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() > 3
        && trimmed.chars().nth(0).map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
        && trimmed.chars().nth(1).map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
        && trimmed.chars().nth(2) == Some('-')
    {
        let prefix = &trimmed[..2];
        if prefix.eq_ignore_ascii_case("TV") {
            return Some(trimmed.to_string());
        }
        let rest = &trimmed[3..];
        return if rest.is_empty() { None } else { Some(rest.to_string()) };
    }
    Some(trimmed.to_string())
}

/// Extracts a 4-digit year from a bare year or ISO/trailing date string.
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

fn has_image_tag(obj: &serde_json::Map<String, Value>, image_type: &str) -> bool {
    let array_key = format!("{}ImageTags", image_type);
    if let Some(Value::Array(arr)) = obj.get(&array_key) {
        if !arr.is_empty() {
            return true;
        }
    }
    if let Some(Value::Object(tags)) = obj.get("ImageTags") {
        let v = value_as_string(tags.get(image_type));
        if !v.is_empty() {
            return true;
        }
    }
    false
}

fn image_url(base_url: &str, item_id: &str, image_type: &str, token: &str, auth: AuthType, wide: bool) -> String {
    let b = base_url.trim_end_matches('/');
    let token_param = auth.token_param();
    if wide {
        format!("{}/Items/{}/Images/{}/0?maxWidth=1920&maxHeight=1080&quality=92&{}={}", b, item_id, image_type, token_param, token)
    } else {
        format!("{}/Items/{}/Images/{}?maxHeight=400&maxWidth=700&quality=90&{}={}", b, item_id, image_type, token_param, token)
    }
}

fn pick_poster_url(base_url: &str, item_id: &str, obj: &serde_json::Map<String, Value>, token: &str, auth: AuthType) -> Option<String> {
    if item_id.is_empty() {
        return None;
    }
    let image_type = if has_image_tag(obj, "Primary") {
        "Primary"
    } else if has_image_tag(obj, "Thumb") {
        "Thumb"
    } else {
        "Primary"
    };
    Some(image_url(base_url, item_id, image_type, token, auth, false))
}

fn pick_wide_art_url(base_url: &str, item_id: &str, obj: &serde_json::Map<String, Value>, token: &str, auth: AuthType) -> Option<String> {
    if item_id.is_empty() {
        return None;
    }
    for image_type in ["Thumb", "Backdrop", "Banner", "Art"] {
        if has_image_tag(obj, image_type) {
            return Some(image_url(base_url, item_id, image_type, token, auth, true));
        }
    }
    None
}

fn video_quality_label_from_dimensions(width: i64, height: i64) -> Option<String> {
    if width <= 0 && height <= 0 {
        return None;
    }
    if height >= 4320 || width >= 6400 {
        Some("8K".to_string())
    } else if height >= 2160 || width >= 3200 {
        Some("4K".to_string())
    } else if height >= 1080 || width >= 1800 {
        Some("1080p".to_string())
    } else if height >= 720 || width >= 1200 {
        Some("720p".to_string())
    } else if height >= 576 {
        Some("576p".to_string())
    } else if height >= 1 || width >= 1 {
        Some("SD".to_string())
    } else {
        None
    }
}

static RE_2160P: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?i)\b2160p?\b").unwrap());
static RE_8K: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?i)\b8k\b").unwrap());
static RE_1080P: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?i)\b1080p?\b").unwrap());
static RE_720P: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?i)\b720p?\b").unwrap());
static RE_576P: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?i)\b576p?\b").unwrap());
static RE_480P: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?i)\b480p?\b").unwrap());
static RE_SD: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"(?i)\bsd\b").unwrap());

fn video_quality_label_from_text(text: &str) -> Option<String> {
    let n = text.to_lowercase();
    if RE_2160P.is_match(&n)
        || ["4k", "uhd", "ultra hd"].iter().any(|k| n.contains(k))
    {
        return Some("4K".to_string());
    }
    if n.contains("4320") || RE_8K.is_match(&n) {
        return Some("8K".to_string());
    }
    if RE_1080P.is_match(&n)
        || n.contains("fhd") || n.contains("full hd")
    {
        return Some("1080p".to_string());
    }
    if RE_720P.is_match(&n) {
        return Some("720p".to_string());
    }
    if RE_576P.is_match(&n) {
        return Some("576p".to_string());
    }
    if RE_480P.is_match(&n)
        || RE_SD.is_match(&n)
    {
        return Some("SD".to_string());
    }
    None
}

fn video_quality_label_from_media_item(obj: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(Value::Array(sources)) = obj.get("MediaSources") {
        for src in sources {
            if let Some(src_obj) = src.as_object() {
                let w = value_as_i64(src_obj.get("Width"));
                let h = value_as_i64(src_obj.get("Height"));
                if let Some(label) = video_quality_label_from_dimensions(w, h) {
                    return Some(label);
                }
                if let Some(Value::Array(streams)) = src_obj.get("MediaStreams") {
                    for stream in streams {
                        if let Some(stream_obj) = stream.as_object() {
                            let t = value_as_string(stream_obj.get("Type"));
                            if !t.is_empty() && !t.eq_ignore_ascii_case("Video") {
                                continue;
                            }
                            let w = value_as_i64(stream_obj.get("Width"));
                            let h = value_as_i64(stream_obj.get("Height"));
                            if let Some(label) = video_quality_label_from_dimensions(w, h) {
                                return Some(label);
                            }
                        }
                    }
                }
            }
        }
    }
    let path = value_as_string(obj.get("Path"));
    if !path.is_empty() {
        if let Some(label) = video_quality_label_from_text(&path) {
            return Some(label);
        }
    }
    video_quality_label_from_text(&value_as_string(obj.get("Name")))
}

/// Mirrors `MediaServerGenres.namesFrom`: the `Genres` string array, else `GenreItems[].Name`.
fn genre_names(obj: &serde_json::Map<String, Value>) -> Vec<String> {
    if let Some(Value::Array(genres)) = obj.get("Genres") {
        let names: Vec<String> = genres
            .iter()
            .map(|g| value_as_string(Some(g)))
            .filter(|n| !n.is_empty())
            .collect();
        if !names.is_empty() {
            return names;
        }
    }
    if let Some(Value::Array(items)) = obj.get("GenreItems") {
        return items
            .iter()
            .filter_map(|g| g.as_object())
            .map(|g| value_as_string(g.get("Name")))
            .filter(|n| !n.is_empty())
            .collect();
    }
    Vec::new()
}

fn parse_items_array(json: &str) -> Vec<serde_json::Map<String, Value>> {
    match serde_json::from_str::<Value>(json) {
        Ok(Value::Object(mut root)) => {
            if let Some(Value::Array(items)) = root.remove("Items") {
                items
                    .into_iter()
                    .filter_map(|v| v.as_object().cloned())
                    .collect()
            } else {
                Vec::new()
            }
        }
        Ok(Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| v.as_object().cloned())
            .collect(),
        _ => Vec::new(),
    }
}

// ── Input/output wire types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseInput {
    pub json: String,
    pub source_id: String,
    pub base_url: String,
    pub token: String,
    pub auth_type: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovieOut {
    pub id: String,
    pub source_id: String,
    pub name: String,
    pub stream_icon: Option<String>,
    pub rating: Option<String>,
    pub year: Option<String>,
    pub genres: Vec<String>,
    pub stream_id: String,
    pub container_extension: String,
    pub banner_url: Option<String>,
    pub video_quality_label: Option<String>,
    pub certification: Option<String>,
    pub parent_id: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoviesOutput {
    pub movies: Vec<MovieOut>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesOut {
    pub id: String,
    pub source_id: String,
    pub name: String,
    pub cover: Option<String>,
    pub banner_url: Option<String>,
    pub rating: Option<String>,
    pub release_date: Option<String>,
    pub genres: Vec<String>,
    pub series_id: String,
    pub certification: Option<String>,
    pub parent_id: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesListOutput {
    pub series: Vec<SeriesOut>,
}

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
    pub epg_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelsOutput {
    pub channels: Vec<ChannelOut>,
}

// ── Parse entry points ─────────────────────────────────────────────────────────

pub fn parse_movies(input: &ParseInput) -> MoviesOutput {
    let auth = AuthType::from_str(&input.auth_type);
    let items = parse_items_array(&input.json);
    let mut movies = Vec::with_capacity(items.len());

    for obj in items {
        let id = value_as_string(obj.get("Id"));
        if id.is_empty() {
            continue;
        }
        let name = value_as_string(obj.get("Name"));
        if name.is_empty() {
            continue;
        }
        let year_raw = value_as_string(obj.get("ProductionYear"));
        let year = extract_year(&year_raw);
        let rating = normalise_official_rating(&value_as_string(obj.get("OfficialRating")));
        let genres = genre_names(&obj);
        let container = value_as_string(obj.get("Container"));
        let container_extension = if container.is_empty() { "mp4".to_string() } else { container };
        let parent_id = value_as_string(obj.get("ParentId"));
        let path = value_as_string(obj.get("Path"));

        let stream_id = match auth {
            Some(AuthType::Jellyfin) => format!(
                "{}/Videos/{}/stream?static=true&api_key={}",
                input.base_url.trim_end_matches('/'),
                id,
                input.token
            ),
            _ => format!(
                "{}/Videos/{}/stream?static=true&X-Emby-Token={}",
                input.base_url.trim_end_matches('/'),
                id,
                input.token
            ),
        };

        let stream_icon = auth.and_then(|a| pick_poster_url(&input.base_url, &id, &obj, &input.token, a));
        let banner_url = auth.and_then(|a| pick_wide_art_url(&input.base_url, &id, &obj, &input.token, a));
        let video_quality_label = video_quality_label_from_media_item(&obj);

        movies.push(MovieOut {
            id,
            source_id: input.source_id.clone(),
            name,
            stream_icon,
            rating: rating.clone(),
            year,
            genres,
            stream_id,
            container_extension,
            banner_url,
            video_quality_label,
            certification: rating,
            parent_id: if parent_id.is_empty() { None } else { Some(parent_id) },
            path: if path.is_empty() { None } else { Some(path) },
        });
    }

    MoviesOutput { movies }
}

pub fn parse_series(input: &ParseInput) -> SeriesListOutput {
    let auth = AuthType::from_str(&input.auth_type);
    let items = parse_items_array(&input.json);
    let mut series = Vec::with_capacity(items.len());

    for obj in items {
        let id = value_as_string(obj.get("Id"));
        if id.is_empty() {
            continue;
        }
        let name = value_as_string(obj.get("Name"));
        if name.is_empty() {
            continue;
        }
        let release_date = value_as_string(obj.get("PremiereDate"));
        let release_date = if release_date.is_empty() { None } else { Some(release_date) };
        let rating = normalise_official_rating(&value_as_string(obj.get("OfficialRating")));
        let genres = genre_names(&obj);
        let parent_id = value_as_string(obj.get("ParentId"));
        let path = value_as_string(obj.get("Path"));

        let cover = auth.and_then(|a| pick_poster_url(&input.base_url, &id, &obj, &input.token, a));
        let banner_url = auth.and_then(|a| pick_wide_art_url(&input.base_url, &id, &obj, &input.token, a));

        series.push(SeriesOut {
            id: id.clone(),
            source_id: input.source_id.clone(),
            name,
            cover,
            banner_url,
            rating: rating.clone(),
            release_date,
            genres,
            series_id: id,
            certification: rating,
            parent_id: if parent_id.is_empty() { None } else { Some(parent_id) },
            path: if path.is_empty() { None } else { Some(path) },
        });
    }

    SeriesListOutput { series }
}

pub fn parse_live_tv_channels(input: &ParseInput) -> ChannelsOutput {
    let auth = AuthType::from_str(&input.auth_type);
    let items = parse_items_array(&input.json);
    let mut channels = Vec::with_capacity(items.len());
    let base = input.base_url.trim_end_matches('/');

    for obj in items {
        let id = value_as_string(obj.get("Id"));
        if id.is_empty() {
            continue;
        }
        let name = value_as_string(obj.get("Name"));
        // TiviMate+20: never invent "Channel N" — skip nameless rows (Kotlin parity).
        if name.is_empty() {
            continue;
        }
        let number = value_as_string(obj.get("Number"));
        let display_name = if number.is_empty() { name.clone() } else { format!("{} {}", number, name) };

        let (genre, logo_url, stream_url, epg_id) = match auth {
            Some(AuthType::Jellyfin) => {
                let genre = value_as_string(obj.get("ChannelType"));
                let genre = if genre.is_empty() { "Live TV".to_string() } else { genre };
                let logo_url = if obj.get("ImageTags").is_some() {
                    format!("{}/Items/{}/Images/Primary?api_key={}", base, id, input.token)
                } else {
                    String::new()
                };
                let stream_url = format!("{}/LiveTv/Channels/{}/stream?api_key={}", base, id, input.token);
                let ext = value_as_string(obj.get("ExternalId"));
                let epg = if ext.is_empty() { id.clone() } else { ext };
                (genre, logo_url, stream_url, epg)
            }
            _ => {
                let genre = "Live TV".to_string();
                // Emby: only set logo when ImageTags.Primary is non-blank.
                let logo_url = obj.get("ImageTags")
                    .and_then(|v| v.as_object())
                    .map(|tags| value_as_string(tags.get("Primary")))
                    .filter(|s| !s.is_empty())
                    .map(|_| format!("{}/Items/{}/Images/Primary?X-Emby-Token={}", base, id, input.token))
                    .unwrap_or_default();
                let stream_url = format!("{}/LiveTv/LiveStreamFiles/{}/stream.ts?X-Emby-Token={}", base, id, input.token);
                let epg = id.clone();
                (genre, logo_url, stream_url, epg)
            }
        };

        channels.push(ChannelOut {
            id: format!("livetv:{}", id),
            source_id: input.source_id.clone(),
            name: display_name,
            genre,
            logo_url,
            stream_url,
            has_catchup: false,
            epg_id: Some(epg_id),
        });
    }

    ChannelsOutput { channels }
}

// ── JNI exports ───────────────────────────────────────────────────────────────

fn read_jstring(env: &mut Env, s: &JString) -> Option<String> {
    env.get_string(s).ok().map(|s| s.into())
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_provider_MediaServerNativeBridge_nativeParseMovies(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let empty = "{\"movies\":[]}";
        let input_str = match read_jstring(env, &input_json) {
            Some(s) => s,
            None => return Ok(env.new_string(empty).unwrap().into_raw()),
        };
        let output_json = match serde_json::from_str::<ParseInput>(&input_str) {
            Ok(input) => serde_json::to_string(&parse_movies(&input)).unwrap_or_else(|_| empty.to_string()),
            Err(_) => empty.to_string(),
        };
        env.new_string(output_json).unwrap_or_else(|_| env.new_string(empty).unwrap()).into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_provider_MediaServerNativeBridge_nativeParseSeries(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let empty = "{\"series\":[]}";
        let input_str = match read_jstring(env, &input_json) {
            Some(s) => s,
            None => return Ok(env.new_string(empty).unwrap().into_raw()),
        };
        let output_json = match serde_json::from_str::<ParseInput>(&input_str) {
            Ok(input) => serde_json::to_string(&parse_series(&input)).unwrap_or_else(|_| empty.to_string()),
            Err(_) => empty.to_string(),
        };
        env.new_string(output_json).unwrap_or_else(|_| env.new_string(empty).unwrap()).into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_provider_MediaServerNativeBridge_nativeParseLiveTvChannels(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let empty = "{\"channels\":[]}";
        let input_str = match read_jstring(env, &input_json) {
            Some(s) => s,
            None => return Ok(env.new_string(empty).unwrap().into_raw()),
        };
        let output_json = match serde_json::from_str::<ParseInput>(&input_str) {
            Ok(input) => serde_json::to_string(&parse_live_tv_channels(&input)).unwrap_or_else(|_| empty.to_string()),
            Err(_) => empty.to_string(),
        };
        env.new_string(output_json).unwrap_or_else(|_| env.new_string(empty).unwrap()).into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_provider_MediaServerNativeBridge_nativeEngineVersion(
    _env: EnvUnowned,
    _class: JClass,
) -> jint {
    ENGINE_VERSION
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn emby_input(json: &str) -> ParseInput {
        ParseInput {
            json: json.to_string(),
            source_id: "emby-source".to_string(),
            base_url: "http://emby.local:8096".to_string(),
            token: "emby-token-123".to_string(),
            auth_type: "emby".to_string(),
        }
    }

    fn jellyfin_input(json: &str) -> ParseInput {
        ParseInput {
            json: json.to_string(),
            source_id: "jellyfin-source".to_string(),
            base_url: "http://jellyfin.local:8096".to_string(),
            token: "jellyfin-token-456".to_string(),
            auth_type: "jellyfin".to_string(),
        }
    }

    #[test]
    fn parse_movies_skips_blank_id_or_name() {
        let json = r#"{"Items":[{"Id":"","Name":"No Id"},{"Id":"123","Name":""}]}"#;
        let out = parse_movies(&emby_input(json));
        assert!(out.movies.is_empty());
    }

    #[test]
    fn parse_movies_emby_builds_urls_and_fields() {
        let json = r#"{
            "Items": [{
                "Id": "movie-1",
                "Name": "The Film",
                "ProductionYear": "2024",
                "OfficialRating": "GB-PG",
                "GenreItems": [{"Name": "Action"}],
                "Container": "mkv",
                "ParentId": "lib-1",
                "Path": "/media/movies/the.film.2024.2160p.mkv",
                "ImageTags": {"Primary": "tag1"},
                "BackdropImageTags": ["tag2"]
            }]
        }"#;
        let out = parse_movies(&emby_input(json));
        assert_eq!(out.movies.len(), 1);
        let m = &out.movies[0];
        assert_eq!(m.id, "movie-1");
        assert_eq!(m.name, "The Film");
        assert_eq!(m.year, Some("2024".to_string()));
        assert_eq!(m.rating, Some("PG".to_string()));
        assert_eq!(m.certification, Some("PG".to_string()));
        assert_eq!(m.genres, vec!["Action".to_string()]);
        assert_eq!(m.container_extension, "mkv");
        assert_eq!(m.parent_id, Some("lib-1".to_string()));
        assert!(m.path.as_ref().unwrap().contains("the.film.2024.2160p.mkv"));
        assert!(m.stream_id.contains("X-Emby-Token=emby-token-123"));
        assert!(m.stream_icon.as_ref().unwrap().contains("/Items/movie-1/Images/Primary?"));
        assert!(m.banner_url.as_ref().unwrap().contains("/Items/movie-1/Images/Backdrop/0?"));
        assert_eq!(m.video_quality_label, Some("4K".to_string()));
    }

    #[test]
    fn parse_movies_jellyfin_uses_api_key() {
        let json = r#"{
            "Items": [{
                "Id": "movie-2",
                "Name": "Jelly Movie",
                "ProductionYear": "2023",
                "OfficialRating": "TV-Y7",
                "GenreItems": [{"Name": "Family"}],
                "Container": "",
                "ImageTags": {"Thumb": "tag1"}
            }]
        }"#;
        let out = parse_movies(&jellyfin_input(json));
        assert_eq!(out.movies.len(), 1);
        let m = &out.movies[0];
        assert!(m.stream_id.contains("api_key=jellyfin-token-456"));
        assert!(m.stream_icon.as_ref().unwrap().contains("/Items/movie-2/Images/Thumb?"));
        assert_eq!(m.container_extension, "mp4");
        assert_eq!(m.rating, Some("TV-Y7".to_string()));
    }

    #[test]
    fn parse_series_emby_extracts_release_date() {
        let json = r#"{
            "Items": [{
                "Id": "series-1",
                "Name": "The Show",
                "PremiereDate": "2022-06-15T00:00:00Z",
                "OfficialRating": "US-TV-14",
                "GenreItems": [{"Name": "Drama"}],
                "ImageTags": {"Primary": "tag1"}
            }]
        }"#;
        let out = parse_series(&emby_input(json));
        assert_eq!(out.series.len(), 1);
        let s = &out.series[0];
        assert_eq!(s.id, "series-1");
        assert_eq!(s.release_date, Some("2022-06-15T00:00:00Z".to_string()));
        assert_eq!(s.rating, Some("TV-14".to_string()));
        assert_eq!(s.genres, vec!["Drama".to_string()]);
        assert_eq!(s.series_id, "series-1");
    }

    #[test]
    fn parse_live_tv_channels_emby() {
        let json = r#"{
            "Items": [
                {"Id": "ch-1", "Name": "BBC One", "Number": "1", "ExternalId": "ext-1", "ImageTags": {"Primary": "tag-p"}},
                {"Id": "ch-2", "Name": "ITV", "ImageTags": {}}
            ]
        }"#;
        let out = parse_live_tv_channels(&emby_input(json));
        assert_eq!(out.channels.len(), 2);
        assert_eq!(out.channels[0].id, "livetv:ch-1");
        assert_eq!(out.channels[0].name, "1 BBC One");
        assert_eq!(out.channels[0].genre, "Live TV");
        assert!(out.channels[0].logo_url.contains("X-Emby-Token=emby-token-123"));
        assert!(out.channels[0].stream_url.contains("/LiveTv/LiveStreamFiles/ch-1/stream.ts?"));
        assert_eq!(out.channels[0].epg_id, Some("ch-1".to_string()));
        // ch-2 has ImageTags but no Primary tag → empty logo.
        assert_eq!(out.channels[1].logo_url, "");
        assert_eq!(out.channels[1].epg_id, Some("ch-2".to_string()));
    }

    #[test]
    fn parse_live_tv_channels_skips_blank_name() {
        let json = r#"{
            "Items": [
                {"Id": "ch-blank", "Name": ""},
                {"Id": "ch-ok", "Name": "BBC One"}
            ]
        }"#;
        let out = parse_live_tv_channels(&emby_input(json));
        assert_eq!(out.channels.len(), 1);
        assert_eq!(out.channels[0].id, "livetv:ch-ok");
        assert_eq!(out.channels[0].name, "BBC One");
    }

    #[test]
    fn parse_live_tv_channels_jellyfin() {
        let json = r#"{
            "Items": [
                {"Id": "ch-3", "Name": "News", "ChannelType": "News", "ImageTags": {"Primary": "t"}}
            ]
        }"#;
        let out = parse_live_tv_channels(&jellyfin_input(json));
        assert_eq!(out.channels.len(), 1);
        assert_eq!(out.channels[0].genre, "News");
        assert!(out.channels[0].logo_url.contains("api_key=jellyfin-token-456"));
        assert!(out.channels[0].stream_url.contains("api_key=jellyfin-token-456"));
    }

    #[test]
    fn parse_movies_accepts_plain_items_array() {
        let json = r#"[{"Id":"m-1","Name":"Arr Movie","ProductionYear":"2021","Container":"mp4"}]"#;
        let out = parse_movies(&emby_input(json));
        assert_eq!(out.movies.len(), 1);
        assert_eq!(out.movies[0].id, "m-1");
    }

    #[test]
    fn normalise_rating_cases() {
        assert_eq!(normalise_official_rating("GB-PG"), Some("PG".to_string()));
        assert_eq!(normalise_official_rating("US-TV-14"), Some("TV-14".to_string()));
        assert_eq!(normalise_official_rating("PG"), Some("PG".to_string()));
        assert_eq!(normalise_official_rating(""), None);
        assert_eq!(normalise_official_rating("GB-"), Some("GB-".to_string()));
    }

    #[test]
    fn genres_prefers_string_array_and_keeps_every_name() {
        let json = r#"{
            "Items": [{
                "Id": "movie-3",
                "Name": "Tenzing",
                "Genres": ["Drama", " ", "History"],
                "GenreItems": [{"Name": "Ignored"}]
            }]
        }"#;
        let out = parse_movies(&emby_input(json));
        assert_eq!(out.movies[0].genres, vec!["Drama".to_string(), "History".to_string()]);
    }

    #[test]
    fn genres_empty_when_server_sent_none() {
        let json = r#"{"Items":[{"Id":"s-9","Name":"Bare","GenreItems":[]}]}"#;
        let out = parse_series(&emby_input(json));
        assert!(out.series[0].genres.is_empty());
        let wire = serde_json::to_string(&out).unwrap();
        assert!(wire.contains("\"genres\":[]"));
        assert!(!wire.contains("categoryId"));
    }

    #[test]
    fn engine_version_is_stable() {
        assert_eq!(ENGINE_VERSION, 2);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
