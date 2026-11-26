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
/// How long a television waits for the relay's first bytes before it is told to go direct.
pub const FIRST_BYTES_WAIT: Duration = Duration::from_secs(6);
/// Reconnects after a stream that was flowing drops, before the relay gives up.
pub const MAX_RECONNECTS: u32 = 3;
/// Refusals (before any bytes, with no success since) after which the relay steps aside.
pub const REFUSALS_TO_STEP_ASIDE: usize = 3;
/// How long the relay stays aside after the last refusal before televisions may try it again.
pub const STEP_ASIDE_MS: i64 = 6 * 3600 * 1000;

// The relay must never hammer a provider (owner, 26 Nov 2026). Every upstream request is one of the
// household's few provider connections (1-2 on these accounts), and a provider at its limit answers
// every extra one with a refusal, so retrying is what keeps it refusing.
/// After a provider refuses, it is not asked again for this long; doubled per refusal in a row.
pub const HOST_COOLDOWN_MS: i64 = 60_000;
pub const HOST_COOLDOWN_MAX_MS: i64 = 10 * 60_000;
/// The most upstream connections the relay holds to one provider at once.
pub const MAX_UPSTREAM_PER_HOST: usize = 2;
/// Waits before each reconnect after a flowing stream drops, in seconds; then the relay stops.
pub const RECONNECT_WAITS_S: [u64; 3] = [2, 5, 10];

/// The provider a stream belongs to, by host name.
pub fn host_of(url: &str) -> String {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).unwrap_or_default()
}

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
    /// Why the relay stopped, once it has: televisions waiting for it are told at once.
    pub failure: Arc<RwLock<Option<String>>>,
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
    /// When the provider refused the Companion before sending anything, and why (newest last).
    refusals: std::sync::Mutex<VecDeque<(i64, String)>>,
    last_success_ms: AtomicI64,
    /// Where the refusals are kept, so a restart does not offer a refused relay again.
    refusals_file: Option<std::path::PathBuf>,
    /// Per provider host: not to be asked again before this time, and refusals in a row.
    cooldowns: std::sync::Mutex<HashMap<String, (i64, u32)>>,
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
        // A live stream never ends: no total timeout (15 s cut every relayed channel, 26 Nov 2026).
        // A silent stream counts as dropped.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .read_timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let refusals_file = data_dir.as_ref().map(|d| d.join("stream_relay_refusals.json"));
        let (refusals, last_success) = refusals_file.as_ref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice::<(Vec<(i64, String)>, i64)>(&b).ok())
            .map(|(list, ok)| (list.into_iter().collect::<VecDeque<_>>(), ok))
            .unwrap_or_default();
        let timeshift_dir = if timeshift_enabled { data_dir } else { None };

        Arc::new(Self {
            relays: Arc::new(RwLock::new(HashMap::new())),
            wall,
            client,
            timeshift_dir,
            timeshift_minutes,
            refusals: std::sync::Mutex::new(refusals),
            last_success_ms: AtomicI64::new(last_success),
            refusals_file,
            cooldowns: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Why this provider must not be asked now, or `None`.
    pub fn cooldown_reason(&self, host: &str) -> Option<String> {
        let now = (self.wall)();
        let map = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
        let (until, streak) = *map.get(host)?;
        (until > now).then(|| format!(
            "{host} refused the Companion {streak} time{} in a row; not asking it again for {} s",
            if streak == 1 { "" } else { "s" }, (until - now + 999) / 1000,
        ))
    }

    fn note_host_refusal(&self, host: &str) {
        let now = (self.wall)();
        let mut map = self.cooldowns.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(host.to_string()).or_insert((0, 0));
        entry.1 += 1;
        let wait = (HOST_COOLDOWN_MS << (entry.1 - 1).min(8)).min(HOST_COOLDOWN_MAX_MS);
        entry.0 = now + wait;
    }

    fn note_host_success(&self, host: &str) {
        self.cooldowns.lock().unwrap_or_else(|e| e.into_inner()).remove(host);
    }

    fn save_refusals(&self, list: &VecDeque<(i64, String)>) {
        if let Some(f) = &self.refusals_file {
            let body = (list.iter().cloned().collect::<Vec<_>>(), self.last_success_ms.load(Ordering::Relaxed));
            if let Ok(bytes) = serde_json::to_vec(&body) {
                let _ = std::fs::write(f, bytes);
            }
        }
    }

    fn note_refusal(&self, why: &str) {
        let now = (self.wall)();
        let mut list = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        list.push_back((now, why.to_string()));
        while list.len() > 16 {
            list.pop_front();
        }
        self.save_refusals(&list);
    }

    fn note_success(&self) {
        self.last_success_ms.store((self.wall)(), Ordering::Relaxed);
        let list = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        self.save_refusals(&list);
    }

    /// Why televisions should not use the relay now, or `None`. When the provider refuses the
    /// Companion itself (26 Nov 2026: HTTP 502 on every channel, the televisions reaching the
    /// provider through a VPN the Companion is not on), every channel change waited for the relay
    /// to fail before playing direct, about 11 s a zap. After [`REFUSALS_TO_STEP_ASIDE`] refusals
    /// with no success since, the card withdraws the relay for [`STEP_ASIDE_MS`] and says why.
    pub fn stepped_aside(&self) -> Option<String> {
        let now = (self.wall)();
        let since_success = self.last_success_ms.load(Ordering::Relaxed);
        let list = self.refusals.lock().unwrap_or_else(|e| e.into_inner());
        let recent: Vec<&(i64, String)> = list.iter()
            .filter(|(at, _)| *at > since_success && now - *at < STEP_ASIDE_MS).collect();
        if recent.len() < REFUSALS_TO_STEP_ASIDE {
            return None;
        }
        let (_, last) = recent.last()?;
        Some(format!(
            "the IPTV provider refused the Companion {} times ({last}); televisions play channels directly until it is tried again",
            recent.len()
        ))
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
                    // A new upstream connection: never to a provider that just refused, and never
                    // more than MAX_UPSTREAM_PER_HOST to one provider. The television goes direct.
                    let host = host_of(upstream_url);
                    if let Some(why) = self.cooldown_reason(&host) {
                        return Err(why);
                    }
                    let held = write.values().filter(|r| host_of(&r.upstream_url) == host).count();
                    if held >= MAX_UPSTREAM_PER_HOST {
                        return Err(format!("the Companion already holds {held} connections to {host}; play this one directly"));
                    }
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
                        failure: Arc::new(RwLock::new(None)),
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
    let mut reconnects = 0u32;
    let mut why_stopped: Option<String> = None;
    let host = host_of(&relay.upstream_url);

    'outer: loop {
        if reconnects > 0 {
            // A pause before every reconnect, longer each time; never straight back.
            let Some(wait) = RECONNECT_WAITS_S.get(reconnects as usize - 1) else { break 'outer };
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(*wait)) => {},
                _ = stop_rx.changed() => break 'outer,
            }
            if relay.active_subscribers.load(Ordering::Relaxed) == 0 && !relay.has_paused_subscriber.load(Ordering::Relaxed) {
                break 'outer;
            }
        }
        if let Some(why) = manager.cooldown_reason(&host) {
            why_stopped = Some(why);
            break 'outer;
        }
        let req = manager.client.get(&relay.upstream_url)
            .header("User-Agent", "VLC/3.0.18 LibVLC/3.0.18")
            .header("Accept", "*/*");

        let failed = match req.send().await {
            Ok(r) if r.status().is_success() => Ok(r),
            Ok(r) => Err(format!("the provider answered HTTP {}", r.status())),
            Err(e) => Err(format!("the provider could not be reached: {e}")),
        };
        let resp = match failed {
            Ok(r) => r,
            Err(why) => {
                eprintln!("stream relay {}: {why}", relay.key);
                // Before any bytes, a refusal is final: the televisions go direct at once rather
                // than wait while the relay knocks on the provider (a refused provider is often
                // one at its connection limit, and every retry is another connection).
                manager.note_host_refusal(&host);
                if relay.upstream_bytes_received.load(Ordering::Relaxed) == 0 {
                    manager.note_refusal(&why);
                }
                why_stopped = Some(why);
                break 'outer;
            }
        };

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
                            if relay.upstream_bytes_received.load(Ordering::Relaxed) == 0 {
                                manager.note_success();
                                manager.note_host_success(&host);
                            }
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
                            why_stopped = Some(format!("the provider's stream dropped: {e}"));
                            break;
                        }
                        None => {
                            eprintln!("stream relay {}: upstream stream EOF", relay.key);
                            why_stopped = Some("the provider ended the stream".into());
                            break;
                        }
                    }
                }
            }
        }

        if relay.active_subscribers.load(Ordering::Relaxed) == 0 && !relay.has_paused_subscriber.load(Ordering::Relaxed) {
            break 'outer;
        }
        reconnects += 1;
        if reconnects > MAX_RECONNECTS {
            break 'outer;
        }
    }

    // Tell every television still attached that this relay is finished, so its player moves on
    // (to the direct stream it holds as a fallback) instead of waiting on a silent connection.
    *relay.failure.write().unwrap_or_else(|e| e.into_inner()) =
        Some(why_stopped.unwrap_or_else(|| "the relay stopped".into()));
    let _ = relay.stop_tx.send(true);
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
    // A television that has not re-read the card since the relay stepped aside is answered here,
    // without the provider being asked.
    if let Some(why) = state.stream_relay.stepped_aside() {
        return (StatusCode::SERVICE_UNAVAILABLE, format!("Companion relay: {why}")).into_response();
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

    let (mut pre_roll, mut rx_broadcast, guard) = match state.stream_relay.subscribe(&upstream_url, channel_id, channel_name).await {
        Ok(t) => t,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("failed to subscribe: {e}")).into_response(),
    };
    let relay = guard.relay.clone();
    let mut stopped = relay.stop_tx.subscribe();

    // Answer only once there is something to play. Answering 200 at once and then sending nothing
    // while the provider refused left every player buffering until its own timeout on every
    // channel change (26 Nov 2026). A refusal now reaches the television within seconds as a 502,
    // and its player switches to the direct stream.
    if pre_roll.is_empty() {
        let first = tokio::time::timeout(FIRST_BYTES_WAIT, async {
            loop {
                if let Some(why) = relay.failure.read().unwrap_or_else(|e| e.into_inner()).clone() {
                    return Err(why);
                }
                tokio::select! {
                    got = rx_broadcast.recv() => match got {
                        Ok(chunk) => return Ok(chunk),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return Err("the relay stopped".to_string()),
                    },
                    _ = stopped.changed() => continue,
                    _ = tokio::time::sleep(Duration::from_millis(200)) => continue,
                }
            }
        }).await;
        match first {
            Ok(Ok(chunk)) => pre_roll.push(chunk),
            Ok(Err(why)) => {
                drop(guard);
                return (StatusCode::BAD_GATEWAY, format!("Companion relay: {why}")).into_response();
            }
            Err(_) => {
                drop(guard);
                return (StatusCode::GATEWAY_TIMEOUT, format!("Companion relay: no video from the provider within {} s", FIRST_BYTES_WAIT.as_secs())).into_response();
            }
        }
    }

    let (tx, rx_mpsc) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(64);

    tokio::spawn(async move {
        let _guard = guard; // Drops when this task ends, decrementing active subscribers
        for chunk in pre_roll {
            if tx.send(Ok(chunk)).await.is_err() {
                return;
            }
        }
        loop {
            tokio::select! {
                // The television hung up: let go now, not when the next chunk fails to send.
                _ = tx.closed() => break,
                // The relay finished: end this television's stream so its player moves on.
                _ = stopped.changed() => break,
                got = rx_broadcast.recv() => match got {
                    Ok(chunk) => {
                        if tx.send(Ok(chunk)).await.is_err() {
                            break;
                        }
                    }
                    // Client lagged behind live edge: resume with next chunk
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
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

    async fn upstream(status: u16, seconds: u64) -> String {
        upstream_counted(status, seconds).await.0
    }

    async fn upstream_counted(status: u16, seconds: u64) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = axum::Router::new().route("/live.ts", axum::routing::get(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
            if status != 200 {
                return (StatusCode::from_u16(status).unwrap(), "nope").into_response();
            }
            let started = tokio::time::Instant::now();
            let body = futures_util::stream::unfold((), move |_| async move {
                if started.elapsed() >= Duration::from_secs(seconds) { return None; }
                tokio::time::sleep(Duration::from_millis(100)).await;
                Some((Ok::<_, std::io::Error>(Bytes::from(vec![0x47u8; 1880])), ()))
            });
            Body::from_stream(body).into_response()
        }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/live.ts"), hits)
    }

    fn movable_clock() -> (crate::state::WallClock, Arc<AtomicI64>) {
        let now = Arc::new(AtomicI64::new(crate::state::now_ms()));
        let c = now.clone();
        (Arc::new(move || c.load(Ordering::SeqCst)), now)
    }

    /// Owner, 26 Nov 2026: "stream relay must never hammer the server".
    #[tokio::test]
    async fn ten_quick_zaps_to_a_refusing_provider_ask_it_once() {
        let (url, hits) = upstream_counted(502, 0).await;
        let mgr = StreamRelayManager::new(Arc::new(crate::state::now_ms), None, false, 0);
        let (_, _, guard) = mgr.subscribe(&url, None, None).await.unwrap();
        let mut stopped = guard.relay.stop_tx.subscribe();
        tokio::time::timeout(Duration::from_secs(3), stopped.changed()).await.unwrap().unwrap();
        drop(guard);
        for _ in 0..9 {
            let why = mgr.subscribe(&url, None, None).await.err().expect("refused without asking");
            assert!(why.contains("not asking it again"), "{why}");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the provider was asked once");
    }

    #[tokio::test]
    async fn never_more_than_two_connections_to_one_provider() {
        let (url, hits) = upstream_counted(200, 30).await;
        let mgr = StreamRelayManager::new(Arc::new(crate::state::now_ms), None, false, 0);
        let (_, _, a) = mgr.subscribe(&format!("{url}?a"), None, None).await.unwrap();
        let (_, _, b) = mgr.subscribe(&format!("{url}?b"), None, None).await.unwrap();
        let third = mgr.subscribe(&format!("{url}?c"), None, None).await.err().expect("third refused");
        assert!(third.contains("already holds 2 connections"), "{third}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        drop((a, b));
    }

    /// 26 Nov 2026: a refused provider kept the relay retrying while every television buffered.
    #[tokio::test]
    async fn a_refused_provider_stops_the_relay_at_once_and_says_why() {
        let url = upstream(502, 0).await;
        let mgr = StreamRelayManager::new(std::sync::Arc::new(crate::state::now_ms), None, false, 0);
        let (_, mut rx, guard) = mgr.subscribe(&url, None, Some("CNN".into())).await.unwrap();
        let relay = guard.relay.clone();
        let started = Instant::now();
        let mut stopped = relay.stop_tx.subscribe();
        tokio::time::timeout(Duration::from_secs(3), stopped.changed()).await.expect("stopped within 3 s").unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(relay.failure.read().unwrap().as_deref().unwrap_or("").contains("502"), "{:?}", relay.failure.read().unwrap());
        assert!(mgr.active_summaries().is_empty(), "gone from the list");
        assert!(rx.try_recv().is_err());
    }

    /// The relay's client had a 15 s total timeout, which cut every relayed channel.
    /// Run with `--ignored` (about 18 s).
    #[tokio::test]
    #[ignore]
    async fn a_relayed_channel_keeps_flowing_past_fifteen_seconds() {
        let url = upstream(200, 60).await;
        let mgr = StreamRelayManager::new(std::sync::Arc::new(crate::state::now_ms), None, false, 0);
        let (_, mut rx, guard) = mgr.subscribe(&url, None, None).await.unwrap();
        let started = Instant::now();
        let mut bytes = 0usize;
        while started.elapsed() < Duration::from_secs(18) {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(chunk)) => bytes += chunk.len(),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                other => panic!("stream stopped after {:?}: {other:?}", started.elapsed()),
            }
        }
        assert!(bytes > 150 * 1880, "{bytes}");
        assert!(guard.relay.failure.read().unwrap().is_none());
    }

    #[tokio::test]
    async fn the_relay_steps_aside_after_three_refusals_and_says_why() {
        let url = upstream(502, 0).await;
        let (wall, now) = movable_clock();
        let mgr = StreamRelayManager::new(wall, None, false, 0);
        for n in 1..=3 {
            assert!(mgr.stepped_aside().is_none(), "not after {} refusals", n - 1);
            let (_, _, guard) = mgr.subscribe(&url, None, None).await.unwrap();
            let mut stopped = guard.relay.stop_tx.subscribe();
            tokio::time::timeout(Duration::from_secs(3), stopped.changed()).await.unwrap().unwrap();
            now.fetch_add(HOST_COOLDOWN_MAX_MS + 1, Ordering::SeqCst); // past the cool-down
        }
        let why = mgr.stepped_aside().expect("aside after three");
        assert!(why.contains("502") && why.contains("3 times"), "{why}");
        // A success clears it.
        mgr.note_success();
        assert!(mgr.stepped_aside().is_none());
    }

    #[tokio::test]
    async fn stepping_aside_survives_a_restart() {
        let dir = crate::household::TempDir::new();
        let url = upstream(502, 0).await;
        let (wall, now) = movable_clock();
        let mgr = StreamRelayManager::new(wall.clone(), Some(dir.0.clone()), false, 0);
        for _ in 0..3 {
            let (_, _, guard) = mgr.subscribe(&url, None, None).await.unwrap();
            let mut stopped = guard.relay.stop_tx.subscribe();
            tokio::time::timeout(Duration::from_secs(3), stopped.changed()).await.unwrap().unwrap();
            now.fetch_add(HOST_COOLDOWN_MAX_MS + 1, Ordering::SeqCst);
        }
        assert!(mgr.stepped_aside().is_some());
        let again = StreamRelayManager::new(wall, Some(dir.0.clone()), false, 0);
        assert!(again.stepped_aside().is_some(), "still aside after a restart");
    }
}

