//! Lineup & Channel Overrides Hub.
//!
//! Provides household-wide channel management:
//! 1. Manual channel overrides (permanently moving channels to different countries or groups,
//!    custom display names, pinned positions, channel numbers per country group, EPG IDs, or hiding).
//! 2. Channel catalog reporting from TV boxes.
//! 3. Two-way synchronization between Home Assistant and Aurelis TV boxes.
//! 4. Automatic & manual channel numbering per country group.

use axum::extract::{Path as UrlPath, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use crate::state::{AppState, WallClock};
use crate::bundle_store::write_atomic;

const OVERRIDES_FILE: &str = "lineup_overrides.json";
const CHANNELS_FILE: &str = "lineup_channels.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelOverride {
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_number: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSummary {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_number: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveOverrideRequest {
    pub channel_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub position: Option<i32>,
    #[serde(default)]
    pub channel_number: Option<i32>,
    #[serde(default)]
    pub epg_id: Option<String>,
    #[serde(default)]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub updated_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelReportRequest {
    #[serde(default)]
    pub device_id: Option<String>,
    pub channels: Vec<ChannelSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignNumbersRequest {
    /// Target country code (e.g. "GB", "US"). If omitted, all countries are processed.
    #[serde(default)]
    pub country_code: Option<String>,
    /// Starting channel number (e.g. 101 for GB, 1 for US). Defaults to standard country base or 1.
    #[serde(default)]
    pub start_number: Option<i32>,
    /// Whether to overwrite existing custom channel numbers. Defaults to false.
    #[serde(default)]
    pub overwrite_existing: Option<bool>,
}

#[derive(Default, Debug, Clone)]
pub struct ChannelQuery {
    pub country: Option<String>,
    pub category: Option<String>,
    pub search: Option<String>,
    pub page: Option<usize>,
    pub edited_only: bool,
}

pub fn all_aurelis_categories() -> &'static [&'static str] {
    &[
        "Local",
        "Regional",
        "Entertainment",
        "Documentaries",
        "Movies",
        "Sport",
        "UHD / 4K",
        "News",
        "Children",
        "Music",
        "Shopping",
        "Religious",
        "Radio",
        "International",
        "Adult",
        "Others",
    ]
}

pub fn aurelis_category_label(cat: aurelis_heuristics::ContentCategory) -> &'static str {
    use aurelis_heuristics::ContentCategory;
    match cat {
        ContentCategory::Local => "Local",
        ContentCategory::Regional => "Regional",
        ContentCategory::Entertainment => "Entertainment",
        ContentCategory::Documentaries => "Documentaries",
        ContentCategory::Movies => "Movies",
        ContentCategory::Sport => "Sport",
        ContentCategory::Uhd => "UHD / 4K",
        ContentCategory::News => "News",
        ContentCategory::Children => "Children",
        ContentCategory::Music => "Music",
        ContentCategory::Shopping => "Shopping",
        ContentCategory::Religious => "Religious",
        ContentCategory::Radio => "Radio",
        ContentCategory::International => "International",
        ContentCategory::Adult => "Adult",
        ContentCategory::Hidden => "Hidden",
        ContentCategory::Others => "Others",
    }
}

pub fn resolve_aurelis_category(ch: &ChannelSummary) -> &'static str {
    let cat = aurelis_heuristics::provider_genre_mapper::categorize(
        ch.country_code.as_deref(),
        ch.genre.as_deref().unwrap_or(""),
        &ch.name,
        None,
        None,
    );
    aurelis_category_label(cat)
}

pub fn matches_category_filter(ch: &ChannelSummary, filter: &str) -> bool {
    let f = filter.trim().to_lowercase();
    if f.is_empty() {
        return true;
    }
    let aurelis = resolve_aurelis_category(ch).to_lowercase();
    if aurelis == f || aurelis.contains(&f) {
        return true;
    }
    if ch.genre.as_deref().map(|g| g.to_lowercase().contains(&f)) == Some(true) {
        return true;
    }
    ch.original_genre.as_deref().map(|g| g.to_lowercase().contains(&f)) == Some(true)
}

pub fn urlencoding_decode(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '+' {
            out.push(' ');
        } else if c == '%' {
            let h1 = chars.next().unwrap_or(' ');
            let h2 = chars.next().unwrap_or(' ');
            let hex = format!("{h1}{h2}");
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                out.push(byte as char);
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn parse_channel_query(raw_query: &str) -> ChannelQuery {
    let mut q = ChannelQuery::default();
    for part in raw_query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            let val = urlencoding_decode(v);
            if !val.trim().is_empty() {
                match k {
                    "country" => q.country = Some(val),
                    "category" => q.category = Some(val),
                    "search" | "q" => q.search = Some(val),
                    "page" => q.page = val.trim().parse::<usize>().ok().map(|p| p.max(1)),
                    "edited" => q.edited_only = matches!(val.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"),
                    _ => {}
                }
            }
        }
    }
    q
}

#[derive(Serialize, Deserialize)]
struct StoredOverrides {
    version: u32,
    overrides: Vec<ChannelOverride>,
}

#[derive(Serialize, Deserialize)]
struct StoredChannels {
    version: u32,
    channels: Vec<ChannelSummary>,
}

pub struct LineupStore {
    dir: PathBuf,
    overrides: Mutex<HashMap<String, ChannelOverride>>,
    channels: Mutex<HashMap<String, ChannelSummary>>,
    wall: WallClock,
}

impl LineupStore {
    pub fn new(dir: PathBuf, wall: WallClock) -> Self {
        let _ = std::fs::create_dir_all(&dir);

        let overrides = {
            let path = dir.join(OVERRIDES_FILE);
            let mut map = HashMap::new();
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(stored) = serde_json::from_slice::<StoredOverrides>(&bytes) {
                    for ov in stored.overrides {
                        map.insert(ov.channel_id.clone(), ov);
                    }
                }
            }
            Mutex::new(map)
        };

        let channels = {
            let path = dir.join(CHANNELS_FILE);
            let mut map = HashMap::new();
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(stored) = serde_json::from_slice::<StoredChannels>(&bytes) {
                    for ch in stored.channels {
                        map.insert(ch.id.clone(), ch);
                    }
                }
            }
            Mutex::new(map)
        };

        Self {
            dir,
            overrides,
            channels,
            wall,
        }
    }

    fn persist_overrides(&self, map: &HashMap<String, ChannelOverride>) -> Result<(), String> {
        let mut list: Vec<ChannelOverride> = map.values().cloned().collect();
        list.sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
        let stored = StoredOverrides {
            version: 1,
            overrides: list,
        };
        let bytes = serde_json::to_vec_pretty(&stored).map_err(|e| e.to_string())?;
        write_atomic(&self.dir, OVERRIDES_FILE, &bytes)
    }

    fn persist_channels(&self, map: &HashMap<String, ChannelSummary>) -> Result<(), String> {
        let mut list: Vec<ChannelSummary> = map.values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        let stored = StoredChannels {
            version: 1,
            channels: list,
        };
        let bytes = serde_json::to_vec_pretty(&stored).map_err(|e| e.to_string())?;
        write_atomic(&self.dir, CHANNELS_FILE, &bytes)
    }

    pub fn get_overrides(&self) -> Vec<ChannelOverride> {
        let lock = self.overrides.lock().unwrap();
        let mut list: Vec<ChannelOverride> = lock.values().cloned().collect();
        list.sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
        list
    }

    pub fn get_override(&self, channel_id: &str) -> Option<ChannelOverride> {
        let lock = self.overrides.lock().unwrap();
        lock.get(channel_id).cloned()
    }

    pub fn set_override(&self, req: SaveOverrideRequest) -> Result<ChannelOverride, String> {
        let channel_id = req.channel_id.trim().to_string();
        if channel_id.is_empty() {
            return Err("channelId cannot be empty".to_string());
        }

        let now = (self.wall)();
        let mut lock = self.overrides.lock().unwrap();

        let norm_country = req.country_code.as_deref().and_then(|cc| {
            let t = cc.trim().to_uppercase();
            if t.is_empty() { None } else { Some(t) }
        });

        let norm_category = req.category.as_deref().and_then(|c| {
            let t = c.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        });

        let norm_name = req.display_name.as_deref().and_then(|n| {
            let t = n.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        });

        let norm_epg = req.epg_id.as_deref().and_then(|e| {
            let t = e.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        });

        // Merge with existing override if present
        let mut existing = lock.get(&channel_id).cloned().unwrap_or_else(|| ChannelOverride {
            channel_id: channel_id.clone(),
            display_name: None,
            category: None,
            country_code: None,
            position: None,
            channel_number: None,
            epg_id: None,
            hidden: None,
            updated_at_ms: now,
        });

        if req.display_name.is_some() {
            existing.display_name = norm_name;
        }
        if req.category.is_some() {
            existing.category = norm_category;
        }
        if req.country_code.is_some() {
            existing.country_code = norm_country;
        }
        if req.position.is_some() {
            existing.position = req.position;
        }
        if req.channel_number.is_some() {
            existing.channel_number = req.channel_number;
        }
        if req.epg_id.is_some() {
            existing.epg_id = norm_epg;
        }
        if req.hidden.is_some() {
            existing.hidden = req.hidden;
        }
        existing.updated_at_ms = req.updated_at_ms.unwrap_or(now);

        lock.insert(channel_id, existing.clone());
        self.persist_overrides(&lock)?;
        Ok(existing)
    }

    pub fn delete_override(&self, channel_id: &str) -> Result<bool, String> {
        let mut lock = self.overrides.lock().unwrap();
        if lock.remove(channel_id).is_some() {
            self.persist_overrides(&lock)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn report_channels(&self, channels: Vec<ChannelSummary>) -> Result<usize, String> {
        let now = (self.wall)();
        let mut lock = self.channels.lock().unwrap();
        let mut updated = 0;
        for mut ch in channels {
            let id = ch.id.trim().to_string();
            if id.is_empty() {
                continue;
            }
            ch.id = id.clone();
            if ch.updated_at_ms <= 0 {
                ch.updated_at_ms = now;
            }
            lock.insert(id, ch);
            updated += 1;
        }
        self.persist_channels(&lock)?;
        Ok(updated)
    }

    pub fn get_channels(&self) -> Vec<ChannelSummary> {
        let channels_lock = self.channels.lock().unwrap();
        let overrides_lock = self.overrides.lock().unwrap();

        let mut out = Vec::with_capacity(channels_lock.len());
        for ch in channels_lock.values() {
            let mut effective = ch.clone();
            if let Some(ov) = overrides_lock.get(&ch.id) {
                if let Some(name) = &ov.display_name {
                    effective.original_name = Some(ch.name.clone());
                    effective.name = name.clone();
                }
                if let Some(cat) = &ov.category {
                    effective.original_genre = ch.genre.clone();
                    effective.genre = Some(cat.clone());
                }
                if let Some(cc) = &ov.country_code {
                    effective.country_code = Some(cc.clone());
                }
                if let Some(num) = ov.channel_number {
                    effective.channel_number = Some(num);
                }
                if let Some(h) = ov.hidden {
                    effective.hidden = Some(h);
                }
            }
            out.push(effective);
        }
        out.sort_by(|a, b| {
            // Sort by country code, then channel number (if set), then name
            match (a.country_code.as_deref(), b.country_code.as_deref()) {
                (Some(c1), Some(c2)) if c1 != c2 => c1.cmp(c2),
                _ => match (a.channel_number, b.channel_number) {
                    (Some(n1), Some(n2)) if n1 != n2 => n1.cmp(&n2),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    _ => a.name.cmp(&b.name),
                },
            }
        });
        out
    }

    /// Automatically assigns channel numbers per country group.
    ///
    /// Standard base numbers:
    /// - UK (GB): 101+
    /// - US: 1+
    /// - IE: 101+
    /// - Default: 1+
    pub fn assign_channel_numbers(&self, req: AssignNumbersRequest) -> Result<usize, String> {
        let now = (self.wall)();
        let target_country = req.country_code.as_deref().map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty());
        let overwrite = req.overwrite_existing.unwrap_or(false);

        let channels = self.get_channels();
        let mut grouped: HashMap<String, Vec<ChannelSummary>> = HashMap::new();
        for ch in channels {
            let country = ch.country_code.as_deref().unwrap_or("UNKNOWN").to_uppercase();
            if let Some(ref target) = target_country {
                if &country != target {
                    continue;
                }
            }
            grouped.entry(country).or_default().push(ch);
        }

        let mut lock = self.overrides.lock().unwrap();
        let mut assigned_count = 0;

        for (country, mut list) in grouped {
            list.sort_by(|a, b| a.name.cmp(&b.name));

            let base_num = req.start_number.unwrap_or_else(|| match country.as_str() {
                "GB" | "UK" | "IE" => 101,
                _ => 1,
            });

            let mut current_num = base_num;
            for ch in list {
                let existing_ov = lock.get(&ch.id);
                let has_num = existing_ov.and_then(|o| o.channel_number).is_some() || ch.channel_number.is_some();
                if has_num && !overwrite {
                    continue;
                }

                let mut ov = existing_ov.cloned().unwrap_or_else(|| ChannelOverride {
                    channel_id: ch.id.clone(),
                    display_name: None,
                    category: None,
                    country_code: Some(country.clone()),
                    position: None,
                    channel_number: None,
                    epg_id: None,
                    hidden: None,
                    updated_at_ms: now,
                });

                ov.channel_number = Some(current_num);
                ov.updated_at_ms = now;
                lock.insert(ch.id.clone(), ov);
                current_num += 1;
                assigned_count += 1;
            }
        }

        if assigned_count > 0 {
            self.persist_overrides(&lock)?;
        }
        Ok(assigned_count)
    }

    pub fn overrides_count(&self) -> usize {
        self.overrides.lock().unwrap().len()
    }

    pub fn channels_count(&self) -> usize {
        self.channels.lock().unwrap().len()
    }

    pub fn clear(&self) -> Result<(), String> {
        {
            let mut o = self.overrides.lock().unwrap();
            o.clear();
            let _ = self.persist_overrides(&o);
        }
        {
            let mut c = self.channels.lock().unwrap();
            c.clear();
            let _ = self.persist_channels(&c);
        }
        let _ = std::fs::remove_file(self.dir.join(OVERRIDES_FILE));
        let _ = std::fs::remove_file(self.dir.join(CHANNELS_FILE));
        let _ = std::fs::remove_dir_all(self.dir.join("arbiter_cache"));
        Ok(())
    }

    fn sanitize_arbiter_key(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.len() < 8 || trimmed.len() > 128 {
            return None;
        }
        if trimmed.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            Some(trimmed.to_string())
        } else {
            None
        }
    }

    pub fn get_arbiter_blob(&self, key: &str) -> Option<Vec<u8>> {
        let clean = Self::sanitize_arbiter_key(key)?;
        let path = self.dir.join("arbiter_cache").join(format!("{clean}.bin"));
        std::fs::read(path).ok()
    }

    pub fn put_arbiter_blob(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        let clean = Self::sanitize_arbiter_key(key).ok_or_else(|| "invalid arbiter key".to_string())?;
        if bytes.is_empty() {
            return Err("empty arbiter payload".to_string());
        }
        let cache_dir = self.dir.join("arbiter_cache");
        std::fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?;
        let file_name = format!("{clean}.bin");
        write_atomic(&cache_dir, &file_name, bytes).map_err(|e| e.to_string())?;

        // Retain only the 3 most recently modified arbiter snapshots.
        if let Ok(entries) = std::fs::read_dir(&cache_dir) {
            let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("bin") {
                        let m = e.metadata().and_then(|md| md.modified()).unwrap_or(std::time::UNIX_EPOCH);
                        Some((m, p))
                    } else {
                        None
                    }
                })
                .collect();
            if files.len() > 3 {
                files.sort_by(|a, b| b.0.cmp(&a.0));
                for (_, old_path) in files.into_iter().skip(3) {
                    let _ = std::fs::remove_file(old_path);
                }
            }
        }
        Ok(())
    }
}

// HTTP Route Handlers

fn json_resp(status: StatusCode, body: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

pub async fn handle_get_overrides(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let overrides = state.lineup.get_overrides();
    let updated_at_ms = overrides.iter().map(|o| o.updated_at_ms).max().unwrap_or(0);
    json_resp(
        StatusCode::OK,
        serde_json::json!({
            "overrides": overrides,
            "updatedAtMs": updated_at_ms,
        }),
    )
}

pub async fn handle_save_override(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    let req: SaveOverrideRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(err) => {
            return json_resp(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "reason": err.to_string() }),
            );
        }
    };
    let ch_id = req.channel_id.clone();
    match state.lineup.set_override(req) {
        Ok(entry) => {
            state.push_job(&format!("lineup: channel '{}' override saved", ch_id));
            json_resp(StatusCode::OK, serde_json::json!({ "ok": true, "override": entry }))
        }
        Err(err) => json_resp(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "ok": false, "reason": err }),
        ),
    }
}

pub async fn handle_delete_override(
    State(state): State<Arc<AppState>>,
    UrlPath(channel_id): UrlPath<String>,
) -> Response {
    match state.lineup.delete_override(&channel_id) {
        Ok(true) => {
            state.push_job(&format!("lineup: channel '{}' override removed", channel_id));
            json_resp(StatusCode::OK, serde_json::json!({ "ok": true, "deleted": true }))
        }
        Ok(false) => json_resp(
            StatusCode::NOT_FOUND,
            serde_json::json!({ "ok": false, "reason": "override not found" }),
        ),
        Err(err) => json_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "reason": err }),
        ),
    }
}

pub async fn handle_get_channels(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> impl IntoResponse {
    let raw_query = req.uri().query().unwrap_or("");
    let query = parse_channel_query(raw_query);
    let mut channels = state.lineup.get_channels();

    if let Some(c) = query.country.as_deref().map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty()) {
        channels.retain(|ch| ch.country_code.as_deref().map(|cc| cc.to_uppercase()) == Some(c.clone()));
    }

    if let Some(cat) = query.category.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        channels.retain(|ch| matches_category_filter(ch, cat));
    }

    if let Some(q) = query.search.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
        channels.retain(|ch| {
            ch.name.to_lowercase().contains(&q)
                || ch.id.to_lowercase().contains(&q)
                || ch.genre.as_deref().map(|g| g.to_lowercase().contains(&q)).unwrap_or(false)
        });
    }

    json_resp(
        StatusCode::OK,
        serde_json::json!({
            "channels": channels,
            "count": channels.len(),
        }),
    )
}

pub async fn handle_report_channels(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    let req: ChannelReportRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(err) => {
            return json_resp(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "reason": err.to_string() }),
            );
        }
    };
    let dev = req.device_id.as_deref().unwrap_or("unknown");
    match state.lineup.report_channels(req.channels) {
        Ok(count) => {
            state.push_job(&format!("lineup: device '{}' reported {} channels", dev, count));
            json_resp(
                StatusCode::OK,
                serde_json::json!({ "ok": true, "reported": count, "saved": count, "totalKnown": state.lineup.channels_count() }),
            )
        }
        Err(err) => json_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "reason": err }),
        ),
    }
}

pub async fn handle_get_arbiter(
    State(state): State<Arc<AppState>>,
    UrlPath(key): UrlPath<String>,
) -> Response {
    match state.lineup.get_arbiter_blob(&key) {
        Some(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/octet-stream")
            .body(axum::body::Body::from(bytes))
            .unwrap(),
        None => json_resp(
            StatusCode::NOT_FOUND,
            serde_json::json!({ "ok": false, "reason": "arbiter snapshot not found" }),
        ),
    }
}

pub async fn handle_put_arbiter(
    State(state): State<Arc<AppState>>,
    UrlPath(key): UrlPath<String>,
    body: axum::body::Bytes,
) -> Response {
    match state.lineup.put_arbiter_blob(&key, &body) {
        Ok(()) => {
            state.push_job(&format!("lineup: stored shared arbiter snapshot ({} bytes)", body.len()));
            json_resp(StatusCode::OK, serde_json::json!({ "ok": true, "bytes": body.len() }))
        }
        Err(err) => json_resp(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "ok": false, "reason": err }),
        ),
    }
}

pub async fn handle_assign_numbers(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    let req: AssignNumbersRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(err) => {
            return json_resp(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "reason": err.to_string() }),
            );
        }
    };
    match state.lineup.assign_channel_numbers(req) {
        Ok(count) => {
            state.push_job(&format!("lineup: assigned numbers to {} channels", count));
            json_resp(StatusCode::OK, serde_json::json!({ "ok": true, "assigned": count }))
        }
        Err(err) => json_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "reason": err }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_test_dir() -> TestDir {
        let p = std::env::temp_dir().join(format!("aurelis-lineup-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&p).expect("temp dir");
        TestDir(p)
    }

    fn test_clock(ms: i64) -> WallClock {
        Arc::new(move || ms)
    }

    #[test]
    fn test_override_crud_and_persistence() {
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));

        assert_eq!(store.get_overrides().len(), 0);

        let req = SaveOverrideRequest {
            channel_id: "bbc_one_london".into(),
            display_name: Some("BBC One HD".into()),
            category: Some("Entertainment".into()),
            country_code: Some("gb".into()),
            position: Some(1),
            channel_number: Some(101),
            epg_id: Some("bbc.one.london".into()),
            hidden: Some(false),
            updated_at_ms: None,
        };

        let saved = store.set_override(req).unwrap();
        assert_eq!(saved.channel_id, "bbc_one_london");
        assert_eq!(saved.country_code, Some("GB".into()));
        assert_eq!(saved.category, Some("Entertainment".into()));
        assert_eq!(saved.position, Some(1));
        assert_eq!(saved.channel_number, Some(101));
        assert_eq!(store.overrides_count(), 1);

        // Reload store from disk
        let reloaded = LineupStore::new(dir.0.clone(), test_clock(2000));
        let retrieved = reloaded.get_override("bbc_one_london").unwrap();
        assert_eq!(retrieved.display_name, Some("BBC One HD".into()));
        assert_eq!(retrieved.country_code, Some("GB".into()));
        assert_eq!(retrieved.channel_number, Some(101));

        // Delete override
        assert!(reloaded.delete_override("bbc_one_london").unwrap());
        assert_eq!(reloaded.overrides_count(), 0);
        assert!(reloaded.get_override("bbc_one_london").is_none());
    }

    #[test]
    fn test_channels_reporting_and_effective_override() {
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));

        let ch = ChannelSummary {
            id: "ch_sky_sports".into(),
            name: "Sky Sports ME UK FHD".into(),
            original_name: None,
            original_genre: None,
            genre: Some("General Sports".into()),
            country_code: Some("US".into()), // Wrong auto-country
            channel_number: None,
            source_id: Some("src1".into()),
            source_type: Some("xtream".into()),
            logo_url: None,
            hidden: None,
            updated_at_ms: 0,
        };

        store.report_channels(vec![ch]).unwrap();
        assert_eq!(store.channels_count(), 1);

        // Before override: effective is what was reported
        let list1 = store.get_channels();
        assert_eq!(list1[0].country_code, Some("US".into()));
        assert_eq!(list1[0].name, "Sky Sports ME UK FHD");

        // Set permanent override to UK and clean name, plus channel number 401
        store.set_override(SaveOverrideRequest {
            channel_id: "ch_sky_sports".into(),
            display_name: Some("Sky Sports Main Event".into()),
            category: Some("Sports".into()),
            country_code: Some("GB".into()),
            position: Some(10),
            channel_number: Some(401),
            epg_id: None,
            hidden: None,
            updated_at_ms: None,
        }).unwrap();

        // After override: effective has permanent moves applied
        let list2 = store.get_channels();
        assert_eq!(list2[0].country_code, Some("GB".into()));
        assert_eq!(list2[0].genre, Some("Sports".into()));
        assert_eq!(list2[0].name, "Sky Sports Main Event");
        assert_eq!(list2[0].channel_number, Some(401));
        assert_eq!(list2[0].original_name, Some("Sky Sports ME UK FHD".into()));
    }

    #[test]
    fn test_assign_channel_numbers_per_country() {
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));

        let ch1 = ChannelSummary {
            id: "gb_bbc1".into(),
            name: "BBC One".into(),
            original_name: None,
            original_genre: None,
            genre: Some("General".into()),
            country_code: Some("GB".into()),
            channel_number: None,
            source_id: None,
            source_type: None,
            logo_url: None,
            hidden: None,
            updated_at_ms: 0,
        };
        let ch2 = ChannelSummary {
            id: "gb_itv1".into(),
            name: "ITV1".into(),
            original_name: None,
            original_genre: None,
            genre: Some("General".into()),
            country_code: Some("GB".into()),
            channel_number: None,
            source_id: None,
            source_type: None,
            logo_url: None,
            hidden: None,
            updated_at_ms: 0,
        };
        let ch3 = ChannelSummary {
            id: "us_nbc".into(),
            name: "NBC East".into(),
            original_name: None,
            original_genre: None,
            genre: Some("General".into()),
            country_code: Some("US".into()),
            channel_number: None,
            source_id: None,
            source_type: None,
            logo_url: None,
            hidden: None,
            updated_at_ms: 0,
        };

        store.report_channels(vec![ch1, ch2, ch3]).unwrap();

        // Assign GB channels (default start 101)
        let count_gb = store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("GB".into()),
            start_number: None,
            overwrite_existing: None,
        }).unwrap();
        assert_eq!(count_gb, 2);

        // Assign US channels (start 1)
        let count_us = store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("US".into()),
            start_number: Some(1),
            overwrite_existing: None,
        }).unwrap();
        assert_eq!(count_us, 1);

        let channels = store.get_channels();
        let bbc = channels.iter().find(|c| c.id == "gb_bbc1").unwrap();
        let itv = channels.iter().find(|c| c.id == "gb_itv1").unwrap();
        let nbc = channels.iter().find(|c| c.id == "us_nbc").unwrap();

        assert_eq!(bbc.channel_number, Some(101));
        assert_eq!(itv.channel_number, Some(102));
        assert_eq!(nbc.channel_number, Some(1));
    }
}
