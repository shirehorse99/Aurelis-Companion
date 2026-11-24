//! Multi-Room Stream Relay & Concurrent Fan-Out: allows multiple televisions and DVR captures
//! in the household to share a single upstream IPTV provider connection over LAN.

use crate::state::AppState;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch};

pub const PRE_ROLL_BYTES_MAX: usize = 512 * 1024; // 512 KB pre-roll buffer for instant player lock
pub const BROADCAST_CAPACITY: usize = 256; // Ring buffer of stream chunks
pub const IDLE_GRACE_PERIOD_SECS: u64 = 12; // Keep upstream stream alive for 12s after last client disconnects
pub const STREAM_TOKEN_VALIDITY_MS: i64 = 24 * 3600 * 1000; // 24 hours

type HmacSha256 = Hmac<Sha256>;

pub struct ActiveRelay {
    pub key: String,
    pub upstream_url: String,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub broadcast_tx: broadcast::Sender<Bytes>,
    pub active_subscribers: Arc<AtomicUsize>,
    pub total_bytes_relayed: Arc<AtomicU64>,
    pub upstream_bytes_received: Arc<AtomicU64>,
    pub started_at_ms: i64,
    pub last_chunk_at_ms: Arc<AtomicI64>,
    pub recent_chunks: Arc<RwLock<VecDeque<Bytes>>>,
    pub recent_bytes: Arc<AtomicUsize>,
    pub stop_tx: watch::Sender<bool>,
    pub has_paused_subscriber: Arc<std::sync::atomic::AtomicBool>,
    pub timeshift: Option<Arc<crate::timeshift::TimeshiftBuffer>>,
}

pub struct SubscriberGuard {
    relay: Arc<ActiveRelay>,
}

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        self.relay.active_subscribers.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct StreamRelayManager {
    relays: Arc<RwLock<HashMap<String, Arc<ActiveRelay>>>>,
    wall: crate::state::WallClock,
    pub client: reqwest::Client,
    timeshift_dir: Option<std::path::PathBuf>,
    timeshift_minutes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelaySummary {
    pub key: String,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub sanitized_url: String,
    pub active_subscribers: usize,
    pub upstream_bytes_received: u64,
    pub total_bytes_relayed: u64,
    pub upstream_streams_saved: usize,
    pub uptime_seconds: u64,
    pub timeshift_buffer_seconds: u64,
}

pub fn url_hash(url: &str) -> String {
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    format!("{:x}", h.finalize())
}

pub fn sanitize_stream_url(url: &str) -> String {
    if let Ok(mut parsed) = reqwest::Url::parse(url) {
        let _ = parsed.set_password(None);
        let _ = parsed.set_username("");

        // Redact Xtream path: /live/<user>/<pass>/<id> or /movie/... or /series/...
        let path = parsed.path().to_string();
        let segments: Vec<&str> = path.split('/').collect();
        if segments.len() >= 5 && (segments[1] == "live" || segments[1] == "movie" || segments[1] == "series") {
            let mut new_segments = segments;
            new_segments[2] = "***";
            new_segments[3] = "***";
            let new_path = new_segments.join("/");
            parsed.set_path(&new_path);
        }

        let query_pairs: Vec<(String, String)> = parsed.query_pairs().map(|(k, v)| {
            if k.eq_ignore_ascii_case("password") || k.eq_ignore_ascii_case("pass") || k.eq_ignore_ascii_case("token") {
                (k.into_owned(), "***".to_string())
            } else {
                (k.into_owned(), v.into_owned())
            }
        }).collect();
        parsed.query_pairs_mut().clear().extend_pairs(query_pairs);
        parsed.to_string()
    } else {
        url.split('?').next().unwrap_or(url).to_string()
    }
}

impl StreamRelayManager {
    pub fn new(
        wall: crate::state::WallClock,
        data_dir: Option<std::path::PathBuf>,
        timeshift_enabled: bool,
        timeshift_minutes: u64,
    ) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let timeshift_dir = if timeshift_enabled { data_dir } else { None };

        Arc::new(Self {
            relays: Arc::new(RwLock::new(HashMap::new())),
            wall,
            client,
            timeshift_dir,
            timeshift_minutes,
        })
    }

    pub fn generate_stream_token(&self, secret: &[u8; 32], key: &str, expires_at_ms: i64) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC can take 32-byte key");
        mac.update(format!("{key}:{expires_at_ms}").as_bytes());
        let result = mac.finalize().into_bytes();
        crate::auth::b64(&result)
    }

    pub fn verify_stream_token(
        &self,
        secret: &[u8; 32],
        key: &str,
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
        mac.update(format!("{key}:{expires_at_ms}").as_bytes());
        mac.verify_slice(&given_bytes).is_ok()
    }

    pub async fn subscribe(
        self: &Arc<Self>,
        upstream_url: &str,
        channel_id: Option<String>,
        channel_name: Option<String>,
    ) -> Result<(Vec<Bytes>, broadcast::Receiver<Bytes>, SubscriberGuard), String> {
        let key = url_hash(upstream_url);

        // Check if relay already exists
        let relay = {
            let read = self.relays.read().unwrap_or_else(|e| e.into_inner());
            read.get(&key).cloned()
        };

        let relay = match relay {
            Some(r) => r,
            None => {
                let mut write = self.relays.write().unwrap_or_else(|e| e.into_inner());
                if let Some(r) = write.get(&key) {
                    r.clone()
                } else {
                    let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
                    let (stop_tx, stop_rx) = watch::channel(false);
                    let now = (self.wall)();
                    let timeshift = if let Some(dir) = &self.timeshift_dir {
                        if self.timeshift_minutes > 0 {
                            crate::timeshift::TimeshiftBuffer::new(&key, dir, self.timeshift_minutes, self.wall.clone()).ok()
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let new_relay = Arc::new(ActiveRelay {
                        key: key.clone(),
                        upstream_url: upstream_url.to_string(),
                        channel_id,
                        channel_name,
                        broadcast_tx,
                        active_subscribers: Arc::new(AtomicUsize::new(0)),
                        total_bytes_relayed: Arc::new(AtomicU64::new(0)),
                        upstream_bytes_received: Arc::new(AtomicU64::new(0)),
                        started_at_ms: now,
                        last_chunk_at_ms: Arc::new(AtomicI64::new(now)),
                        recent_chunks: Arc::new(RwLock::new(VecDeque::new())),
                        recent_bytes: Arc::new(AtomicUsize::new(0)),
                        stop_tx,
                        has_paused_subscriber: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        timeshift,
                    });
                    write.insert(key.clone(), new_relay.clone());

                    // Spawn upstream ingestion worker
                    let mgr = self.clone();
                    let r = new_relay.clone();
                    tokio::spawn(async move {
                        run_upstream_ingest(mgr, r, stop_rx).await;
                    });

                    new_relay
                }
            }
        };

        // Increment subscribers
        relay.active_subscribers.fetch_add(1, Ordering::SeqCst);

        // Pre-roll buffer snapshot
        let pre_roll: Vec<Bytes> = {
            let recent = relay.recent_chunks.read().unwrap_or_else(|e| e.into_inner());
            recent.iter().cloned().collect()
        };

        let rx = relay.broadcast_tx.subscribe();
        let guard = SubscriberGuard { relay: relay.clone() };

        Ok((pre_roll, rx, guard))
    }

    pub fn remove_relay(&self, key: &str) {
        let mut write = self.relays.write().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = write.remove(key) {
            if let Some(ts) = &r.timeshift {
                ts.close();
            }
        }
    }

    pub fn get_relay(&self, key: &str) -> Option<Arc<ActiveRelay>> {
        let read = self.relays.read().unwrap_or_else(|e| e.into_inner());
        read.get(key).cloned()
    }

    pub fn set_paused_subscriber(&self, key: &str, paused: bool) {
        let read = self.relays.read().unwrap_or_else(|e| e.into_inner());
        if let Some(relay) = read.get(key) {
            relay.has_paused_subscriber.store(paused, Ordering::SeqCst);
        }
    }

    pub fn timeshift_info(&self, key: &str) -> Option<crate::timeshift::TimeshiftInfo> {
        let read = self.relays.read().unwrap_or_else(|e| e.into_inner());
        read.get(key).and_then(|r| r.timeshift.as_ref().map(|ts| ts.info()))
    }

    pub fn active_summaries(&self) -> Vec<RelaySummary> {
        let now = (self.wall)();
        let read = self.relays.read().unwrap_or_else(|e| e.into_inner());
        read.values().map(|r| {
            let subs = r.active_subscribers.load(Ordering::Relaxed);
            let upstream_bytes = r.upstream_bytes_received.load(Ordering::Relaxed);
            let relayed_bytes = r.total_bytes_relayed.load(Ordering::Relaxed);
            let uptime = (now.saturating_sub(r.started_at_ms) / 1000).max(0) as u64;
            let timeshift_seconds = r.timeshift.as_ref().map(|ts| ts.buffer_seconds()).unwrap_or(0);
            RelaySummary {
                key: r.key.clone(),
                channel_id: r.channel_id.clone(),
                channel_name: r.channel_name.clone(),
                sanitized_url: sanitize_stream_url(&r.upstream_url),
                active_subscribers: subs,
                upstream_bytes_received: upstream_bytes,
                total_bytes_relayed: relayed_bytes,
                upstream_streams_saved: subs.saturating_sub(1),
                uptime_seconds: uptime,
                timeshift_buffer_seconds: timeshift_seconds,
            }
        }).collect()
    }

    pub fn total_streams_saved(&self) -> usize {
        let read = self.relays.read().unwrap_or_else(|e| e.into_inner());
        read.values().map(|r| {
            let subs = r.active_subscribers.load(Ordering::Relaxed);
            subs.saturating_sub(1)
        }).sum()
    }
}

async fn run_upstream_ingest(
    manager: Arc<StreamRelayManager>,
    relay: Arc<ActiveRelay>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut backoff_ms = 500u64;

    'outer: loop {
        let req = manager.client.get(&relay.upstream_url)
            .header("User-Agent", "VLC/3.0.18 LibVLC/3.0.18")
            .header("Accept", "*/*");

        let resp = match req.send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                eprintln!("stream relay {}: upstream HTTP {}", relay.key, r.status());
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {},
                    _ = stop_rx.changed() => break 'outer,
                }
                backoff_ms = (backoff_ms * 2).min(5000);
                if relay.active_subscribers.load(Ordering::Relaxed) == 0 && !relay.has_paused_subscriber.load(Ordering::Relaxed) {
                    break 'outer;
                }
                continue 'outer;
            }
            Err(e) => {
                eprintln!("stream relay {}: upstream error: {e}", relay.key);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {},
                    _ = stop_rx.changed() => break 'outer,
                }
                backoff_ms = (backoff_ms * 2).min(5000);
                if relay.active_subscribers.load(Ordering::Relaxed) == 0 && !relay.has_paused_subscriber.load(Ordering::Relaxed) {
                    break 'outer;
                }
                continue 'outer;
            }
        };

        backoff_ms = 500;
        let mut stream = resp.bytes_stream();
        let mut idle_since: Option<Instant> = None;

        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    break 'outer;
                }
                chunk_opt = stream.next() => {
                    match chunk_opt {
                        Some(Ok(chunk)) => {
                            let len = chunk.len();
                            relay.upstream_bytes_received.fetch_add(len as u64, Ordering::Relaxed);
                            relay.last_chunk_at_ms.store((manager.wall)(), Ordering::Relaxed);

                            // Append to rolling timeshift buffer
                            if let Some(ts) = &relay.timeshift {
                                let _ = ts.write_chunk(&chunk);
                            }

                            // Append to pre-roll buffer
                            {
                                let mut recent = relay.recent_chunks.write().unwrap_or_else(|e| e.into_inner());
                                recent.push_back(chunk.clone());
                                let mut total = relay.recent_bytes.fetch_add(len, Ordering::Relaxed) + len;
                                while total > PRE_ROLL_BYTES_MAX && recent.len() > 1 {
                                    if let Some(front) = recent.pop_front() {
                                        let front_len = front.len();
                                        relay.recent_bytes.fetch_sub(front_len, Ordering::Relaxed);
                                        total = total.saturating_sub(front_len);
                                    }
                                }
                            }

                            let sub_count = relay.active_subscribers.load(Ordering::Relaxed);
                            let is_paused = relay.has_paused_subscriber.load(Ordering::Relaxed);
                            if sub_count > 0 || is_paused {
                                idle_since = None;
                                if sub_count > 0 {
                                    relay.total_bytes_relayed.fetch_add((len * sub_count) as u64, Ordering::Relaxed);
                                    let _ = relay.broadcast_tx.send(chunk);
                                }
                            } else {
                                let now = Instant::now();
                                match idle_since {
                                    None => idle_since = Some(now),
                                    Some(since) => {
                                        if now.duration_since(since).as_secs() >= IDLE_GRACE_PERIOD_SECS {
                                            break 'outer;
                                        }
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            eprintln!("stream relay {}: read error: {e}", relay.key);
                            break;
                        }
                        None => {
                            eprintln!("stream relay {}: upstream stream EOF", relay.key);
                            break;
                        }
                    }
                }
            }
        }

        if relay.active_subscribers.load(Ordering::Relaxed) == 0 && !relay.has_paused_subscriber.load(Ordering::Relaxed) {
            break 'outer;
        }
    }

    manager.remove_relay(&relay.key);
}

fn parse_relay_query(query_str: Option<&str>) -> (Option<String>, Option<String>, Option<String>, Option<String>, Option<i64>) {
    let mut url = None;
    let mut channel_id = None;
    let mut channel_name = None;
    let mut token = None;
    let mut expires = None;

    if let Some(q) = query_str {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                match k {
                    "url" => {
                        let decoded = v.replace("%3A", ":").replace("%2F", "/").replace("%3F", "?").replace("%3D", "=").replace("%26", "&");
                        url = Some(decoded);
                    }
                    "channel_id" => channel_id = Some(v.to_string()),
                    "channel_name" => channel_name = Some(v.to_string()),
                    "token" => {
                        let dec = v.replace("%2B", "+").replace("%2F", "/").replace("%3D", "=");
                        token = Some(dec);
                    }
                    "expires" => expires = v.parse::<i64>().ok(),
                    _ => {}
                }
            }
        }
    }
    (url, channel_id, channel_name, token, expires)
}

pub async fn live_relay_route(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Response {
    if !state.config.stream_relay {
        return (StatusCode::SERVICE_UNAVAILABLE, "stream relay is disabled").into_response();
    }

    let (url_opt, channel_id, channel_name, token_opt, expires_opt) = parse_relay_query(req.uri().query());
    let Some(upstream_url) = url_opt else {
        return (StatusCode::BAD_REQUEST, "missing 'url' parameter").into_response();
    };

    let key = url_hash(&upstream_url);

    // Verify authentication: either valid HMAC stream token or paired household secret
    let authorized = match (token_opt, expires_opt) {
        (Some(token), Some(expires)) => {
            if let Some(secret) = state.auth.household_secret() {
                state.stream_relay.verify_stream_token(&secret, &key, &token, expires, state.wall_ms())
            } else {
                false
            }
        }
        _ => false,
    };

    if !authorized {
        return (StatusCode::UNAUTHORIZED, "invalid or expired stream token").into_response();
    }

    let (pre_roll, mut rx_broadcast, guard) = match state.stream_relay.subscribe(&upstream_url, channel_id, channel_name).await {
        Ok(t) => t,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("failed to subscribe: {e}")).into_response(),
    };

    let (tx, rx_mpsc) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(64);

    tokio::spawn(async move {
        let _guard = guard; // Drops when this task ends, decrementing active subscribers
        for chunk in pre_roll {
            if tx.send(Ok(chunk)).await.is_err() {
                return;
            }
        }
        loop {
            match rx_broadcast.recv().await {
                Ok(chunk) => {
                    if tx.send(Ok(chunk)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Client lagged behind live edge: resume with next chunk
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    let stream = futures_util::stream::unfold(rx_mpsc, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
            (header::CONNECTION, "keep-alive"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

pub async fn status_route(
    State(state): State<Arc<AppState>>,
) -> Response {
    let summaries = state.stream_relay.active_summaries();
    let total_saved = state.stream_relay.total_streams_saved();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&serde_json::json!({
            "activeRelays": summaries,
            "totalStreamsSaved": total_saved,
        })).unwrap_or_else(|_| "{}".to_string())
    ).into_response()
}

fn parse_timeshift_query(query_str: Option<&str>) -> (Option<String>, Option<i64>, Option<String>, Option<i64>, Option<String>, Option<String>) {
    let mut url = None;
    let mut from_ms = None;
    let mut token = None;
    let mut expires = None;
    let mut channel_id = None;
    let mut channel_name = None;

    if let Some(q) = query_str {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                match k {
                    "url" => {
                        let decoded = v.replace("%3A", ":").replace("%2F", "/").replace("%3F", "?").replace("%3D", "=").replace("%26", "&");
                        url = Some(decoded);
                    }
                    "from_ms" => from_ms = v.parse::<i64>().ok(),
                    "token" => {
                        let dec = v.replace("%2B", "+").replace("%2F", "/").replace("%3D", "=");
                        token = Some(dec);
                    }
                    "expires" => expires = v.parse::<i64>().ok(),
                    "channel_id" => channel_id = Some(v.to_string()),
                    "channel_name" => channel_name = Some(v.to_string()),
                    _ => {}
                }
            }
        }
    }
    (url, from_ms, token, expires, channel_id, channel_name)
}

pub async fn timeshift_stream_route(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Response {
    let (url_opt, from_ms_opt, token_opt, expires_opt, channel_id, channel_name) = parse_timeshift_query(req.uri().query());
    let Some(upstream_url) = url_opt else {
        return (StatusCode::BAD_REQUEST, "missing 'url' parameter").into_response();
    };
    let from_ms = from_ms_opt.unwrap_or(0);

    let key = url_hash(&upstream_url);
    let authorized = match (token_opt, expires_opt) {
        (Some(token), Some(expires)) => {
            if let Some(secret) = state.auth.household_secret() {
                state.stream_relay.verify_stream_token(&secret, &key, &token, expires, state.wall_ms())
            } else {
                false
            }
        }
        _ => false,
    };

    if !authorized {
        return (StatusCode::UNAUTHORIZED, "invalid or expired stream token").into_response();
    }

    // Ensure relay is active or start it
    let relay = match state.stream_relay.get_relay(&key) {
        Some(r) => r,
        None => {
            // Subscribe to initiate the relay if not already running
            match state.stream_relay.subscribe(&upstream_url, channel_id, channel_name).await {
                Ok((_, _, _)) => match state.stream_relay.get_relay(&key) {
                    Some(r) => r,
                    None => return (StatusCode::BAD_GATEWAY, "failed to start stream relay").into_response(),
                },
                Err(e) => return (StatusCode::BAD_GATEWAY, format!("failed to subscribe: {e}")).into_response(),
            }
        }
    };

    let timeshift = match &relay.timeshift {
        Some(ts) => ts.clone(),
        None => return (StatusCode::BAD_REQUEST, "timeshift is disabled on this Companion").into_response(),
    };

    relay.active_subscribers.fetch_add(1, Ordering::SeqCst);
    let guard = SubscriberGuard { relay: relay.clone() };

    let (tx, rx_mpsc) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(64);

    let ts = timeshift.clone();
    let r = relay.clone();
    tokio::spawn(async move {
        let _guard = guard;
        crate::timeshift::spawn_timeshift_reader(ts, r, from_ms, tx);
    });

    let stream = futures_util::stream::unfold(rx_mpsc, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "video/mp2t"),
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
            (header::CONNECTION, "keep-alive"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

pub async fn timeshift_info_route(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Response {
    let (url_opt, _, token_opt, expires_opt, _, _) = parse_timeshift_query(req.uri().query());
    let Some(upstream_url) = url_opt else {
        return (StatusCode::BAD_REQUEST, "missing 'url' parameter").into_response();
    };

    let key = url_hash(&upstream_url);
    let authorized = match (token_opt, expires_opt) {
        (Some(token), Some(expires)) => {
            if let Some(secret) = state.auth.household_secret() {
                state.stream_relay.verify_stream_token(&secret, &key, &token, expires, state.wall_ms())
            } else {
                false
            }
        }
        _ => false,
    };

    if !authorized {
        return (StatusCode::UNAUTHORIZED, "invalid or expired stream token").into_response();
    }

    match state.stream_relay.timeshift_info(&key) {
        Some(info) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&info).unwrap_or_else(|_| "{}".to_string()),
        )
            .into_response(),
        None => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::json!({
                "available": false,
                "reason": "no active timeshift buffer for this stream"
            })
            .to_string(),
        )
            .into_response(),
    }
}

fn parse_pause_query(query_str: Option<&str>) -> (Option<String>, Option<bool>, Option<String>, Option<i64>) {
    let mut url = None;
    let mut paused = None;
    let mut token = None;
    let mut expires = None;

    if let Some(q) = query_str {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                match k {
                    "url" => {
                        let decoded = v.replace("%3A", ":").replace("%2F", "/").replace("%3F", "?").replace("%3D", "=").replace("%26", "&");
                        url = Some(decoded);
                    }
                    "paused" => paused = match v {
                        "true" | "1" => Some(true),
                        _ => Some(false),
                    },
                    "token" => {
                        let dec = v.replace("%2B", "+").replace("%2F", "/").replace("%3D", "=");
                        token = Some(dec);
                    }
                    "expires" => expires = v.parse::<i64>().ok(),
                    _ => {}
                }
            }
        }
    }
    (url, paused, token, expires)
}

pub async fn pause_route(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Response {
    let (url_opt, paused_opt, token_opt, expires_opt) = parse_pause_query(req.uri().query());
    let Some(upstream_url) = url_opt else {
        return (StatusCode::BAD_REQUEST, "missing 'url' parameter").into_response();
    };

    let key = url_hash(&upstream_url);
    let authorized = match (token_opt, expires_opt) {
        (Some(token), Some(expires)) => {
            if let Some(secret) = state.auth.household_secret() {
                state.stream_relay.verify_stream_token(&secret, &key, &token, expires, state.wall_ms())
            } else {
                false
            }
        }
        _ => false,
    };

    if !authorized {
        return (StatusCode::UNAUTHORIZED, "invalid or expired stream token").into_response();
    }

    state.stream_relay.set_paused_subscriber(&key, paused_opt.unwrap_or(false));
    (StatusCode::OK, "ok").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_hash_is_deterministic() {
        let u1 = "http://example.com/live/user/pass/1.ts";
        let u2 = "http://example.com/live/user/pass/1.ts";
        let u3 = "http://example.com/live/user/pass/2.ts";
        assert_eq!(url_hash(u1), url_hash(u2));
        assert_ne!(url_hash(u1), url_hash(u3));
    }

    #[test]
    fn sanitize_stream_url_redacts_credentials() {
        let raw = "http://user:secret123@iptv.server:8080/live/user/secret123/100.ts?token=xyz987&other=ok";
        let sanitized = sanitize_stream_url(raw);
        assert!(!sanitized.contains("secret123"));
        assert!(!sanitized.contains("xyz987"));
        assert!(sanitized.contains("token=***"));
        assert!(sanitized.contains("other=ok"));
    }

    #[tokio::test]
    async fn subscriber_guard_tracks_active_count() {
        let wall: crate::state::WallClock = Arc::new(|| 1_000_000);
        let mgr = StreamRelayManager::new(wall, None, false, 0);

        let url = "http://127.0.0.1:9/fake.ts";
        let (_pre1, _rx1, guard1) = mgr.subscribe(url, Some("ch1".into()), Some("Channel 1".into())).await.unwrap();

        let summaries = mgr.active_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].active_subscribers, 1);
        assert_eq!(summaries[0].upstream_streams_saved, 0);

        let (_pre2, _rx2, guard2) = mgr.subscribe(url, Some("ch1".into()), Some("Channel 1".into())).await.unwrap();
        let summaries = mgr.active_summaries();
        assert_eq!(summaries[0].active_subscribers, 2);
        assert_eq!(summaries[0].upstream_streams_saved, 1); // 1 connection saved!

        drop(guard1);
        let summaries = mgr.active_summaries();
        assert_eq!(summaries[0].active_subscribers, 1);

        drop(guard2);
        let summaries = mgr.active_summaries();
        assert_eq!(summaries[0].active_subscribers, 0);
    }

    #[test]
    fn token_generation_and_verification() {
        let wall: crate::state::WallClock = Arc::new(|| 1_000_000);
        let mgr = StreamRelayManager::new(wall, None, false, 0);
        let secret = [42u8; 32];
        let wrong_secret = [99u8; 32];
        let key = url_hash("http://provider.tv/1.ts");
        let expires = 2_000_000;

        let token = mgr.generate_stream_token(&secret, &key, expires);
        assert!(mgr.verify_stream_token(&secret, &key, &token, expires, 1_500_000));
        assert!(!mgr.verify_stream_token(&secret, &key, &token, expires, 2_000_001)); // expired
        assert!(!mgr.verify_stream_token(&wrong_secret, &key, &token, expires, 1_500_000)); // wrong secret
        assert!(!mgr.verify_stream_token(&secret, "other-key", &token, expires, 1_500_000)); // wrong key
    }
}
