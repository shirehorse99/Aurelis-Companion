//! Household IPTV Tuner Arbiter and Concurrent Connection Lease Pool.
//!
//! IPTV subscriptions typically limit concurrent streams (e.g. 1, 2, or 3 connections).
//! Exceeding this limit causes provider 401s, stream drops, or account lockouts.
//!
//! The Tuner Arbiter provides central household coordination:
//! 1. Allocates and enforces tuner leases per provider/host.
//! 2. Reclaims inactive streams via a 30-second heartbeat window.
//! 3. Enables multi-room stream sharing: if TV B requests the same stream that TV A is
//!    already watching, Companion grants the lease as `shared: true` and hands out a local
//!    live relay URL (`/v1/relay/live`), consuming ZERO additional provider connections!

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Extension;
use crate::state::{AppState, WallClock};
use crate::stream_relay::StreamRelayManager;

pub const DEFAULT_LEASE_EXPIRY_SEC: u32 = 30;
pub const HEARTBEAT_EXPIRY_MS: i64 = (DEFAULT_LEASE_EXPIRY_SEC as i64) * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TunerLeaseRequest {
    pub device_id: String,
    pub device_name: String,
    pub provider_id: String,
    pub stream_url: String,
    pub channel_name: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_connections() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveViewer {
    pub device_id: String,
    pub device_name: String,
    pub channel_name: String,
    pub stream_url: String,
    pub started_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TunerConflict {
    pub active_viewers: Vec<ActiveViewer>,
    pub max_connections: u32,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TunerLeaseResponse {
    pub granted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    pub shared: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    pub expires_in_sec: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<TunerConflict>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TunerHeartbeatRequest {
    pub lease_id: String,
    pub device_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TunerHeartbeatResponse {
    pub extended: bool,
    pub expires_in_sec: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TunerReleaseRequest {
    pub lease_id: String,
    pub device_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TunerLease {
    pub lease_id: String,
    pub device_id: String,
    pub device_name: String,
    pub provider_id: String,
    pub stream_url: String,
    pub channel_name: String,
    pub max_connections: u32,
    pub acquired_at_ms: i64,
    pub last_heartbeat_ms: i64,
    pub shared: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TunerStatus {
    pub active_leases: Vec<TunerLease>,
    pub total_active: usize,
}

pub struct TunerStore {
    inner: Mutex<TunerInner>,
    clock: WallClock,
}

struct TunerInner {
    leases: HashMap<String, TunerLease>,
}

impl TunerStore {
    pub fn new(clock: WallClock) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(TunerInner {
                leases: HashMap::new(),
            }),
            clock,
        })
    }

    pub fn set_clock(&mut self, clock: WallClock) {
        self.clock = clock;
    }

    fn now_ms(&self) -> i64 {
        (self.clock)()
    }

    pub async fn sweep_expired(&self) {
        let now = self.now_ms();
        let mut inner = self.inner.lock().await;
        inner.leases.retain(|_, lease| {
            now - lease.last_heartbeat_ms < HEARTBEAT_EXPIRY_MS
        });
    }

    pub async fn acquire(
        &self,
        req: TunerLeaseRequest,
        secret: &[u8; 32],
        stream_relay: Option<&StreamRelayManager>,
    ) -> TunerLeaseResponse {
        let now = self.now_ms();
        let mut inner = self.inner.lock().await;

        // 1. Purge expired leases first
        inner.leases.retain(|_, lease| {
            now - lease.last_heartbeat_ms < HEARTBEAT_EXPIRY_MS
        });

        let norm_stream_url = normalize_stream_url(&req.stream_url);

        // 2. Check if this same device already holds a lease for this stream
        for lease in inner.leases.values_mut() {
            if lease.device_id == req.device_id && normalize_stream_url(&lease.stream_url) == norm_stream_url {
                lease.last_heartbeat_ms = now;
                return TunerLeaseResponse {
                    granted: true,
                    lease_id: Some(lease.lease_id.clone()),
                    shared: lease.shared,
                    relay_url: None,
                    expires_in_sec: DEFAULT_LEASE_EXPIRY_SEC,
                    conflict: None,
                };
            }
        }

        // 3. Multi-room same-stream check: is another TV already playing this exact stream?
        let mut matching_active_lease = None;
        for lease in inner.leases.values() {
            if lease.device_id != req.device_id && normalize_stream_url(&lease.stream_url) == norm_stream_url {
                matching_active_lease = Some(lease.clone());
                break;
            }
        }

        if let Some(_active) = matching_active_lease {
            // Another TV is watching this stream! Offer a shared lease via local Companion relay.
            let lease_id = format!("lease_{}_{}", req.device_id, now);
            let relay_url = if let Some(relay) = stream_relay {
                let expires_at_ms = now + (3600 * 4 * 1000); // 4 hour relay token
                let token = relay.generate_stream_token(secret, &req.stream_url, expires_at_ms);
                Some(format!(
                    "/v1/relay/live?url={}&token={}&expires={}",
                    urlencoding_encode(&req.stream_url),
                    token,
                    expires_at_ms
                ))
            } else {
                None
            };

            inner.leases.insert(lease_id.clone(), TunerLease {
                lease_id: lease_id.clone(),
                device_id: req.device_id,
                device_name: req.device_name,
                provider_id: req.provider_id,
                stream_url: req.stream_url,
                channel_name: req.channel_name,
                max_connections: req.max_connections,
                acquired_at_ms: now,
                last_heartbeat_ms: now,
                shared: true,
            });

            return TunerLeaseResponse {
                granted: true,
                lease_id: Some(lease_id),
                shared: true,
                relay_url,
                expires_in_sec: DEFAULT_LEASE_EXPIRY_SEC,
                conflict: None,
            };
        }

        // 4. Count active independent (non-shared) streams on this provider
        let mut active_for_provider: Vec<ActiveViewer> = Vec::new();
        for lease in inner.leases.values() {
            if lease.provider_id == req.provider_id && !lease.shared {
                active_for_provider.push(ActiveViewer {
                    device_id: lease.device_id.clone(),
                    device_name: lease.device_name.clone(),
                    channel_name: lease.channel_name.clone(),
                    stream_url: lease.stream_url.clone(),
                    started_at_ms: lease.acquired_at_ms,
                });
            }
        }

        let max_conn = if req.max_connections == 0 { 2 } else { req.max_connections };

        if active_for_provider.len() >= (max_conn as usize) {
            // Max concurrent streams reached for this provider!
            return TunerLeaseResponse {
                granted: false,
                lease_id: None,
                shared: false,
                relay_url: None,
                expires_in_sec: 0,
                conflict: Some(TunerConflict {
                    active_viewers: active_for_provider,
                    max_connections: max_conn,
                    reason: format!(
                        "Household limit of {} concurrent stream{} reached for this provider",
                        max_conn,
                        if max_conn == 1 { "" } else { "s" }
                    ),
                }),
            };
        }

        // 5. Capacity available -> grant lease
        let lease_id = format!("lease_{}_{}", req.device_id, now);
        inner.leases.insert(lease_id.clone(), TunerLease {
            lease_id: lease_id.clone(),
            device_id: req.device_id,
            device_name: req.device_name,
            provider_id: req.provider_id,
            stream_url: req.stream_url,
            channel_name: req.channel_name,
            max_connections: max_conn,
            acquired_at_ms: now,
            last_heartbeat_ms: now,
            shared: false,
        });

        TunerLeaseResponse {
            granted: true,
            lease_id: Some(lease_id),
            shared: false,
            relay_url: None,
            expires_in_sec: DEFAULT_LEASE_EXPIRY_SEC,
            conflict: None,
        }
    }

    pub async fn heartbeat(&self, req: TunerHeartbeatRequest) -> TunerHeartbeatResponse {
        let now = self.now_ms();
        let mut inner = self.inner.lock().await;

        if let Some(lease) = inner.leases.get_mut(&req.lease_id) {
            if lease.device_id == req.device_id {
                lease.last_heartbeat_ms = now;
                return TunerHeartbeatResponse {
                    extended: true,
                    expires_in_sec: DEFAULT_LEASE_EXPIRY_SEC,
                };
            }
        }

        TunerHeartbeatResponse {
            extended: false,
            expires_in_sec: 0,
        }
    }

    pub async fn release(&self, req: TunerReleaseRequest) -> bool {
        let mut inner = self.inner.lock().await;
        if let Some(lease) = inner.leases.get(&req.lease_id) {
            if lease.device_id == req.device_id {
                inner.leases.remove(&req.lease_id);
                return true;
            }
        }
        false
    }

    pub async fn status(&self) -> TunerStatus {
        let now = self.now_ms();
        let mut inner = self.inner.lock().await;
        inner.leases.retain(|_, lease| {
            now - lease.last_heartbeat_ms < HEARTBEAT_EXPIRY_MS
        });

        let mut list: Vec<TunerLease> = inner.leases.values().cloned().collect();
        list.sort_by_key(|l| l.acquired_at_ms);

        TunerStatus {
            total_active: list.len(),
            active_leases: list,
        }
    }

    pub async fn reset(&self) {
        let mut inner = self.inner.lock().await;
        inner.leases.clear();
    }
}

fn normalize_stream_url(url: &str) -> String {
    let s = if let Some(pos) = url.find('?') {
        &url[..pos]
    } else {
        url
    };
    s.trim_end_matches('/').to_string()
}

fn urlencoding_encode(s: &str) -> String {
    let mut encoded = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{:02X}", b));
        }
    }
    encoded
}

pub async fn acquire_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
    body: axum::body::Bytes,
) -> Response {
    if !state.card().capabilities.tuner {
        return session.sealed_reason(StatusCode::SERVICE_UNAVAILABLE, "tuner arbiter is disabled or companion is paused");
    }
    let req: TunerLeaseRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return session.sealed_reason(StatusCode::BAD_REQUEST, &format!("invalid request JSON: {e}")),
    };
    let secret = match state.auth.household_secret() {
        Some(s) => s,
        None => return session.sealed_reason(StatusCode::UNAUTHORIZED, "unpaired"),
    };
    let relay = if state.card().capabilities.stream_relay {
        Some(&*state.stream_relay)
    } else {
        None
    };
    let res = state.tuner.acquire(req, &secret, relay).await;
    session.sealed_json(StatusCode::OK, &serde_json::to_value(&res).unwrap_or_default())
}

pub async fn heartbeat_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
    body: axum::body::Bytes,
) -> Response {
    let req: TunerHeartbeatRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return session.sealed_reason(StatusCode::BAD_REQUEST, &format!("invalid request JSON: {e}")),
    };
    let res = state.tuner.heartbeat(req).await;
    session.sealed_json(StatusCode::OK, &serde_json::to_value(&res).unwrap_or_default())
}

pub async fn release_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
    body: axum::body::Bytes,
) -> Response {
    let req: TunerReleaseRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return session.sealed_reason(StatusCode::BAD_REQUEST, &format!("invalid request JSON: {e}")),
    };
    let success = state.tuner.release(req).await;
    session.sealed_json(StatusCode::OK, &serde_json::json!({ "released": success }))
}

pub async fn status_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<crate::auth::Session>,
) -> Response {
    let status = state.tuner.status().await;
    session.sealed_json(StatusCode::OK, &serde_json::to_value(&status).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn make_test_clock(initial_ms: i64) -> (WallClock, Arc<AtomicI64>) {
        let current = Arc::new(AtomicI64::new(initial_ms));
        let cur_clone = current.clone();
        let clock = Arc::new(move || cur_clone.load(Ordering::SeqCst));
        (clock, current)
    }

    #[tokio::test]
    async fn acquire_under_capacity_is_granted() {
        let (clock, _) = make_test_clock(100_000);
        let store = TunerStore::new(clock);
        let secret = [0x42u8; 32];

        let req = TunerLeaseRequest {
            device_id: "tv_living_room".into(),
            device_name: "Living Room TV".into(),
            provider_id: "provider_alpha".into(),
            stream_url: "http://stream.example.com/live/ch1.ts".into(),
            channel_name: "Sky Sports 1".into(),
            max_connections: 2,
        };

        let res = store.acquire(req, &secret, None).await;
        assert!(res.granted);
        assert!(!res.shared);
        assert!(res.lease_id.is_some());
        assert_eq!(res.expires_in_sec, DEFAULT_LEASE_EXPIRY_SEC);
        assert!(res.conflict.is_none());

        let status = store.status().await;
        assert_eq!(status.total_active, 1);
        assert_eq!(status.active_leases[0].channel_name, "Sky Sports 1");
    }

    #[tokio::test]
    async fn same_device_same_stream_refreshes_lease() {
        let (clock, _) = make_test_clock(100_000);
        let store = TunerStore::new(clock);
        let secret = [0x42u8; 32];

        let req = TunerLeaseRequest {
            device_id: "tv_living_room".into(),
            device_name: "Living Room TV".into(),
            provider_id: "provider_alpha".into(),
            stream_url: "http://stream.example.com/live/ch1.ts".into(),
            channel_name: "Sky Sports 1".into(),
            max_connections: 2,
        };

        let res1 = store.acquire(req.clone(), &secret, None).await;
        assert!(res1.granted);
        let lease_id1 = res1.lease_id.unwrap();

        let res2 = store.acquire(req, &secret, None).await;
        assert!(res2.granted);
        assert_eq!(res2.lease_id.unwrap(), lease_id1);

        let status = store.status().await;
        assert_eq!(status.total_active, 1);
    }

    #[tokio::test]
    async fn multi_room_same_stream_is_shared() {
        let (clock, _) = make_test_clock(100_000);
        let store = TunerStore::new(clock.clone());
        let secret = [0x42u8; 32];

        let req1 = TunerLeaseRequest {
            device_id: "tv_living_room".into(),
            device_name: "Living Room TV".into(),
            provider_id: "provider_alpha".into(),
            stream_url: "http://stream.example.com/live/ch1.ts".into(),
            channel_name: "Sky Sports 1".into(),
            max_connections: 1, // Only 1 connection allowed!
        };

        let res1 = store.acquire(req1, &secret, None).await;
        assert!(res1.granted);
        assert!(!res1.shared);

        // TV 2 tunes into the same stream despite max_connections == 1
        let req2 = TunerLeaseRequest {
            device_id: "tv_bedroom".into(),
            device_name: "Bedroom TV".into(),
            provider_id: "provider_alpha".into(),
            stream_url: "http://stream.example.com/live/ch1.ts".into(),
            channel_name: "Sky Sports 1".into(),
            max_connections: 1,
        };

        let res2 = store.acquire(req2, &secret, None).await;
        assert!(res2.granted);
        assert!(res2.shared); // Granted as shared!
        assert!(res2.conflict.is_none());

        let status = store.status().await;
        assert_eq!(status.total_active, 2);
    }

    #[tokio::test]
    async fn max_connections_conflict_on_different_streams() {
        let (clock, _) = make_test_clock(100_000);
        let store = TunerStore::new(clock);
        let secret = [0x42u8; 32];

        let req1 = TunerLeaseRequest {
            device_id: "tv_living_room".into(),
            device_name: "Living Room TV".into(),
            provider_id: "provider_alpha".into(),
            stream_url: "http://stream.example.com/live/ch1.ts".into(),
            channel_name: "Sky Sports 1".into(),
            max_connections: 1,
        };
        let res1 = store.acquire(req1, &secret, None).await;
        assert!(res1.granted);

        // TV 2 requests a DIFFERENT stream on the same 1-connection provider
        let req2 = TunerLeaseRequest {
            device_id: "tv_bedroom".into(),
            device_name: "Bedroom TV".into(),
            provider_id: "provider_alpha".into(),
            stream_url: "http://stream.example.com/live/ch2.ts".into(),
            channel_name: "TNT Sports 1".into(),
            max_connections: 1,
        };
        let res2 = store.acquire(req2, &secret, None).await;
        assert!(!res2.granted);
        assert!(res2.conflict.is_some());
        let conflict = res2.conflict.unwrap();
        assert_eq!(conflict.max_connections, 1);
        assert_eq!(conflict.active_viewers.len(), 1);
        assert_eq!(conflict.active_viewers[0].device_name, "Living Room TV");
        assert_eq!(conflict.active_viewers[0].channel_name, "Sky Sports 1");
    }

    #[tokio::test]
    async fn heartbeat_and_release_lifecycle() {
        let (clock, time_ref) = make_test_clock(100_000);
        let store = TunerStore::new(clock);
        let secret = [0x42u8; 32];

        let req = TunerLeaseRequest {
            device_id: "tv_living_room".into(),
            device_name: "Living Room TV".into(),
            provider_id: "provider_alpha".into(),
            stream_url: "http://stream.example.com/live/ch1.ts".into(),
            channel_name: "Sky Sports 1".into(),
            max_connections: 1,
        };

        let res = store.acquire(req, &secret, None).await;
        let lease_id = res.lease_id.unwrap();

        // Heartbeat succeeds
        let hb = store.heartbeat(TunerHeartbeatRequest {
            lease_id: lease_id.clone(),
            device_id: "tv_living_room".into(),
        }).await;
        assert!(hb.extended);

        // Advance clock past expiry (35 seconds)
        time_ref.store(135_000, Ordering::SeqCst);
        store.sweep_expired().await;

        let status = store.status().await;
        assert_eq!(status.total_active, 0); // Expired automatically!

        // Acquire again at 135_000
        let res2 = store.acquire(TunerLeaseRequest {
            device_id: "tv_living_room".into(),
            device_name: "Living Room TV".into(),
            provider_id: "provider_alpha".into(),
            stream_url: "http://stream.example.com/live/ch1.ts".into(),
            channel_name: "Sky Sports 1".into(),
            max_connections: 1,
        }, &secret, None).await;
        let lease_id2 = res2.lease_id.unwrap();

        // Explicit release
        let released = store.release(TunerReleaseRequest {
            lease_id: lease_id2,
            device_id: "tv_living_room".into(),
        }).await;
        assert!(released);

        let status2 = store.status().await;
        assert_eq!(status2.total_active, 0);
    }
}
