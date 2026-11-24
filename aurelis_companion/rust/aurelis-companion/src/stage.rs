//! Catch-up Auto-Downloader and Episode Stager: Pre-fetches catch-up programmes and
//! series episodes to local storage on the Companion for instant, zero-buffering playback.

use crate::bundle_store::write_atomic;
use crate::state::{AppState, WallClock};
use axum::body::Body;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

pub const DIR_NAME: &str = "staged";
pub const FILES_DIR: &str = "files";
pub const STAGED_FILE: &str = "staged.json";
pub const DEFAULT_RETENTION_MS: i64 = 7 * 24 * 3600 * 1000; // 7 days
pub const STREAM_TOKEN_VALIDITY_MS: i64 = 24 * 3600 * 1000; // 24 hours

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StageStatus {
    Queued,
    Downloading,
    Ready,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedEntry {
    pub id: String,
    pub programme_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub stream_url: String,
    pub start_ms: i64,
    pub duration_sec: u64,
    pub status: StageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default)]
    pub bytes_downloaded: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleStageRequest {
    pub programme_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub title: String,
    pub description: Option<String>,
    pub stream_url: String,
    pub start_ms: i64,
    pub duration_sec: u64,
    pub expires_at_ms: Option<i64>,
}

pub struct StageStore {
    root_dir: PathBuf,
    files_dir: PathBuf,
    items: Mutex<HashMap<String, StagedEntry>>,
    wall: Mutex<WallClock>,
    used_bytes: AtomicU64,
    client: reqwest::Client,
}

impl StageStore {
    pub fn open(data_dir: &Path, wall: WallClock) -> Arc<Self> {
        let root = data_dir.join(DIR_NAME);
        let files = root.join(FILES_DIR);
        let _ = std::fs::create_dir_all(&files);

        let manifest_path = root.join(STAGED_FILE);
        let items_map = if manifest_path.exists() {
            match std::fs::read_to_string(&manifest_path) {
                Ok(text) => match serde_json::from_str::<Vec<StagedEntry>>(&text) {
                    Ok(list) => {
                        let mut m = HashMap::new();
                        for item in list {
                            m.insert(item.id.clone(), item);
                        }
                        m
                    }
                    Err(e) => {
                        eprintln!("StageStore: failed to parse manifest: {e}");
                        HashMap::new()
                    }
                },
                Err(e) => {
                    eprintln!("StageStore: failed to read manifest: {e}");
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };

        let mut initial_used = 0u64;
        for item in items_map.values() {
            if item.status == StageStatus::Ready {
                if let Some(file_name) = &item.file_name {
                    let p = files.join(file_name);
                    if let Ok(meta) = std::fs::metadata(p) {
                        initial_used = initial_used.saturating_add(meta.len());
                    }
                }
            }
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Arc::new(Self {
            root_dir: root,
            files_dir: files,
            items: Mutex::new(items_map),
            wall: Mutex::new(wall),
            used_bytes: AtomicU64::new(initial_used),
            client,
        })
    }

    #[cfg(test)]
    pub fn set_clock(&self, wall: WallClock) {
        *self.wall.lock().unwrap() = wall;
    }

    pub fn now_ms(&self) -> i64 {
        let clock = self.wall.lock().unwrap();
        (*clock)()
    }

    pub fn used_mb(&self) -> u64 {
        self.used_bytes.load(Ordering::Relaxed) / (1024 * 1024)
    }

    fn lock_items(&self) -> MutexGuard<'_, HashMap<String, StagedEntry>> {
        self.items.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn list(&self) -> Vec<StagedEntry> {
        let lock = self.lock_items();
        let mut list: Vec<StagedEntry> = lock.values().cloned().collect();
        list.sort_by_key(|e| -e.created_at_ms);
        list
    }

    pub fn get(&self, id: &str) -> Option<StagedEntry> {
        self.lock_items().get(id).cloned()
    }

    pub fn schedule(&self, req: ScheduleStageRequest) -> Result<StagedEntry, (StatusCode, String)> {
        if req.stream_url.trim().is_empty() {
            return Err((StatusCode::BAD_REQUEST, "streamUrl cannot be empty".to_string()));
        }

        let now = self.now_ms();
        let expires = req.expires_at_ms.unwrap_or(now + DEFAULT_RETENTION_MS);
        let id = format!("{}_{}", req.channel_id, req.start_ms);
        let file_name = format!("{}.ts", id);

        let mut lock = self.lock_items();
        if let Some(existing) = lock.get(&id) {
            if existing.status == StageStatus::Ready || existing.status == StageStatus::Downloading {
                return Ok(existing.clone());
            }
        }

        let entry = StagedEntry {
            id: id.clone(),
            programme_id: req.programme_id,
            channel_id: req.channel_id,
            channel_name: req.channel_name,
            title: req.title,
            description: req.description,
            stream_url: req.stream_url,
            start_ms: req.start_ms,
            duration_sec: req.duration_sec,
            status: StageStatus::Queued,
            file_name: Some(file_name),
            bytes_downloaded: 0,
            total_bytes: None,
            error_message: None,
            created_at_ms: now,
            expires_at_ms: expires,
            playback_url: Some(format!("/v1/stage/stream/{}", id)),
        };

        lock.insert(id.clone(), entry.clone());
        self.persist_locked(&lock);
        drop(lock);

        Ok(entry)
    }

    pub async fn start_download(self: &Arc<Self>, id: String) {
        let entry = match self.get(&id) {
            Some(e) => e,
            None => return,
        };

        let file_name = match &entry.file_name {
            Some(f) => f.clone(),
            None => return,
        };

        let target_path = self.files_dir.join(&file_name);

        {
            let mut lock = self.lock_items();
            if let Some(e) = lock.get_mut(&id) {
                e.status = StageStatus::Downloading;
            }
            self.persist_locked(&lock);
        }

        let req = self.client.get(&entry.stream_url)
            .header("User-Agent", "VLC/3.0.18 LibVLC/3.0.18")
            .header("Accept", "*/*");

        let resp = match req.send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                let err = format!("upstream HTTP {}", r.status());
                self.mark_failed(&id, &err);
                return;
            }
            Err(e) => {
                let err = format!("upstream connection failed: {e}");
                self.mark_failed(&id, &err);
                return;
            }
        };

        let total_bytes = resp.content_length();

        let mut file = match tokio::fs::File::create(&target_path).await {
            Ok(f) => f,
            Err(e) => {
                self.mark_failed(&id, &format!("failed to create local file: {e}"));
                return;
            }
        };

        let mut stream = resp.bytes_stream();
        let mut downloaded = 0u64;

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    if let Err(e) = file.write_all(&chunk).await {
                        self.mark_failed(&id, &format!("disk write error: {e}"));
                        return;
                    }
                    downloaded += chunk.len() as u64;
                    self.used_bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);

                    // Periodic memory state update
                    if downloaded % (512 * 1024) < chunk.len() as u64 {
                        let mut lock = self.lock_items();
                        if let Some(e) = lock.get_mut(&id) {
                            e.bytes_downloaded = downloaded;
                            e.total_bytes = total_bytes;
                        }
                    }
                }
                Err(e) => {
                    self.mark_failed(&id, &format!("stream error: {e}"));
                    return;
                }
            }
        }

        if let Err(e) = file.flush().await {
            self.mark_failed(&id, &format!("file flush error: {e}"));
            return;
        }

        {
            let mut lock = self.lock_items();
            if let Some(e) = lock.get_mut(&id) {
                e.status = StageStatus::Ready;
                e.bytes_downloaded = downloaded;
                e.total_bytes = total_bytes.or(Some(downloaded));
            }
            self.persist_locked(&lock);
        }
    }

    fn mark_failed(&self, id: &str, error: &str) {
        let mut lock = self.lock_items();
        if let Some(e) = lock.get_mut(id) {
            e.status = StageStatus::Failed;
            e.error_message = Some(error.to_string());
        }
        self.persist_locked(&lock);
    }

    pub fn delete(&self, id: &str) -> Result<(), (StatusCode, String)> {
        let mut lock = self.lock_items();
        let entry = lock.remove(id).ok_or_else(|| (StatusCode::NOT_FOUND, "item not found".to_string()))?;

        if let Some(f) = entry.file_name {
            let p = self.files_dir.join(f);
            if let Ok(meta) = std::fs::metadata(&p) {
                self.used_bytes.fetch_sub(meta.len(), Ordering::Relaxed);
            }
            let _ = std::fs::remove_file(p);
        }

        self.persist_locked(&lock);
        Ok(())
    }

    pub fn prune_expired(&self) -> usize {
        let now = self.now_ms();
        let mut to_remove = Vec::new();

        {
            let lock = self.lock_items();
            for (id, item) in lock.iter() {
                if item.expires_at_ms > 0 && item.expires_at_ms < now {
                    to_remove.push(id.clone());
                }
            }
        }

        let mut count = 0;
        for id in to_remove {
            if self.delete(&id).is_ok() {
                count += 1;
            }
        }
        count
    }

    pub async fn reset(&self) {
        let mut lock = self.lock_items();
        lock.clear();
        let _ = std::fs::remove_dir_all(&self.files_dir);
        let _ = std::fs::create_dir_all(&self.files_dir);
        self.used_bytes.store(0, Ordering::Relaxed);
        self.persist_locked(&lock);
    }

    fn persist_locked(&self, lock: &HashMap<String, StagedEntry>) {
        let list: Vec<&StagedEntry> = lock.values().collect();
        if let Ok(json) = serde_json::to_vec_pretty(&list) {
            let _ = write_atomic(&self.root_dir, STAGED_FILE, &json);
        }
    }

    pub fn generate_stream_token(&self, secret: &[u8; 32], id: &str, expires_at_ms: i64) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts 32-byte key");
        mac.update(format!("{id}:{expires_at_ms}").as_bytes());
        let expected = mac.finalize().into_bytes();
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD_NO_PAD, expected)
    }

    pub fn verify_stream_token(&self, secret: &[u8; 32], id: &str, token: &str, expires_at_ms: i64, now_ms: i64) -> bool {
        if expires_at_ms < now_ms {
            return false;
        }
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts 32-byte key");
        mac.update(format!("{id}:{expires_at_ms}").as_bytes());
        let expected = mac.finalize().into_bytes();
        let expected_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD_NO_PAD, expected);
        expected_b64 == token
    }
}

fn urlencoding_simple(s: &str) -> String {
    s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
}

// ── HTTP Handlers ─────────────────────────────────────────────────────────────

pub async fn schedule_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
    body: axum::body::Bytes,
) -> Response {
    let req: ScheduleStageRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return session.sealed_reason(StatusCode::BAD_REQUEST, &format!("invalid request JSON: {e}")),
    };
    match state.stage.schedule(req) {
        Ok(entry) => {
            let s = state.clone();
            let id = entry.id.clone();
            tokio::spawn(async move {
                s.stage.start_download(id).await;
            });
            session.sealed_json(StatusCode::CREATED, &serde_json::to_value(&entry).unwrap_or_default())
        }
        Err((code, msg)) => session.sealed_reason(code, &msg),
    }
}

pub async fn list_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
) -> Response {
    let mut list = state.stage.list();
    let now = state.wall_ms();
    let expires = now + STREAM_TOKEN_VALIDITY_MS;
    let secret = session.secret;

    for entry in &mut list {
        if entry.status == StageStatus::Ready {
            let token = state.stage.generate_stream_token(&secret, &entry.id, expires);
            let encoded_token = urlencoding_simple(&token);
            entry.playback_url = Some(format!("/v1/stage/stream/{}?token={}&expires={}", entry.id, encoded_token, expires));
        }
    }

    session.sealed_json(StatusCode::OK, &serde_json::to_value(&list).unwrap_or_default())
}

pub async fn delete_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    match state.stage.delete(&id) {
        Ok(()) => session.sealed_json(StatusCode::OK, &serde_json::json!({ "deleted": true })),
        Err((code, msg)) => session.sealed_reason(code, &msg),
    }
}

pub async fn stream_route(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    req: Request<Body>,
) -> Response {
    let (token_opt, expires_opt) = parse_token_query(req.uri().query());

    let authorized = match (token_opt, expires_opt) {
        (Some(token), Some(expires)) => {
            if let Some(secret) = state.auth.household_secret() {
                state.stage.verify_stream_token(&secret, &id, &token, expires, state.wall_ms())
            } else {
                false
            }
        }
        _ => false,
    };

    if !authorized {
        return (StatusCode::UNAUTHORIZED, "invalid or expired token").into_response();
    }

    let entry = match state.stage.get(&id) {
        Some(e) if e.status == StageStatus::Ready => e,
        Some(_) => return (StatusCode::ACCEPTED, "item is still downloading").into_response(),
        None => return (StatusCode::NOT_FOUND, "item not found").into_response(),
    };

    let file_name = match entry.file_name {
        Some(f) => f,
        None => return (StatusCode::NOT_FOUND, "staged file missing").into_response(),
    };

    let path = state.stage.files_dir.join(file_name);
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "file not found on disk").into_response(),
    };

    let meta = match file.metadata().await {
        Ok(m) => m,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let total_len = meta.len();
    let range_header = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(String::from);

    if let Some(range_str) = range_header.as_deref() {
        if let Some((start, end)) = parse_range_spec(range_str, total_len) {
            let length = end - start + 1;
            if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            let stream = ReaderStream::new(file.take(length));
            let content_range = format!("bytes {start}-{end}/{total_len}");

            return (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, "video/mp2t"),
                    (header::CONTENT_RANGE, content_range.as_str()),
                    (header::CONTENT_LENGTH, length.to_string().as_str()),
                    (header::ACCEPT_RANGES, "bytes"),
                ],
                Body::from_stream(stream),
            ).into_response();
        }
    }

    let stream = ReaderStream::new(file);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CONTENT_LENGTH, total_len.to_string().as_str()),
            (header::ACCEPT_RANGES, "bytes"),
        ],
        Body::from_stream(stream),
    ).into_response()
}

fn parse_token_query(q: Option<&str>) -> (Option<String>, Option<i64>) {
    let mut token = None;
    let mut expires = None;
    if let Some(query) = q {
        for part in query.split('&') {
            if let Some((k, v)) = part.split_once('=') {
                if k == "token" {
                    token = Some(v.to_string());
                } else if k == "expires" {
                    expires = v.parse::<i64>().ok();
                }
            }
        }
    }
    (token, expires)
}

fn parse_range_spec(spec: &str, total: u64) -> Option<(u64, u64)> {
    let s = spec.strip_prefix("bytes=")?;
    let mut parts = s.split('-');
    let start_str = parts.next()?.trim();
    let end_str = parts.next()?.trim();

    if start_str.is_empty() {
        let suffix: u64 = end_str.parse().ok()?;
        if suffix == 0 || suffix > total {
            Some((0, total - 1))
        } else {
            Some((total - suffix, total - 1))
        }
    } else {
        let start: u64 = start_str.parse().ok()?;
        if start >= total {
            return None;
        }
        let end = if end_str.is_empty() {
            total - 1
        } else {
            let e: u64 = end_str.parse().ok()?;
            e.min(total - 1)
        };
        if start <= end {
            Some((start, end))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_creates_queued_entry_and_persists() {
        let tmp = std::env::temp_dir().join(format!("aurelis-stage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = StageStore::open(&tmp, Arc::new(|| 1_000_000));

        let req = ScheduleStageRequest {
            programme_id: "prog-100".into(),
            channel_id: "bbc1".into(),
            channel_name: "BBC One".into(),
            title: "EastEnders".into(),
            description: Some("Soap opera episode".into()),
            stream_url: "http://upstream.com/catchup/100.ts".into(),
            start_ms: 1_000_000,
            duration_sec: 1800,
            expires_at_ms: None,
        };

        let entry = store.schedule(req).expect("schedule succeeds");
        assert_eq!(entry.status, StageStatus::Queued);
        assert_eq!(entry.id, "bbc1_1000000");
        assert_eq!(entry.file_name, Some("bbc1_1000000.ts".to_string()));

        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "bbc1_1000000");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn delete_removes_entry_and_file() {
        let tmp = std::env::temp_dir().join(format!("aurelis-stage-del-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = StageStore::open(&tmp, Arc::new(|| 1_000_000));

        let req = ScheduleStageRequest {
            programme_id: "prog-200".into(),
            channel_id: "itv1".into(),
            channel_name: "ITV 1".into(),
            title: "News".into(),
            description: None,
            stream_url: "http://upstream.com/catchup/200.ts".into(),
            start_ms: 2_000_000,
            duration_sec: 1800,
            expires_at_ms: None,
        };

        let entry = store.schedule(req).unwrap();
        assert_eq!(store.list().len(), 1);

        store.delete(&entry.id).unwrap();
        assert_eq!(store.list().len(), 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
