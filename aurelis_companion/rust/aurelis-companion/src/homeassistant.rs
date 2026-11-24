//! Home Assistant Media Player Bridge & Smart TV Notification Hub.
//!
//! 1. Receives TV playback state reports from Aurelis TV boxes:
//!    `POST /v1/tv/state`
//!    Synchronizes active playback to Home Assistant via REST API:
//!    `POST /api/states/media_player.aurelis_<slug>`
//!    Enabling native media player controls and room lighting automations in Home Assistant.
//!
//! 2. Receives smart home notifications (e.g. Doorbell, Camera motion, Alarms):
//!    `POST /v1/notifications/push`
//!    Buffers notifications and distributes them to connected TV boxes:
//!    `GET /v1/notifications/poll?since_ms=<timestamp>&device_id=<device>`
//!    `GET /v1/notifications/active`

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Extension;
use crate::state::{AppState, WallClock};

pub const MAX_NOTIFICATIONS_STORED: usize = 50;
pub const DEFAULT_NOTIFICATION_DURATION_SEC: u32 = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TvPlaybackState {
    pub device_id: String,
    pub device_name: String,
    pub state: String, // "playing", "paused", "idle", "off"
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub channel_name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub position_sec: Option<u64>,
    #[serde(default)]
    pub duration_sec: Option<u64>,
    #[serde(default)]
    pub volume: Option<f32>,
    #[serde(default)]
    pub is_muted: Option<bool>,
    #[serde(default)]
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PushNotificationRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    pub message: String,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default = "default_duration")]
    pub duration_sec: u32,
    #[serde(default = "default_priority")]
    pub priority: String, // "normal", "high", "critical"
    #[serde(default)]
    pub target_device: Option<String>,
}

fn default_duration() -> u32 {
    DEFAULT_NOTIFICATION_DURATION_SEC
}

fn default_priority() -> String {
    "normal".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TvNotification {
    pub id: String,
    pub title: String,
    pub message: String,
    pub image_url: Option<String>,
    pub duration_sec: u32,
    pub priority: String,
    pub target_device: Option<String>,
    pub timestamp_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TvRemoteCommand {
    pub id: String,
    pub device_id: String,
    pub action: String, // "play", "pause", "play_pause", "stop", "next_channel", "previous_channel", "tune"
    #[serde(default)]
    pub value: Option<String>,
    pub timestamp_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostTvCommandRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub device_id: String,
    pub action: String,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationsConfigRequest {
    #[serde(default)]
    pub notifications_enabled: Option<bool>,
    #[serde(default)]
    pub suppress_cctv_duplicates: Option<bool>,
    #[serde(default)]
    pub cctv_active_cameras: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatchupConfigRequest {
    #[serde(default)]
    pub catchup_ad_skip_enabled: Option<bool>,
    #[serde(default)]
    pub catchup_live_handoff_enabled: Option<bool>,
}

pub struct HaManager {
    enabled: bool,
    hass_url: Option<String>,
    hass_token: Option<String>,
    data_dir: Option<std::path::PathBuf>,
    client: reqwest::Client,
    devices: Mutex<HashMap<String, TvPlaybackState>>,
    notifications: Mutex<VecDeque<TvNotification>>,
    notifications_enabled: AtomicBool,
    suppress_cctv_duplicates: AtomicBool,
    cctv_active_cameras: Mutex<HashSet<String>>,
    catchup_ad_skip_enabled: AtomicBool,
    catchup_live_handoff_enabled: AtomicBool,
    commands: Mutex<HashMap<String, VecDeque<TvRemoteCommand>>>,
    wall: WallClock,
}

impl HaManager {
    pub fn new(enabled: bool, hass_url: Option<String>, hass_token: Option<String>, wall: WallClock) -> Arc<Self> {
        Self::with_data_dir(enabled, hass_url, hass_token, None, wall)
    }

    pub fn with_data_dir(
        enabled: bool,
        hass_url: Option<String>,
        hass_token: Option<String>,
        data_dir: Option<std::path::PathBuf>,
        wall: WallClock,
    ) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let mut initial_devices = HashMap::new();
        if let Some(dir) = &data_dir {
            let path = dir.join("ha_devices.json");
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(list) = serde_json::from_slice::<Vec<TvPlaybackState>>(&bytes) {
                    for mut dev in list {
                        // On Companion restart, treat previously playing devices as idle until they report again.
                        if dev.state == "playing" || dev.state == "paused" {
                            dev.state = "idle".to_string();
                        }
                        initial_devices.insert(dev.device_id.clone(), dev);
                    }
                }
            }
        }

        Arc::new(Self {
            enabled,
            hass_url,
            hass_token,
            data_dir,
            client,
            devices: Mutex::new(initial_devices),
            notifications: Mutex::new(VecDeque::with_capacity(MAX_NOTIFICATIONS_STORED)),
            notifications_enabled: AtomicBool::new(true),
            suppress_cctv_duplicates: AtomicBool::new(true),
            cctv_active_cameras: Mutex::new(HashSet::new()),
            catchup_ad_skip_enabled: AtomicBool::new(true),
            catchup_live_handoff_enabled: AtomicBool::new(true),
            commands: Mutex::new(HashMap::new()),
            wall,
        })
    }

    fn persist_devices(&self, devs: &HashMap<String, TvPlaybackState>) {
        if let Some(dir) = &self.data_dir {
            let _ = std::fs::create_dir_all(dir);
            let list: Vec<&TvPlaybackState> = devs.values().collect();
            if let Ok(bytes) = serde_json::to_vec_pretty(&list) {
                let _ = crate::bundle_store::write_atomic(dir, "ha_devices.json", &bytes);
            }
        }
    }

    pub fn notifications_enabled(&self) -> bool {
        self.notifications_enabled.load(Ordering::Relaxed)
    }

    pub fn set_notifications_enabled(&self, val: bool) {
        self.notifications_enabled.store(val, Ordering::Relaxed);
    }

    pub fn suppress_cctv_duplicates(&self) -> bool {
        self.suppress_cctv_duplicates.load(Ordering::Relaxed)
    }

    pub fn set_suppress_cctv_duplicates(&self, val: bool) {
        self.suppress_cctv_duplicates.store(val, Ordering::Relaxed);
    }

    pub async fn register_cctv_active_cameras(&self, cameras: Vec<String>) {
        let mut active = self.cctv_active_cameras.lock().await;
        for c in cameras {
            let trimmed = c.trim().to_string();
            if !trimmed.is_empty() {
                active.insert(trimmed);
            }
        }
    }

    pub async fn get_cctv_active_cameras(&self) -> Vec<String> {
        let active = self.cctv_active_cameras.lock().await;
        active.iter().cloned().collect()
    }

    pub fn catchup_ad_skip_enabled(&self) -> bool {
        self.catchup_ad_skip_enabled.load(Ordering::Relaxed)
    }

    pub fn set_catchup_ad_skip_enabled(&self, val: bool) {
        self.catchup_ad_skip_enabled.store(val, Ordering::Relaxed);
    }

    pub fn catchup_live_handoff_enabled(&self) -> bool {
        self.catchup_live_handoff_enabled.load(Ordering::Relaxed)
    }

    pub fn set_catchup_live_handoff_enabled(&self, val: bool) {
        self.catchup_live_handoff_enabled.store(val, Ordering::Relaxed);
    }

    pub async fn queue_command(&self, req: PostTvCommandRequest) -> TvRemoteCommand {
        let now = (self.wall)();
        let cmd_id = req.id.unwrap_or_else(|| format!("cmd_{}_{}", now, rand::random::<u16>()));
        let cmd = TvRemoteCommand {
            id: cmd_id,
            device_id: req.device_id.clone(),
            action: req.action.to_ascii_lowercase(),
            value: req.value,
            timestamp_ms: now,
        };

        let mut map = self.commands.lock().await;
        let queue = map.entry(req.device_id).or_insert_with(VecDeque::new);
        if queue.len() >= 20 {
            queue.pop_front();
        }
        queue.push_back(cmd.clone());
        cmd
    }

    pub async fn poll_commands(&self, device_id: &str) -> Vec<TvRemoteCommand> {
        let mut map = self.commands.lock().await;
        if let Some(queue) = map.get_mut(device_id) {
            queue.drain(..).collect()
        } else {
            Vec::new()
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn has_ha_credentials(&self) -> bool {
        self.hass_url.is_some() && self.hass_token.is_some()
    }

    pub async fn update_playback_state(&self, mut state: TvPlaybackState) -> (String, bool) {
        if state.updated_at_ms <= 0 {
            state.updated_at_ms = (self.wall)();
        }

        {
            let mut devs = self.devices.lock().await;
            devs.insert(state.device_id.clone(), state.clone());
            self.persist_devices(&devs);
        }

        self.push_entity_to_ha(&state).await
    }

    /// Re-publishes all persisted `media_player.aurelis_*` entities to Home Assistant (e.g. after
    /// Companion or HA restart), transitioning stale playing/paused devices (>120s without update)
    /// to `idle`.
    pub async fn republish_known_devices(&self) {
        if !self.enabled || !self.has_ha_credentials() {
            return;
        }
        let now = (self.wall)();
        let snapshot: Vec<TvPlaybackState> = {
            let mut devs = self.devices.lock().await;
            let mut changed = false;
            for dev in devs.values_mut() {
                if (dev.state == "playing" || dev.state == "paused") && now - dev.updated_at_ms > 120_000 {
                    dev.state = "idle".to_string();
                    dev.updated_at_ms = now;
                    changed = true;
                }
            }
            if changed {
                self.persist_devices(&devs);
            }
            devs.values().cloned().collect()
        };
        for dev in snapshot {
            let _ = self.push_entity_to_ha(&dev).await;
        }
    }

    async fn push_entity_to_ha(&self, state: &TvPlaybackState) -> (String, bool) {
        let entity_slug = slugify_device_id(&state.device_id);
        let entity_id = format!("media_player.aurelis_{}", entity_slug);

        let mut synced_to_ha = false;
        if self.enabled {
            if let (Some(url), Some(token)) = (&self.hass_url, &self.hass_token) {
                let clean_base = url.trim_end_matches('/');
                let target_url = format!("{clean_base}/states/{entity_id}");

                let mut attrs = serde_json::Map::new();
                attrs.insert("friendly_name".into(), serde_json::Value::String(format!("Aurelis {}", state.device_name)));
                attrs.insert("supported_features".into(), serde_json::Value::Number(20495.into()));
                attrs.insert("device_class".into(), serde_json::Value::String("tv".into()));
                attrs.insert("app_name".into(), serde_json::Value::String("Aurelis TV".into()));
                if let Some(mt) = &state.media_type {
                    attrs.insert("media_content_type".into(), serde_json::Value::String(mt.clone()));
                } else {
                    attrs.insert("media_content_type".into(), serde_json::Value::String("video".into()));
                }
                if let Some(t) = &state.title {
                    attrs.insert("media_title".into(), serde_json::Value::String(t.clone()));
                }
                if let Some(ch) = &state.channel_name {
                    attrs.insert("media_channel".into(), serde_json::Value::String(ch.clone()));
                }
                if let Some(art) = &state.artwork_url {
                    attrs.insert("entity_picture".into(), serde_json::Value::String(art.clone()));
                }
                if let Some(pos) = state.position_sec {
                    attrs.insert("media_position".into(), serde_json::Value::Number(pos.into()));
                }
                if let Some(dur) = state.duration_sec {
                    attrs.insert("media_duration".into(), serde_json::Value::Number(dur.into()));
                }
                if let Some(vol) = state.volume {
                    if let Some(num) = serde_json::Number::from_f64(vol as f64) {
                        attrs.insert("volume_level".into(), serde_json::Value::Number(num));
                    }
                }
                if let Some(muted) = state.is_muted {
                    attrs.insert("is_volume_muted".into(), serde_json::Value::Bool(muted));
                }

                let payload = serde_json::json!({
                    "state": state.state,
                    "attributes": attrs
                });

                let res = self.client.post(&target_url)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .body(payload.to_string())
                    .send()
                    .await;

                synced_to_ha = match res {
                    Ok(resp) => resp.status().is_success(),
                    Err(e) => {
                        eprintln!("[HA] Failed to sync state to {target_url}: {e}");
                        false
                    }
                };
            }
        }

        (entity_id, synced_to_ha)
    }

    pub async fn get_all_states(&self) -> Vec<TvPlaybackState> {
        let devs = self.devices.lock().await;
        devs.values().cloned().collect()
    }

    pub async fn push_notification(&self, req: PushNotificationRequest) -> Result<TvNotification, String> {
        if !self.notifications_enabled() {
            return Err("notifications_disabled_by_companion_master_switch".into());
        }

        if self.suppress_cctv_duplicates() {
            let active = self.cctv_active_cameras.lock().await;
            let title_lower = req.title.to_ascii_lowercase();
            let msg_lower = req.message.to_ascii_lowercase();
            for cam in active.iter() {
                let cam_lower = cam.to_ascii_lowercase();
                if title_lower.contains(&cam_lower) || msg_lower.contains(&cam_lower) {
                    return Err(format!("suppressed_duplicate_cctv_alert: matches active Aurelis camera '{cam}'"));
                }
            }
        }

        let now = (self.wall)();
        let notif_id = req.id.unwrap_or_else(|| {
            format!("notif_{}_{}", now, rand::random::<u16>())
        });

        let notif = TvNotification {
            id: notif_id,
            title: req.title,
            message: req.message,
            image_url: req.image_url,
            duration_sec: if req.duration_sec > 0 { req.duration_sec } else { DEFAULT_NOTIFICATION_DURATION_SEC },
            priority: req.priority,
            target_device: req.target_device,
            timestamp_ms: now,
        };

        let mut buffer = self.notifications.lock().await;
        if buffer.len() >= MAX_NOTIFICATIONS_STORED {
            buffer.pop_front();
        }
        buffer.push_back(notif.clone());

        Ok(notif)
    }

    pub async fn poll_notifications(&self, since_ms: i64, device_id: Option<&str>) -> Vec<TvNotification> {
        let buffer = self.notifications.lock().await;
        buffer.iter()
            .filter(|n| n.timestamp_ms > since_ms)
            .filter(|n| match (&n.target_device, device_id) {
                (None, _) => true,
                (Some(target), Some(dev)) => target.eq_ignore_ascii_case(dev),
                (Some(_), None) => true,
            })
            .cloned()
            .collect()
    }

    pub async fn get_active_notifications(&self) -> Vec<TvNotification> {
        let now = (self.wall)();
        let buffer = self.notifications.lock().await;
        buffer.iter()
            .filter(|n| {
                let age_ms = now - n.timestamp_ms;
                let duration_ms = (n.duration_sec as i64) * 1000;
                age_ms >= 0 && age_ms <= duration_ms
            })
            .cloned()
            .collect()
    }
}

pub fn slugify_device_id(raw: &str) -> String {
    let mut out = String::new();
    let mut last_was_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !out.is_empty() {
            out.push('_');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_end_matches('_');
    if trimmed.is_empty() {
        "tv".to_string()
    } else {
        trimmed.chars().take(32).collect()
    }
}

// ---- Axum Handlers ----------------------------------------------------------------------------

pub async fn handle_report_tv_state(
    State(state): State<Arc<AppState>>,
    Extension(_): Extension<()>,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, &'static str)> {
    if !state.card().paired {
        return Err((StatusCode::FORBIDDEN, "companion is not paired"));
    }

    let payload: TvPlaybackState = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return Err((StatusCode::BAD_REQUEST, "invalid TvPlaybackState JSON")),
    };

    let (entity_id, ha_synced) = state.ha.update_playback_state(payload).await;

    let reply = serde_json::json!({
        "ok": true,
        "entityId": entity_id,
        "haSynced": ha_synced,
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(reply.to_string().into())
        .unwrap())
}

pub async fn handle_get_tv_states(
    State(state): State<Arc<AppState>>,
    Extension(_): Extension<()>,
) -> Result<Response, (StatusCode, &'static str)> {
    if !state.card().paired {
        return Err((StatusCode::FORBIDDEN, "companion is not paired"));
    }

    let states = state.ha.get_all_states().await;
    let reply = serde_json::json!({
        "devices": states,
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(reply.to_string().into())
        .unwrap())
}

pub async fn handle_push_notification(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, &'static str)> {
    let payload: PushNotificationRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return Err((StatusCode::BAD_REQUEST, "invalid PushNotificationRequest JSON")),
    };

    match state.ha.push_notification(payload).await {
        Ok(notif) => {
            let reply = serde_json::json!({
                "ok": true,
                "id": notif.id,
                "timestampMs": notif.timestamp_ms,
            });
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(reply.to_string().into())
                .unwrap())
        }
        Err(reason) => {
            let reply = serde_json::json!({
                "ok": false,
                "suppressed": true,
                "reason": reason,
            });
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(reply.to_string().into())
                .unwrap())
        }
    }
}

pub async fn handle_post_tv_command(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, &'static str)> {
    let payload: PostTvCommandRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return Err((StatusCode::BAD_REQUEST, "invalid PostTvCommandRequest JSON")),
    };

    let cmd = state.ha.queue_command(payload).await;
    let reply = serde_json::json!({
        "ok": true,
        "id": cmd.id,
        "deviceId": cmd.device_id,
        "action": cmd.action,
        "timestampMs": cmd.timestamp_ms,
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(reply.to_string().into())
        .unwrap())
}

pub async fn handle_poll_tv_commands(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> Result<Response, (StatusCode, &'static str)> {
    if !state.card().paired {
        return Err((StatusCode::FORBIDDEN, "companion is not paired"));
    }

    let query = req.uri().query().unwrap_or("");
    let mut device_id = String::new();
    for part in query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            if k == "device_id" {
                device_id = v.to_string();
            }
        }
    }

    let commands = if !device_id.is_empty() {
        state.ha.poll_commands(&device_id).await
    } else {
        Vec::new()
    };

    let reply = serde_json::json!({
        "commands": commands,
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(reply.to_string().into())
        .unwrap())
}

pub async fn handle_get_notifications_config(
    State(state): State<Arc<AppState>>,
) -> Result<Response, (StatusCode, &'static str)> {
    let cameras = state.ha.get_cctv_active_cameras().await;
    let reply = serde_json::json!({
        "notificationsEnabled": state.ha.notifications_enabled(),
        "suppressCctvDuplicates": state.ha.suppress_cctv_duplicates(),
        "cctvActiveCameras": cameras,
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(reply.to_string().into())
        .unwrap())
}

pub async fn handle_set_notifications_config(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, &'static str)> {
    let payload: NotificationsConfigRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return Err((StatusCode::BAD_REQUEST, "invalid NotificationsConfigRequest JSON")),
    };

    if let Some(enabled) = payload.notifications_enabled {
        state.ha.set_notifications_enabled(enabled);
    }
    if let Some(suppress) = payload.suppress_cctv_duplicates {
        state.ha.set_suppress_cctv_duplicates(suppress);
    }
    if let Some(cams) = payload.cctv_active_cameras {
        state.ha.register_cctv_active_cameras(cams).await;
    }

    let cameras = state.ha.get_cctv_active_cameras().await;
    let reply = serde_json::json!({
        "ok": true,
        "notificationsEnabled": state.ha.notifications_enabled(),
        "suppressCctvDuplicates": state.ha.suppress_cctv_duplicates(),
        "cctvActiveCameras": cameras,
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(reply.to_string().into())
        .unwrap())
}

pub async fn handle_get_catchup_config(
    State(state): State<Arc<AppState>>,
) -> Result<Response, (StatusCode, &'static str)> {
    let reply = serde_json::json!({
        "catchupAdSkipEnabled": state.ha.catchup_ad_skip_enabled(),
        "catchupLiveHandoffEnabled": state.ha.catchup_live_handoff_enabled(),
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(reply.to_string().into())
        .unwrap())
}

pub async fn handle_set_catchup_config(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, &'static str)> {
    let payload: CatchupConfigRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return Err((StatusCode::BAD_REQUEST, "invalid CatchupConfigRequest JSON")),
    };

    if let Some(skip) = payload.catchup_ad_skip_enabled {
        state.ha.set_catchup_ad_skip_enabled(skip);
    }
    if let Some(live) = payload.catchup_live_handoff_enabled {
        state.ha.set_catchup_live_handoff_enabled(live);
    }

    let reply = serde_json::json!({
        "ok": true,
        "catchupAdSkipEnabled": state.ha.catchup_ad_skip_enabled(),
        "catchupLiveHandoffEnabled": state.ha.catchup_live_handoff_enabled(),
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(reply.to_string().into())
        .unwrap())
}

pub async fn handle_poll_notifications(
    State(state): State<Arc<AppState>>,
    Extension(_): Extension<()>,
    req: Request,
) -> Result<Response, (StatusCode, &'static str)> {
    if !state.card().paired {
        return Err((StatusCode::FORBIDDEN, "companion is not paired"));
    }

    let query = req.uri().query().unwrap_or("");
    let mut since_ms: i64 = 0;
    let mut device_id: Option<String> = None;

    for part in query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            if k == "since_ms" {
                since_ms = v.parse::<i64>().unwrap_or(0);
            } else if k == "device_id" {
                device_id = Some(v.to_string());
            }
        }
    }

    let notifs = state.ha.poll_notifications(since_ms, device_id.as_deref()).await;
    let reply = serde_json::json!({
        "notifications": notifs,
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(reply.to_string().into())
        .unwrap())
}

pub async fn handle_get_active_notifications(
    State(state): State<Arc<AppState>>,
    Extension(_): Extension<()>,
) -> Result<Response, (StatusCode, &'static str)> {
    if !state.card().paired {
        return Err((StatusCode::FORBIDDEN, "companion is not paired"));
    }

    let active = state.ha.get_active_notifications().await;
    let reply = serde_json::json!({
        "active": active,
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(reply.to_string().into())
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_clock(now: i64) -> WallClock {
        Arc::new(move || now)
    }

    #[test]
    fn test_slugify() {
        assert_eq!(slugify_device_id("Box R 4K Plus"), "box_r_4k_plus");
        assert_eq!(slugify_device_id("192.168.3.246:5555"), "192_168_3_246_5555");
        assert_eq!(slugify_device_id("Living-Room--Box"), "living_room_box");
        assert_eq!(slugify_device_id(""), "tv");
        assert_eq!(slugify_device_id("???"), "tv");
    }

    #[tokio::test]
    async fn test_playback_state_storage() {
        let clock = test_clock(1000);
        let mgr = HaManager::new(false, None, None, clock);

        let state = TvPlaybackState {
            device_id: "box_r_4k_plus".to_string(),
            device_name: "Living Room Box".to_string(),
            state: "playing".to_string(),
            media_type: Some("live".to_string()),
            channel_name: Some("Sky Sports Main Event".to_string()),
            title: Some("Super Sunday".to_string()),
            artwork_url: Some("http://pic.png".to_string()),
            position_sec: Some(10),
            duration_sec: Some(100),
            volume: Some(0.75),
            is_muted: Some(false),
            updated_at_ms: 0,
        };

        let (entity_id, synced) = mgr.update_playback_state(state).await;
        assert_eq!(entity_id, "media_player.aurelis_box_r_4k_plus");
        assert!(!synced); // disabled HA credentials in test

        let states = mgr.get_all_states().await;
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].title.as_deref(), Some("Super Sunday"));
        assert_eq!(states[0].updated_at_ms, 1000);
    }

    #[tokio::test]
    async fn test_notification_buffering_and_poll() {
        let clock = test_clock(5000);
        let mgr = HaManager::new(true, None, None, clock);

        let req1 = PushNotificationRequest {
            id: Some("n1".to_string()),
            title: "Doorbell".to_string(),
            message: "Front Door ring".to_string(),
            image_url: Some("http://camera/snap.jpg".to_string()),
            duration_sec: 8,
            priority: "high".to_string(),
            target_device: None,
        };

        let req2 = PushNotificationRequest {
            id: Some("n2".to_string()),
            title: "Washer".to_string(),
            message: "Laundry done".to_string(),
            image_url: None,
            duration_sec: 10,
            priority: "normal".to_string(),
            target_device: Some("bedroom_box".to_string()),
        };

        let _ = mgr.push_notification(req1).await;
        let _ = mgr.push_notification(req2).await;

        // Poll for all since 4000
        let all = mgr.poll_notifications(4000, None).await;
        assert_eq!(all.len(), 2);

        // Poll for bedroom_box
        let bedroom = mgr.poll_notifications(4000, Some("bedroom_box")).await;
        assert_eq!(bedroom.len(), 2); // broadcast n1 + targeted n2

        // Poll for living_room (should only see n1)
        let living = mgr.poll_notifications(4000, Some("living_room")).await;
        assert_eq!(living.len(), 1);
        assert_eq!(living[0].id, "n1");

        // Poll with since >= 5000 (none)
        let none = mgr.poll_notifications(5000, None).await;
        assert_eq!(none.len(), 0);
    }

    #[tokio::test]
    async fn test_notification_master_switch_and_cctv_dedup() {
        let clock = test_clock(5000);
        let mgr = HaManager::new(true, None, None, clock);

        // Register Front Door Camera as active in Aurelis CCTV
        mgr.register_cctv_active_cameras(vec!["Front Door".to_string()]).await;

        let req1 = PushNotificationRequest {
            id: Some("n1".to_string()),
            title: "Front Door".to_string(),
            message: "Motion detected".to_string(),
            image_url: None,
            duration_sec: 8,
            priority: "normal".to_string(),
            target_device: None,
        };

        // Suppressed because it matches active CCTV camera
        let res1 = mgr.push_notification(req1).await;
        assert!(res1.is_err());
        assert!(res1.unwrap_err().contains("suppressed_duplicate_cctv_alert"));

        // Push non-matching notification (Garage Door)
        let req2 = PushNotificationRequest {
            id: Some("n2".to_string()),
            title: "Garage Door".to_string(),
            message: "Left open".to_string(),
            image_url: None,
            duration_sec: 8,
            priority: "normal".to_string(),
            target_device: None,
        };
        let res2 = mgr.push_notification(req2).await;
        assert!(res2.is_ok());

        // Toggle master switch off
        mgr.set_notifications_enabled(false);
        assert!(!mgr.notifications_enabled());

        let req3 = PushNotificationRequest {
            id: Some("n3".to_string()),
            title: "Garage Door".to_string(),
            message: "Closed".to_string(),
            image_url: None,
            duration_sec: 8,
            priority: "normal".to_string(),
            target_device: None,
        };
        let res3 = mgr.push_notification(req3).await;
        assert!(res3.is_err());
        assert!(res3.unwrap_err().contains("notifications_disabled_by_companion_master_switch"));
    }

    #[tokio::test]
    async fn test_tv_command_queue_and_poll() {
        let clock = test_clock(6000);
        let mgr = HaManager::new(true, None, None, clock);

        let req = PostTvCommandRequest {
            id: Some("cmd1".to_string()),
            device_id: "tv_lounge".to_string(),
            action: "play".to_string(),
            value: None,
        };

        let cmd = mgr.queue_command(req).await;
        assert_eq!(cmd.id, "cmd1");
        assert_eq!(cmd.action, "play");

        let polled = mgr.poll_commands("tv_lounge").await;
        assert_eq!(polled.len(), 1);
        assert_eq!(polled[0].action, "play");

        // Polling again drains the queue
        let empty = mgr.poll_commands("tv_lounge").await;
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn test_catchup_config_toggle() {
        let clock = test_clock(1000);
        let mgr = HaManager::new(true, None, None, clock);
        assert!(mgr.catchup_ad_skip_enabled());
        assert!(mgr.catchup_live_handoff_enabled());

        mgr.set_catchup_ad_skip_enabled(false);
        assert!(!mgr.catchup_ad_skip_enabled());

        mgr.set_catchup_live_handoff_enabled(false);
        assert!(!mgr.catchup_live_handoff_enabled());
    }
}
