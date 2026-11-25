//! Headless Household DVR: scheduled captures, metadata persistence, stream pump and range streaming.

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

pub const DIR_NAME: &str = "dvr";
pub const FILES_DIR: &str = "files";
pub const RECORDINGS_FILE: &str = "recordings.json";
pub const STREAM_TOKEN_VALIDITY_MS: i64 = 24 * 3600 * 1000; // 24 hours
pub const AUTO_PRUNE_WATERMARK_MB: u64 = 5120; // 5 GB floor before auto-pruning old unprotected captures

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingStatus {
    Scheduled,
    Recording,
    Finished,
    Partial,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingEntry {
    pub id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub stream_url: String,
    pub scheduled_start_ms: i64,
    pub scheduled_end_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_start_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_end_ms: Option<i64>,
    pub status: RecordingStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default)]
    pub bytes_written: u64,
    pub scheduled_by_device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_url: Option<String>,
    #[serde(default)]
    pub protected: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRequest {
    pub channel_id: String,
    pub channel_name: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub stream_url: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub device_name: String,
}

#[derive(Serialize, Deserialize)]
struct StoredRecordings {
    version: u32,
    recordings: Vec<RecordingEntry>,
}

pub struct DvrStore {
    dir: PathBuf,
    files_dir: PathBuf,
    recordings: Mutex<HashMap<String, RecordingEntry>>,
    gate: tokio::sync::Mutex<()>,
    wall: WallClock,
    total_bytes: AtomicU64,
}

fn generate_id() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl DvrStore {
    pub fn open(data_dir: &Path, wall: WallClock) -> Arc<DvrStore> {
        let dir = data_dir.join(DIR_NAME);
        let files_dir = dir.join(FILES_DIR);
        let _ = std::fs::create_dir_all(&files_dir);

        let mut recordings = HashMap::new();
        let mut total_bytes = 0u64;

        let index_path = dir.join(RECORDINGS_FILE);
        if let Ok(content) = std::fs::read_to_string(&index_path) {
            if let Ok(stored) = serde_json::from_str::<StoredRecordings>(&content) {
                for mut entry in stored.recordings {
                    let file = files_dir.join(format!("{}.ts", entry.id));
                    if let Ok(meta) = std::fs::metadata(&file) {
                        let actual_len = meta.len();
                        entry.bytes_written = actual_len;
                        total_bytes += actual_len;
                    }
                    recordings.insert(entry.id.clone(), entry);
                }
            }
        }

        Arc::new(DvrStore {
            dir,
            files_dir,
            recordings: Mutex::new(recordings),
            gate: tokio::sync::Mutex::new(()),
            wall,
            total_bytes: AtomicU64::new(total_bytes),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, RecordingEntry>> {
        self.recordings.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist_locked(&self, map: &HashMap<String, RecordingEntry>) -> Result<(), String> {
        let mut list: Vec<RecordingEntry> = map.values().cloned().collect();
        list.sort_by_key(|r| r.scheduled_start_ms);
        let stored = StoredRecordings { version: 1, recordings: list };
        let bytes = serde_json::to_vec_pretty(&stored).map_err(|e| format!("serialization error: {e}"))?;
        write_atomic(&self.dir, RECORDINGS_FILE, &bytes)
    }

    pub fn schedule(&self, req: ScheduleRequest, free_disk_mb: u64) -> Result<RecordingEntry, (StatusCode, String)> {
        let now = (self.wall)();
        if req.end_ms <= req.start_ms {
            return Err((StatusCode::BAD_REQUEST, "recording end time must be after start time".to_string()));
        }
        if req.end_ms <= now {
            return Err((StatusCode::BAD_REQUEST, "recording is already in the past".to_string()));
        }
        if free_disk_mb < crate::host::MIN_FREE_DISK_MB {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Companion free disk ({free_disk_mb} MB) is below 2048 MB floor"),
            ));
        }

        let mut lock = self.lock();

        // Check for conflicting overlap on the same channel
        for existing in lock.values() {
            if existing.channel_id == req.channel_id
                && matches!(existing.status, RecordingStatus::Scheduled | RecordingStatus::Recording)
            {
                let overlap = !(req.end_ms <= existing.scheduled_start_ms || req.start_ms >= existing.scheduled_end_ms);
                if overlap {
                    return Err((
                        StatusCode::CONFLICT,
                        format!("channel '{}' is already scheduled during that time window", req.channel_name),
                    ));
                }
            }
        }

        let id = generate_id();
        let file_name = format!("{id}.ts");
        let entry = RecordingEntry {
            id: id.clone(),
            channel_id: req.channel_id,
            channel_name: req.channel_name,
            title: req.title,
            description: req.description,
            stream_url: req.stream_url,
            scheduled_start_ms: req.start_ms,
            scheduled_end_ms: req.end_ms,
            actual_start_ms: None,
            actual_end_ms: None,
            status: RecordingStatus::Scheduled,
            file_name: Some(file_name),
            bytes_written: 0,
            scheduled_by_device: req.device_name,
            failure_reason: None,
            created_at_ms: now,
            playback_url: None,
            protected: false,
        };

        lock.insert(id, entry.clone());
        if let Err(e) = self.persist_locked(&lock) {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("failed to persist recording: {e}")));
        }

        Ok(entry)
    }

    pub fn list(&self) -> Vec<RecordingEntry> {
        let lock = self.lock();
        let mut list: Vec<RecordingEntry> = lock.values().cloned().collect();
        list.sort_by_key(|r| r.scheduled_start_ms);
        list
    }

    pub fn get(&self, id: &str) -> Option<RecordingEntry> {
        self.lock().get(id).cloned()
    }

    pub fn delete(&self, id: &str) -> Result<(), (StatusCode, String)> {
        let mut lock = self.lock();
        let entry = match lock.get(id) {
            Some(e) => e.clone(),
            None => return Err((StatusCode::NOT_FOUND, "recording not found".to_string())),
        };

        if entry.status == RecordingStatus::Recording {
            // If recording is active, transition to Cancelled
            let now = (self.wall)();
            if let Some(e) = lock.get_mut(id) {
                e.status = RecordingStatus::Cancelled;
                e.actual_end_ms = Some(now);
            }
            let _ = self.persist_locked(&lock);
            return Ok(());
        }

        // Remove from memory
        lock.remove(id);
        let _ = self.persist_locked(&lock);

        // Delete underlying file
        let path = self.files_dir.join(format!("{id}.ts"));
        if let Ok(meta) = std::fs::metadata(&path) {
            self.total_bytes.fetch_sub(meta.len(), Ordering::Relaxed);
            let _ = std::fs::remove_file(path);
        }

        Ok(())
    }

    pub fn set_protected(&self, id: &str, protected: bool) -> Result<bool, (StatusCode, String)> {
        let mut lock = self.lock();
        match lock.get_mut(id) {
            Some(entry) => {
                entry.protected = protected;
                if let Err(e) = self.persist_locked(&lock) {
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("failed to persist: {e}")));
                }
                Ok(protected)
            }
            None => Err((StatusCode::NOT_FOUND, "recording not found".to_string())),
        }
    }

    pub fn prune_oldest_unprotected(&self, target_free_mb: u64, current_free_mb: u64) -> usize {
        if current_free_mb >= target_free_mb {
            return 0;
        }
        let needed_bytes = (target_free_mb.saturating_sub(current_free_mb)) * 1024 * 1024;
        let mut freed_bytes = 0u64;
        let mut pruned_count = 0usize;

        let mut lock = self.lock();
        let mut candidates: Vec<(String, i64, u64)> = lock.values()
            .filter(|r| !r.protected && matches!(r.status, RecordingStatus::Finished | RecordingStatus::Partial | RecordingStatus::Failed | RecordingStatus::Cancelled))
            .map(|r| (r.id.clone(), r.scheduled_start_ms, r.bytes_written))
            .collect();

        candidates.sort_by_key(|(_, start, _)| *start);

        for (id, _, bytes) in candidates {
            if freed_bytes >= needed_bytes {
                break;
            }
            lock.remove(&id);
            let path = self.files_dir.join(format!("{id}.ts"));
            if let Ok(meta) = std::fs::metadata(&path) {
                let file_len = meta.len();
                self.total_bytes.fetch_sub(file_len, Ordering::Relaxed);
                freed_bytes += file_len;
                let _ = std::fs::remove_file(path);
            } else {
                freed_bytes += bytes;
            }
            pruned_count += 1;
        }

        if pruned_count > 0 {
            let _ = self.persist_locked(&lock);
        }
        pruned_count
    }

    pub fn update_status(&self, id: &str, status: RecordingStatus, bytes: Option<u64>, error: Option<String>) {
        let mut lock = self.lock();
        if let Some(entry) = lock.get_mut(id) {
            entry.status = status;
            let now = (self.wall)();
            if status == RecordingStatus::Recording && entry.actual_start_ms.is_none() {
                entry.actual_start_ms = Some(now);
            }
            if matches!(status, RecordingStatus::Finished | RecordingStatus::Partial | RecordingStatus::Failed | RecordingStatus::Cancelled) {
                entry.actual_end_ms = Some(now);
            }
            if let Some(b) = bytes {
                entry.bytes_written = b;
            }
            if let Some(err) = error {
                entry.failure_reason = Some(err);
            }
            let _ = self.persist_locked(&lock);
        }
    }

    pub fn update_bytes(&self, id: &str, bytes: u64) {
        let mut lock = self.lock();
        if let Some(entry) = lock.get_mut(id) {
            entry.bytes_written = bytes;
            let _ = self.persist_locked(&lock);
        }
    }

    pub fn finish_recording(&self, id: &str, status: RecordingStatus, bytes: u64, error: Option<String>) {
        let mut lock = self.lock();
        if let Some(entry) = lock.get_mut(id) {
            let old_bytes = entry.bytes_written;
            entry.status = status;
            entry.bytes_written = bytes;
            entry.actual_end_ms = Some((self.wall)());
            if let Some(err) = error {
                entry.failure_reason = Some(err);
            }
            if bytes > old_bytes {
                self.total_bytes.fetch_add(bytes - old_bytes, Ordering::Relaxed);
            }
            let _ = self.persist_locked(&lock);
        }
    }

    pub fn file_path(&self, id: &str) -> PathBuf {
        self.files_dir.join(format!("{id}.ts"))
    }

    #[cfg(test)]
    pub fn set_clock(&mut self, wall: WallClock) {
        self.wall = wall;
    }

    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    pub fn used_mb(&self) -> u64 {
        self.used_bytes().div_ceil(1024 * 1024)
    }

    pub async fn reset(&self) -> Result<(), String> {
        let _guard = self.gate.lock().await;
        {
            let mut lock = self.lock();
            lock.clear();
        }
        self.total_bytes.store(0, Ordering::Relaxed);
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
        let _ = tokio::fs::create_dir_all(&self.files_dir).await;
        Ok(())
    }

    pub fn generate_stream_token(&self, secret: &[u8; 32], id: &str, expires_at_ms: i64) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC can take 32-byte key");
        mac.update(format!("{id}:{expires_at_ms}").as_bytes());
        let result = mac.finalize().into_bytes();
        crate::auth::b64(&result)
    }

    pub fn verify_stream_token(
        &self,
        secret: &[u8; 32],
        id: &str,
        token: &str,
        expires_at_ms: i64,
        now_ms: i64,
    ) -> bool {
        if now_ms > expires_at_ms {
            return false;
        }
        let Some(given_bytes) = crate::auth::unb64(token) else {
            return false;
        };
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC can take 32-byte key");
        mac.update(format!("{id}:{expires_at_ms}").as_bytes());
        mac.verify_slice(&given_bytes).is_ok()
    }
}

// ---- Background stream capture and scheduler loop ---------------------------------------------

pub async fn run_dvr_scheduler(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(15));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        if state.is_paused() {
            continue;
        }

        let now = state.wall_ms();
        let free_disk_mb = state.facts().disk_free_mb;

        // Auto-pruning: if free disk drops below 5120 MB (5 GB), prune oldest unprotected recordings
        if free_disk_mb < AUTO_PRUNE_WATERMARK_MB {
            let pruned = state.dvr.prune_oldest_unprotected(AUTO_PRUNE_WATERMARK_MB, free_disk_mb);
            if pruned > 0 {
                eprintln!("dvr: auto-pruned {pruned} old unprotected recordings to restore free space");
            }
        }

        let recordings = state.dvr.list();

        for entry in recordings {
            if entry.status == RecordingStatus::Scheduled {
                // If past scheduled end, mark missed/failed
                if entry.scheduled_end_ms <= now {
                    state.dvr.update_status(
                        &entry.id,
                        RecordingStatus::Failed,
                        Some(0),
                        Some("missed: Companion was offline or unreachable during scheduled window".to_string()),
                    );
                    continue;
                }

                // If due to start (within 30s before start or currently in progress)
                if entry.scheduled_start_ms <= now + 30_000 {
                    if free_disk_mb < crate::host::MIN_FREE_DISK_MB {
                        state.dvr.update_status(
                            &entry.id,
                            RecordingStatus::Failed,
                            Some(0),
                            Some(format!("cancelled: free disk ({free_disk_mb} MB) below 2048 MB floor")),
                        );
                        continue;
                    }

                    let s = state.clone();
                    let e = entry.clone();
                    tokio::spawn(async move {
                        capture_stream(s, e).await;
                    });
                }
            }
        }
    }
}

/// How long a connect may take, and how long a live stream may go silent, before the capture
/// treats it as dropped and reconnects.
const CAPTURE_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CAPTURE_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// Waits between reconnects (the last repeats) so a dead provider is not hammered.
const CAPTURE_RETRY_WAITS_S: &[u64] = &[2, 5, 10, 20, 30];
/// More time than this missing in drops marks the recording Partial rather than Finished.
const CAPTURE_GAP_PARTIAL_MS: i64 = 60_000;

/// Records `entry.stream_url` until the scheduled end.
///
/// A live stream never ends, so the client has no total timeout: the old 15-second total
/// timeout cut every recording 15 s in (25 Nov 2026: two Channel 4 recordings, 7 MB each, both
/// "Partial: stream disconnected"). Instead a silent stream counts as dropped, and a drop is
/// reconnected and appended to the same file until the end, as an MPEG-TS stream allows.
async fn capture_stream(state: Arc<AppState>, entry: RecordingEntry) {
    state.dvr.update_status(&entry.id, RecordingStatus::Recording, None, None);

    let file_path = state.dvr.file_path(&entry.id);
    let file = match tokio::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&file_path).await {
        Ok(f) => f,
        Err(e) => {
            state.dvr.finish_recording(&entry.id, RecordingStatus::Failed, 0, Some(format!("failed to open file: {e}")));
            return;
        }
    };

    let client = match reqwest::Client::builder()
        .connect_timeout(CAPTURE_CONNECT_TIMEOUT)
        .read_timeout(CAPTURE_IDLE_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            state.dvr.finish_recording(&entry.id, RecordingStatus::Failed, 0, Some(format!("HTTP client error: {e}")));
            return;
        }
    };

    let mut writer = tokio::io::BufWriter::new(file);
    let mut written = 0u64;
    let mut last_progress = tokio::time::Instant::now();
    let mut fatal: Option<String> = None;
    let mut drops = 0u32;
    let mut gap_ms = 0i64;
    let mut last_error: Option<String> = None;
    let mut dropped_at: Option<i64> = None;

    'connect: loop {
        if state.wall_ms() >= entry.scheduled_end_ms {
            break;
        }
        if state.dvr.get(&entry.id).is_some_and(|curr| curr.status == RecordingStatus::Cancelled) {
            fatal = Some("cancelled".into());
            break;
        }
        let attempt_error = match client.get(&entry.stream_url).send().await {
            Ok(r) if r.status().is_success() => {
                if let Some(at) = dropped_at.take() {
                    gap_ms += state.wall_ms() - at;
                }
                let mut stream = r.bytes_stream();
                let mut error = None;
                while let Some(chunk_res) = stream.next().await {
                    if state.wall_ms() >= entry.scheduled_end_ms {
                        break 'connect;
                    }
                    if state.dvr.get(&entry.id).is_some_and(|curr| curr.status == RecordingStatus::Cancelled) {
                        fatal = Some("cancelled".into());
                        break 'connect;
                    }
                    if written % (8 * 1024 * 1024) < 65536 {
                        if let Ok(free) = crate::host::free_disk_bytes(&state.config.data_dir) {
                            if free < crate::store::FREE_DISK_FLOOR_BYTES {
                                fatal = Some("disk floor reached during capture".into());
                                break 'connect;
                            }
                        }
                    }
                    match chunk_res {
                        Ok(chunk) => {
                            if let Err(e) = writer.write_all(&chunk).await {
                                eprintln!("dvr write error {}: {e}", entry.id);
                                fatal = Some(format!("disk write error: {e}"));
                                break 'connect;
                            }
                            written += chunk.len() as u64;
                            if last_progress.elapsed().as_secs() >= 5 {
                                let _ = writer.flush().await;
                                state.dvr.update_bytes(&entry.id, written);
                                last_progress = tokio::time::Instant::now();
                            }
                        }
                        Err(e) => {
                            error = Some(format!("stream dropped: {e}"));
                            break;
                        }
                    }
                }
                error.unwrap_or_else(|| "the provider ended the stream".into())
            }
            // A refusal of the first connection is final (wrong URL, no subscription); later
            // it is usually the provider's connection limit, so keep trying until the end.
            Ok(r) if written == 0 && drops == 0 => {
                fatal = Some(format!("stream HTTP {}", r.status()));
                break;
            }
            Ok(r) => format!("stream HTTP {}", r.status()),
            Err(e) if written == 0 && drops == 0 => {
                fatal = Some(format!("stream connection failed: {e}"));
                break;
            }
            Err(e) => format!("stream connection failed: {e}"),
        };
        drops += 1;
        eprintln!("dvr {}: {attempt_error}; reconnecting (drop {drops})", entry.id);
        last_error = Some(attempt_error);
        dropped_at.get_or_insert(state.wall_ms());
        let _ = writer.flush().await;
        state.dvr.update_bytes(&entry.id, written);
        let wait = CAPTURE_RETRY_WAITS_S[(drops as usize - 1).min(CAPTURE_RETRY_WAITS_S.len() - 1)];
        let remaining = entry.scheduled_end_ms - state.wall_ms();
        if remaining <= 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis((wait * 1000).min(remaining as u64))).await;
    }
    if let Some(at) = dropped_at {
        gap_ms += state.wall_ms().min(entry.scheduled_end_ms) - at;
    }

    let _ = writer.flush().await;

    let (final_status, reason) = capture_outcome(fatal, written, drops, gap_ms, last_error);
    state.dvr.finish_recording(&entry.id, final_status, written, reason);
}

/// The status and printable reason a capture ends with.
fn capture_outcome(
    fatal: Option<String>,
    written: u64,
    drops: u32,
    gap_ms: i64,
    last_error: Option<String>,
) -> (RecordingStatus, Option<String>) {
    let drop_note = (drops > 0).then(|| format!(
        "{drops} stream drop{}, {} s missing; last: {}",
        if drops == 1 { "" } else { "s" },
        gap_ms.max(0) / 1000,
        last_error.as_deref().unwrap_or("unknown"),
    ));
    match fatal {
        Some(reason) if reason == "cancelled" => (RecordingStatus::Cancelled, Some(reason)),
        Some(reason) => {
            let status = if written > 0 { RecordingStatus::Partial } else { RecordingStatus::Failed };
            let reason = match drop_note { Some(note) => format!("{reason} ({note})"), None => reason };
            (status, Some(reason))
        }
        None if written == 0 => (RecordingStatus::Failed, Some(drop_note.unwrap_or_else(|| "no data received".into()))),
        None if gap_ms > CAPTURE_GAP_PARTIAL_MS => (RecordingStatus::Partial, drop_note),
        None => (RecordingStatus::Finished, drop_note),
    }
}

// ---- HTTP Handlers ----------------------------------------------------------------------------

fn json_resp(status: StatusCode, body: serde_json::Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

pub async fn schedule_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
    body: axum::body::Bytes,
) -> Response {
    let req: ScheduleRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return session.sealed_reason(StatusCode::BAD_REQUEST, &format!("invalid request JSON: {e}")),
    };

    let free_disk_mb = state.facts().disk_free_mb;
    match state.dvr.schedule(req, free_disk_mb) {
        Ok(entry) => session.sealed_json(StatusCode::CREATED, &serde_json::to_value(&entry).unwrap_or_default()),
        Err((status, reason)) => session.sealed_reason(status, &reason),
    }
}

pub async fn list_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
) -> Response {
    let mut list = state.dvr.list();
    let now = state.wall_ms();
    let expires = now + STREAM_TOKEN_VALIDITY_MS;
    let secret = session.secret;

    for entry in &mut list {
        if matches!(entry.status, RecordingStatus::Recording | RecordingStatus::Finished | RecordingStatus::Partial) {
            let token = state.dvr.generate_stream_token(&secret, &entry.id, expires);
            let encoded_token = urlencoding_simple(&token);
            entry.playback_url = Some(format!("/v1/dvr/stream/{}?token={}&expires={}", entry.id, encoded_token, expires));
        }
    }

    session.sealed_json(StatusCode::OK, &serde_json::to_value(&list).unwrap_or_default())
}

pub async fn delete_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    match state.dvr.delete(&id) {
        Ok(()) => session.sealed_json(StatusCode::OK, &serde_json::json!({ "deleted": true })),
        Err((status, reason)) => session.sealed_reason(status, &reason),
    }
}

#[derive(Deserialize)]
pub struct ProtectRequest {
    pub protected: bool,
}

pub async fn protect_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
    UrlPath(id): UrlPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let req: ProtectRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return session.sealed_reason(StatusCode::BAD_REQUEST, &format!("invalid request JSON: {e}")),
    };

    match state.dvr.set_protected(&id, req.protected) {
        Ok(p) => session.sealed_json(StatusCode::OK, &serde_json::json!({ "protected": p })),
        Err((status, reason)) => session.sealed_reason(status, &reason),
    }
}

fn parse_query(query_str: Option<&str>) -> (Option<String>, Option<i64>) {
    let mut token = None;
    let mut expires = None;
    if let Some(q) = query_str {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == "token" {
                    let dec = v.replace("%2B", "+").replace("%2F", "/").replace("%3D", "=");
                    token = Some(dec);
                } else if k == "expires" {
                    expires = v.parse::<i64>().ok();
                }
            }
        }
    }
    (token, expires)
}

fn urlencoding_simple(s: &str) -> String {
    s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
}

pub async fn stream_route(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    req: Request,
) -> Response {
    let (token_opt, expires_opt) = parse_query(req.uri().query());
    // Validate stream token or household auth
    let authorized = match (token_opt, expires_opt) {
        (Some(token), Some(expires)) => {
            if let Some(secret) = state.auth.household_secret() {
                state.dvr.verify_stream_token(&secret, &id, &token, expires, state.wall_ms())
            } else {
                false
            }
        }
        _ => false,
    };

    if !authorized {
        return json_resp(StatusCode::UNAUTHORIZED, serde_json::json!({ "reason": "invalid or expired stream token" }));
    }

    let file_path = state.dvr.file_path(&id);
    let mut file = match tokio::fs::File::open(&file_path).await {
        Ok(f) => f,
        Err(_) => return json_resp(StatusCode::NOT_FOUND, serde_json::json!({ "reason": "recording stream not found" })),
    };

    let total_len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    if total_len == 0 {
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "video/mp2t"),
                (header::CONTENT_LENGTH, "0"),
                (header::ACCEPT_RANGES, "bytes"),
            ],
            Body::empty(),
        )
            .into_response();
    }

    // Check Range header
    let range_header = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_string);
    if let Some(range_str) = range_header.as_deref() {
        if let Some(parsed) = parse_range_spec(range_str, total_len) {
            let (start, end) = parsed;
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
            )
                .into_response();
        } else {
            // Invalid range
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{total_len}"))],
                Body::empty(),
            )
                .into_response();
        }
    }

    // Full body stream
    let stream = ReaderStream::new(file);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CONTENT_LENGTH, total_len.to_string().as_str()),
            (header::ACCEPT_RANGES, "bytes"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

fn parse_range_spec(spec: &str, total: u64) -> Option<(u64, u64)> {
    let s = spec.strip_prefix("bytes=")?;
    let mut parts = s.split('-');
    let start_str = parts.next()?.trim();
    let end_str = parts.next()?.trim();

    if start_str.is_empty() {
        // Suffix range: bytes=-500 (last 500 bytes)
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
            let explicit_end: u64 = end_str.parse().ok()?;
            explicit_end.min(total - 1)
        };
        if start > end {
            return None;
        }
        Some((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir() -> TestDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("aurelis-dvr-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::create_dir_all(&p);
        TestDir(p)
    }

    fn test_store(dir: &Path) -> Arc<DvrStore> {
        let wall: WallClock = Arc::new(|| 1_000_000);
        DvrStore::open(dir, wall)
    }

    #[test]
    fn schedule_validates_times_and_overlap() {
        let tmp = temp_dir();
        let store = test_store(&tmp.0);

        // End before start
        let req_invalid = ScheduleRequest {
            channel_id: "bbc1".into(),
            channel_name: "BBC One".into(),
            title: "News".into(),
            description: None,
            stream_url: "http://iptv/bbc1.ts".into(),
            start_ms: 2_000_000,
            end_ms: 1_500_000,
            device_name: "Living Room TV".into(),
        };
        let err = store.schedule(req_invalid, 10_000).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        // Schedule valid
        let req1 = ScheduleRequest {
            channel_id: "bbc1".into(),
            channel_name: "BBC One".into(),
            title: "News 1".into(),
            description: None,
            stream_url: "http://iptv/bbc1.ts".into(),
            start_ms: 1_100_000,
            end_ms: 1_200_000,
            device_name: "Living Room TV".into(),
        };
        let entry1 = store.schedule(req1, 10_000).unwrap();
        assert_eq!(entry1.status, RecordingStatus::Scheduled);
        assert_eq!(entry1.channel_id, "bbc1");

        // Schedule conflicting overlap on same channel
        let req_overlap = ScheduleRequest {
            channel_id: "bbc1".into(),
            channel_name: "BBC One".into(),
            title: "News Conflict".into(),
            description: None,
            stream_url: "http://iptv/bbc1.ts".into(),
            start_ms: 1_150_000,
            end_ms: 1_250_000,
            device_name: "Bedroom TV".into(),
        };
        let err_overlap = store.schedule(req_overlap, 10_000).unwrap_err();
        assert_eq!(err_overlap.0, StatusCode::CONFLICT);

        // Schedule non-overlapping on same channel succeeds
        let req2 = ScheduleRequest {
            channel_id: "bbc1".into(),
            channel_name: "BBC One".into(),
            title: "News 2".into(),
            description: None,
            stream_url: "http://iptv/bbc1.ts".into(),
            start_ms: 1_200_000,
            end_ms: 1_300_000,
            device_name: "Living Room TV".into(),
        };
        let entry2 = store.schedule(req2, 10_000).unwrap();
        assert_eq!(entry2.channel_id, "bbc1");

        // Overlap on different channel succeeds
        let req_diff = ScheduleRequest {
            channel_id: "itv1".into(),
            channel_name: "ITV 1".into(),
            title: "Movie".into(),
            description: None,
            stream_url: "http://iptv/itv1.ts".into(),
            start_ms: 1_150_000,
            end_ms: 1_250_000,
            device_name: "Bedroom TV".into(),
        };
        assert!(store.schedule(req_diff, 10_000).is_ok());
    }

    #[test]
    fn schedule_enforces_disk_floor() {
        let tmp = temp_dir();
        let store = test_store(&tmp.0);

        let req = ScheduleRequest {
            channel_id: "bbc1".into(),
            channel_name: "BBC One".into(),
            title: "News".into(),
            description: None,
            stream_url: "http://iptv/bbc1.ts".into(),
            start_ms: 1_100_000,
            end_ms: 1_200_000,
            device_name: "Living Room TV".into(),
        };
        // 1000 MB free < 2048 MB floor
        let err = store.schedule(req, 1000).unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.1.contains("2048 MB"));
    }

    #[test]
    fn persistence_and_reload() {
        let tmp = temp_dir();
        let wall: WallClock = Arc::new(|| 1_000_000);
        let store = DvrStore::open(&tmp.0, wall.clone());

        let req = ScheduleRequest {
            channel_id: "bbc1".into(),
            channel_name: "BBC One".into(),
            title: "Tonight".into(),
            description: Some("Evening news".into()),
            stream_url: "http://iptv/bbc1.ts".into(),
            start_ms: 1_500_000,
            end_ms: 1_600_000,
            device_name: "Living Room TV".into(),
        };
        let entry = store.schedule(req, 10_000).unwrap();

        // Re-open from disk
        let reopened = DvrStore::open(&tmp.0, wall);
        let list = reopened.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, entry.id);
        assert_eq!(list[0].title, "Tonight");
        assert_eq!(list[0].description.as_deref(), Some("Evening news"));
    }

    #[test]
    fn stream_token_generation_and_verification() {
        let tmp = temp_dir();
        let store = test_store(&tmp.0);
        let secret = [7u8; 32];
        let id = "test-rec-123";
        let expires = 2_000_000;

        let token = store.generate_stream_token(&secret, id, expires);
        assert!(store.verify_stream_token(&secret, id, &token, expires, 1_500_000));

        // Expired token rejected
        assert!(!store.verify_stream_token(&secret, id, &token, expires, 2_000_001));

        // Wrong secret rejected
        let wrong_secret = [9u8; 32];
        assert!(!store.verify_stream_token(&wrong_secret, id, &token, expires, 1_500_000));

        // Tampered ID rejected
        assert!(!store.verify_stream_token(&secret, "other-id", &token, expires, 1_500_000));
    }

    #[test]
    fn range_spec_parser() {
        assert_eq!(parse_range_spec("bytes=0-499", 1000), Some((0, 499)));
        assert_eq!(parse_range_spec("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range_spec("bytes=-200", 1000), Some((800, 999)));
        assert_eq!(parse_range_spec("bytes=0-2000", 1000), Some((0, 999)));
        assert_eq!(parse_range_spec("bytes=1500-", 1000), None);
        assert_eq!(parse_range_spec("bytes=800-400", 1000), None);
    }

    #[test]
    fn protect_and_auto_prune() {
        let tmp = temp_dir();
        let store = test_store(&tmp.0);

        let mut entries = Vec::new();
        for i in 1..=3 {
            let req = ScheduleRequest {
                channel_id: format!("ch{i}"),
                channel_name: format!("Channel {i}"),
                title: format!("Show {i}"),
                description: None,
                stream_url: "http://stream".into(),
                start_ms: 1_000_000 + i * 100_000,
                end_ms: 1_000_000 + i * 100_000 + 50_000,
                device_name: "TV".into(),
            };
            let e = store.schedule(req, 10_000).unwrap();
            entries.push(e);
        }

        for e in &entries {
            store.finish_recording(&e.id, RecordingStatus::Finished, 100 * 1024 * 1024, None);
        }

        assert!(store.set_protected(&entries[0].id, true).unwrap());
        assert!(store.get(&entries[0].id).unwrap().protected);

        let pruned = store.prune_oldest_unprotected(5120, 2000);
        assert_eq!(pruned, 2);

        let surviving = store.get(&entries[0].id);
        assert!(surviving.is_some());
        assert!(surviving.unwrap().protected);

        assert!(store.get(&entries[1].id).is_none());
        assert!(store.get(&entries[2].id).is_none());
    }

    #[test]
    fn capture_outcome_names_drops_and_only_long_gaps_are_partial() {
        let (status, reason) = capture_outcome(None, 5_000_000, 0, 0, None);
        assert_eq!(status, RecordingStatus::Finished);
        assert_eq!(reason, None);
        let (status, reason) = capture_outcome(None, 5_000_000, 1, 4_000, Some("stream dropped: reset".into()));
        assert_eq!(status, RecordingStatus::Finished);
        assert_eq!(reason.as_deref(), Some("1 stream drop, 4 s missing; last: stream dropped: reset"));
        let (status, _) = capture_outcome(None, 5_000_000, 3, 90_000, Some("stream HTTP 509".into()));
        assert_eq!(status, RecordingStatus::Partial);
        let (status, reason) = capture_outcome(Some("stream HTTP 404 Not Found".into()), 0, 0, 0, None);
        assert_eq!(status, RecordingStatus::Failed);
        assert_eq!(reason.as_deref(), Some("stream HTTP 404 Not Found"));
        let (status, _) = capture_outcome(Some("cancelled".into()), 10, 0, 0, None);
        assert_eq!(status, RecordingStatus::Cancelled);
        let (status, reason) = capture_outcome(None, 0, 2, 30_000, Some("stream connection failed: refused".into()));
        assert_eq!(status, RecordingStatus::Failed);
        assert!(reason.unwrap().contains("2 stream drops"));
    }

    /// The 25 Nov 2026 failure: a live stream was cut 15 s in by a total request timeout.
    /// Streams for 18 s, drops once, then streams again; run with `--ignored` (it takes ~24 s).
    #[tokio::test]
    #[ignore]
    async fn capture_records_past_fifteen_seconds_and_reconnects_after_a_drop() {
        use crate::state::{test_config, test_state};
        let served = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = served.clone();
        let app = axum::Router::new().route("/live.ts", axum::routing::get(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                // The first connection streams for 17 s, then the "provider" drops it.
                let started = tokio::time::Instant::now();
                let body = futures_util::stream::unfold(0u32, move |i| async move {
                    if n == 0 && started.elapsed() >= Duration::from_secs(17) { return None; }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![0x47u8; 188 * 10])), i + 1))
                });
                axum::body::Body::from_stream(body)
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = temp_dir();
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let state = Arc::new(test_state(config));
        let now = state.wall_ms();
        let entry = state.dvr.schedule(ScheduleRequest {
            channel_id: "c4".into(), channel_name: "Channel 4".into(), title: "Test".into(), description: None,
            stream_url: format!("http://{addr}/live.ts"), start_ms: now, end_ms: now + 24_000, device_name: "test".into(),
        }, 100_000).unwrap();
        capture_stream(state.clone(), entry.clone()).await;
        let done = state.dvr.get(&entry.id).unwrap();
        assert_eq!(done.status, RecordingStatus::Finished, "{:?}", done.failure_reason);
        assert!(served.load(Ordering::SeqCst) >= 2, "never reconnected");
        // ~18 s before the drop plus the time after the reconnect: well past the old 15 s cut.
        let ran_ms = done.actual_end_ms.unwrap() - done.actual_start_ms.unwrap();
        assert!(ran_ms >= 23_000 && done.bytes_written > 0, "ran {ran_ms} ms, {} bytes", done.bytes_written);
        assert!(done.failure_reason.as_deref().is_some_and(|r| r.starts_with("1 stream drop")), "{:?}", done.failure_reason);
    }
}

