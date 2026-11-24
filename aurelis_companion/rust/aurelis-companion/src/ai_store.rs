//! `/v1/ai/get` and `/v1/ai/put`: the household's shared AI answers.
//!
//! A television offers every answer it caches and asks for the ones it misses. The key is the
//! television's own identity string (it already encodes profile, date and content ids), so the
//! Companion never interprets it. The value is kept exactly as sent, and `storedAtMs` is the
//! television's stamp: it is carried back unchanged, never re-dated, and when two offers meet the
//! newer stamp is kept.

use crate::auth::{Refusal, Session};
use crate::state::AppState;
use crate::store::{BlobStore, Conflict, Meta, StoreRefusal};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Extension;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::AsyncReadExt;

/// A value over this is refused with a reason. It is 1 MiB less 8 KiB so that anything passing
/// still fits, sealed and wrapped in its JSON, inside the 1 MiB request body cap: the television
/// always gets a sealed `stored:false`, never an unsealed 413.
pub const MAX_VALUE_BYTES: usize = 1024 * 1024 - 8 * 1024;
pub const MAX_KEY_BYTES: usize = 2048;

/// A stamp further ahead of this Companion's clock than the bundle route allows is refused: kept,
/// it would out-rank every honest offer for that key ([`Conflict::KeepNewer`]) and never age out
/// on any television, so one box with a wrong clock would freeze the answer for the house.
pub fn future_stamp_refusal(stored_at_ms: i64, now_ms: i64) -> Option<String> {
    let ahead = stored_at_ms.saturating_sub(now_ms);
    if ahead > crate::bundle_store::MAX_CLOCK_LEAD_MS as i64 {
        Some(format!("not stored: 'storedAtMs' is {} min ahead of this Companion's clock — check the television's clock", ahead / 60_000))
    } else {
        None
    }
}

/// Stores one answer. A newer `storedAtMs` already held wins over an older offer.
pub async fn put(store: &BlobStore, key: &str, value: &Value, stored_at_ms: i64) -> Result<(), StoreRefusal> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(StoreRefusal { reason: format!("not stored: the key is {} bytes; it must be 1 to {MAX_KEY_BYTES}", key.len()) });
    }
    let bytes = serde_json::to_vec(value).map_err(|e| StoreRefusal { reason: format!("not stored: the value cannot be encoded: {e}") })?;
    if bytes.len() > MAX_VALUE_BYTES {
        return Err(StoreRefusal { reason: format!("not stored: the value is {} bytes and the largest kept is {MAX_VALUE_BYTES} (1 MiB less 8 KiB)", bytes.len()) });
    }
    store.put_bytes_with(key, &bytes, Meta::at(stored_at_ms), Conflict::KeepNewer).await
}

/// `Ok(None)` is a plain miss. `Err` is a held answer that could not be read, with the reason
/// (it is also dropped, so the next ask is an honest miss).
pub async fn get(store: &BlobStore, key: &str) -> Result<Option<(Value, i64)>, String> {
    let Some((meta, mut file)) = store.get(key).await else { return Ok(None) };
    let mut bytes = Vec::new();
    if let Err(e) = (&mut file).take(MAX_VALUE_BYTES as u64 + 1).read_to_end(&mut bytes).await {
        return Err(format!("the held answer could not be read: {e}"));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => Ok(Some((v, meta.stored_at_ms))),
        Err(e) => {
            let why = format!("the held answer is not valid JSON ({e}); dropped");
            store.forget(key, &why);
            Err(why)
        }
    }
}

fn parse(body: &Bytes) -> Result<Value, Refusal> {
    serde_json::from_slice(body).map_err(|e| Refusal::new(StatusCode::BAD_REQUEST, format!("request body is not JSON: {e}")))
}

fn key_of(v: &Value) -> Result<&str, Refusal> {
    v.get("key").and_then(Value::as_str).ok_or_else(|| Refusal::new(StatusCode::BAD_REQUEST, "'key' must be text"))
}

pub async fn get_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>, body: Bytes) -> Response {
    let key = match parse(&body).and_then(|v| key_of(&v).map(str::to_string)) {
        Ok(k) => k,
        Err(r) => return r.into_response_sealed(&session),
    };
    match get(&state.stores.ai, &key).await {
        Ok(Some((value, stored_at_ms))) => session.sealed_json(StatusCode::OK, &json!({ "hit": true, "value": value, "storedAtMs": stored_at_ms })),
        Ok(None) => session.sealed_json(StatusCode::OK, &json!({ "hit": false })),
        Err(why) => {
            eprintln!("ai store: {why}");
            session.sealed_json(StatusCode::OK, &json!({ "hit": false, "reason": why }))
        }
    }
}

pub async fn put_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>, body: Bytes) -> Response {
    let v = match parse(&body) {
        Ok(v) => v,
        Err(r) => return r.into_response_sealed(&session),
    };
    let key = match key_of(&v) {
        Ok(k) => k,
        Err(r) => return r.into_response_sealed(&session),
    };
    let Some(value) = v.get("value") else {
        return session.sealed_reason(StatusCode::BAD_REQUEST, "'value' is missing");
    };
    let Some(stored_at_ms) = v.get("storedAtMs").and_then(Value::as_i64) else {
        return session.sealed_reason(StatusCode::BAD_REQUEST, "'storedAtMs' must be a whole number of milliseconds");
    };
    if let Some(why) = future_stamp_refusal(stored_at_ms, state.wall_ms()) {
        eprintln!("ai store: {why}");
        return session.sealed_json(StatusCode::BAD_REQUEST, &json!({ "stored": false, "reason": why }));
    }
    match put(&state.stores.ai, key, value, stored_at_ms).await {
        Ok(()) => session.sealed_json(StatusCode::OK, &json!({ "stored": true })),
        Err(r) => {
            eprintln!("ai store: {}", r.reason);
            session.sealed_json(StatusCode::OK, &json!({ "stored": false, "reason": r.reason }))
        }
    }
}

trait SealedRefusal {
    fn into_response_sealed(self, session: &Session) -> Response;
}

impl SealedRefusal for Refusal {
    fn into_response_sealed(self, session: &Session) -> Response {
        session.sealed_reason(self.status, &self.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{b64, unb64};
    use crate::household::{Household, TempDir};
    use crate::state::{test_config, test_state};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use companion_protocol::{proof, seal};
    use tower::ServiceExt;

    const SECRET: [u8; 32] = [9u8; 32];

    /// A paired Companion on a fresh folder.
    struct Rig {
        _dir: TempDir,
        state: Arc<AppState>,
    }

    fn rig() -> Rig {
        let dir = TempDir::new();
        Household::create(&dir.0, SECRET).expect("household");
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        Rig { state: Arc::new(test_state(config)), _dir: dir }
    }

    impl Rig {
        async fn send(&self, req: Request<Body>) -> (StatusCode, Vec<u8>) {
            let res = crate::router(self.state.clone()).oneshot(req).await.expect("infallible");
            let status = res.status();
            (status, to_bytes(res.into_body(), 4 << 20).await.expect("body").to_vec())
        }

        async fn authed(&self, path: &str, plain: &Value) -> (StatusCode, Value) {
            let req = Request::builder().uri("/v1/challenge").header("x-aurelis-companion", "1").body(Body::empty()).unwrap();
            let (s, b) = self.send(req).await;
            assert_eq!(s, StatusCode::OK);
            let nonce = serde_json::from_slice::<Value>(&b).unwrap()["nonce"].as_str().unwrap().to_string();
            let sealed = seal::seal(plain.to_string().as_bytes(), &SECRET);
            let p = proof::proof(&SECRET, &unb64(&nonce).unwrap(), "POST", path, &sealed);
            let req = Request::builder()
                .method("POST")
                .uri(path)
                .header("x-aurelis-companion", "1")
                .header("x-aurelis-nonce", nonce)
                .header("x-aurelis-proof", b64(&p))
                .body(Body::from(sealed))
                .unwrap();
            let (status, body) = self.send(req).await;
            let opened = seal::open(&body, &SECRET).unwrap_or(body);
            (status, serde_json::from_slice(&opened).unwrap_or(Value::Null))
        }
    }

    #[tokio::test]
    #[allow(non_snake_case)]
    async fn ai_get_returns_value_and_original_storedAtMs() {
        let r = rig();
        let (s, v) = r.authed("/v1/ai/put", &json!({"key": "tvtimes:gb:2026-11-17", "value": "The lead is sport.", "storedAtMs": 1_700_000_123_456i64})).await;
        assert_eq!((s, &v["stored"]), (StatusCode::OK, &json!(true)), "{v}");
        let (s, v) = r.authed("/v1/ai/get", &json!({"key": "tvtimes:gb:2026-11-17"})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["hit"], true);
        assert_eq!(v["value"], "The lead is sport.");
        assert_eq!(v["storedAtMs"], 1_700_000_123_456i64, "never re-dated");
        let (_, miss) = r.authed("/v1/ai/get", &json!({"key": "never-offered"})).await;
        assert_eq!(miss, json!({"hit": false}));
        // The card shows what the answers weigh (rounded up, so a held answer never reads 0).
        assert_eq!(r.state.card().disk.used_mb.ai, 1);
    }

    #[tokio::test]
    #[allow(non_snake_case)]
    async fn ai_put_keeps_newer_storedAtMs_on_conflict() {
        let r = rig();
        let put = |value: &str, at: i64| json!({"key": "k", "value": value, "storedAtMs": at});
        assert_eq!(r.authed("/v1/ai/put", &put("second", 2000)).await.1["stored"], true);
        let (_, older) = r.authed("/v1/ai/put", &put("first", 1000)).await;
        assert_eq!(older["stored"], false);
        assert!(older["reason"].as_str().unwrap().contains("2000"), "{older}");
        let (_, held) = r.authed("/v1/ai/get", &json!({"key": "k"})).await;
        assert_eq!((held["value"].as_str(), held["storedAtMs"].as_i64()), (Some("second"), Some(2000)));
        assert_eq!(r.authed("/v1/ai/put", &put("third", 3000)).await.1["stored"], true);
        let (_, held) = r.authed("/v1/ai/get", &json!({"key": "k"})).await;
        assert_eq!((held["value"].as_str(), held["storedAtMs"].as_i64()), (Some("third"), Some(3000)));
    }

    #[tokio::test]
    async fn a_value_over_1_mib_is_refused_with_its_size() {
        let r = rig();
        let big = Value::String("x".repeat(MAX_VALUE_BYTES));
        let e = put(&r.state.stores.ai, "k", &big, 1).await.unwrap_err();
        assert!(e.reason.contains("8 KiB") && e.reason.contains(&(MAX_VALUE_BYTES + 2).to_string()), "{}", e.reason);
        assert!(get(&r.state.stores.ai, "k").await.unwrap().is_none());
        let ok = Value::String("x".repeat(MAX_VALUE_BYTES - 2));
        put(&r.state.stores.ai, "k", &ok, 1).await.expect("exactly at the limit is kept");
    }

    #[tokio::test]
    async fn the_largest_value_fits_the_sealed_body_and_one_byte_over_gets_a_sealed_refusal() {
        let r = rig();
        let largest = "x".repeat(MAX_VALUE_BYTES - 2);
        let (s, v) = r.authed("/v1/ai/put", &json!({"key": "tvtimes:gb:2026-11-17:profile-with-a-long-identity", "value": largest, "storedAtMs": 1})).await;
        assert_eq!((s, &v["stored"]), (StatusCode::OK, &json!(true)), "{s} {v}");
        let over = "x".repeat(MAX_VALUE_BYTES - 1);
        let (s, v) = r.authed("/v1/ai/put", &json!({"key": "k2", "value": over, "storedAtMs": 1})).await;
        assert_eq!((s, &v["stored"]), (StatusCode::OK, &json!(false)), "{s} {v}");
        assert!(v["reason"].as_str().unwrap().contains("8 KiB"), "{v}");
    }

    #[tokio::test]
    async fn malformed_requests_say_why() {
        let r = rig();
        for (path, body) in [
            ("/v1/ai/get", json!({})),
            ("/v1/ai/get", json!({"key": 5})),
            ("/v1/ai/put", json!({"key": "k", "storedAtMs": 1})),
            ("/v1/ai/put", json!({"key": "k", "value": "v"})),
            ("/v1/ai/put", json!({"key": "", "value": "v", "storedAtMs": 1})),
        ] {
            let (s, v) = r.authed(path, &body).await;
            let refused = s == StatusCode::BAD_REQUEST || v["stored"] == false;
            assert!(refused, "{path} {body} -> {s} {v}");
            assert!(v["reason"].as_str().is_some_and(|x| !x.is_empty()), "{path} {body} -> {v}");
        }
    }

    #[tokio::test]
    async fn the_future_stamp_check_reads_the_states_own_wall_clock() {
        // Pinned in 2001: a stamp 11 min after that is long past by the system clock, so only a
        // check that reads the injected clock refuses it.
        let pinned = 1_000_000_000_000i64;
        let dir = TempDir::new();
        Household::create(&dir.0, SECRET).expect("household");
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let r = Rig { state: Arc::new(test_state(config).with_wall_clock(Arc::new(move || pinned))), _dir: dir };
        let (s, v) = r.authed("/v1/ai/put", &json!({"key": "k", "value": "v", "storedAtMs": pinned + 11 * 60_000})).await;
        assert_eq!((s, &v["stored"]), (StatusCode::BAD_REQUEST, &json!(false)), "{v}");
        let (s, v) = r.authed("/v1/ai/put", &json!({"key": "k", "value": "v", "storedAtMs": pinned + 5 * 60_000})).await;
        assert_eq!((s, &v["stored"]), (StatusCode::OK, &json!(true)), "{v}");
    }

    #[tokio::test]
    async fn a_stamp_more_than_ten_minutes_ahead_is_refused_with_a_reason() {
        let r = rig();
        let now = crate::state::now_ms();
        let far = now + 11 * 60_000;
        let (s, v) = r.authed("/v1/ai/put", &json!({"key": "k", "value": "v", "storedAtMs": far})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["stored"], false);
        assert!(v["reason"].as_str().unwrap().contains("ahead of this Companion's clock"), "{v}");
        assert!(get(&r.state.stores.ai, "k").await.unwrap().is_none(), "nothing kept");
        // A few minutes of drift is still welcome.
        let (s, v) = r.authed("/v1/ai/put", &json!({"key": "k", "value": "v", "storedAtMs": now + 5 * 60_000})).await;
        assert_eq!((s, &v["stored"]), (StatusCode::OK, &json!(true)), "{v}");
    }

    #[test]
    fn future_stamp_boundary() {
        let lead = crate::bundle_store::MAX_CLOCK_LEAD_MS as i64;
        assert!(future_stamp_refusal(1_000 + lead, 1_000).is_none());
        assert!(future_stamp_refusal(1_001 + lead, 1_000).is_some());
        assert!(future_stamp_refusal(i64::MIN, 1_000).is_none());
        assert!(future_stamp_refusal(i64::MAX, i64::MIN + 1).is_some(), "no overflow");
    }

    #[tokio::test]
    async fn ai_routes_need_the_household() {
        let r = rig();
        let req = Request::builder().method("POST").uri("/v1/ai/get").header("x-aurelis-companion", "1").body(Body::from("{}")).unwrap();
        assert_eq!(r.send(req).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unpairing_empties_the_store_and_its_counters() {
        let r = rig();
        r.authed("/v1/ai/put", &json!({"key": "k", "value": "v", "storedAtMs": 1})).await;
        assert!(r.state.stores.ai.used_bytes() > 0);
        let (s, v) = r.authed("/v1/unpair", &json!({})).await;
        assert_eq!((s, &v["wiped"]), (StatusCode::OK, &json!(true)));
        assert_eq!(r.state.stores.ai.used_bytes(), 0);
        assert!(get(&r.state.stores.ai, "k").await.unwrap().is_none());
        assert_eq!(r.state.card().disk.used_mb.ai, 0);
        // A new household can store again.
        put(&r.state.stores.ai, "k2", &json!("v"), 5).await.expect("stores work after unpair");
    }
}
