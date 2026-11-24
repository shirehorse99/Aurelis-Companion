//! `/v1/challenge`, `/v1/pair/start`, `/v1/pair/finish` and `/v1/unpair`.

use crate::auth::{b64, unb64, Refusal, Session};
use crate::json_response;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use std::net::SocketAddr;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use serde_json::{json, Value};
use std::sync::Arc;

fn parse(body: &Bytes) -> Result<Value, Refusal> {
    serde_json::from_slice(body).map_err(|e| Refusal::new(StatusCode::BAD_REQUEST, format!("request body is not JSON: {e}")))
}

fn field(v: &Value, name: &str) -> Result<Vec<u8>, Refusal> {
    v.get(name)
        .and_then(Value::as_str)
        .and_then(unb64)
        .ok_or_else(|| Refusal::new(StatusCode::BAD_REQUEST, format!("'{name}' must be base64 text")))
}

pub async fn challenge(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>) -> Response {
    match state.auth.challenge(peer.map(|ConnectInfo(a)| a.ip().to_canonical())) {
        Ok(n) => json_response(StatusCode::OK, json!({ "nonce": b64(&n) })),
        Err(r) => r.into_response(),
    }
}

pub async fn pair_start(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let result = parse(&body).and_then(|v| field(&v, "msg")).map(|msg| state.auth.start_pairing(msg));
    let result = match result {
        Ok(fut) => fut.await,
        Err(r) => Err(r),
    };
    state.sync_pairing(); // an expired code may have been replaced
    match result {
        Ok((id, msg)) => json_response(StatusCode::OK, json!({ "msg": b64(&msg), "pairingId": id })),
        Err(r) => r.into_response(),
    }
}

pub async fn pair_finish(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let result = parse(&body).and_then(|v| {
        let id = v.get("pairingId").and_then(Value::as_str).ok_or_else(|| Refusal::new(StatusCode::BAD_REQUEST, "'pairingId' must be text"))?.to_string();
        Ok((id, field(&v, "confirm")?, field(&v, "sealedSecret")?))
    });
    let result = match result {
        Ok((id, confirm, sealed)) => state.auth.finish_pairing(&id, &confirm, sealed).await,
        Err(r) => Err(r),
    };
    state.sync_pairing(); // a fifth wrong try makes a new code; a good one clears it
    match result {
        Ok((confirm, fp)) => {
            state.push_job(&format!("paired to household {fp}"));
            json_response(StatusCode::OK, json!({ "confirm": b64(&confirm), "fingerprint": fp }))
        }
        Err(r) => r.into_response(),
    }
}

/// Authenticated. Wipes the household and every store, forgets the secret, mints a new code.
pub async fn unpair(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>) -> Response {
    let result = crate::state::unpair(&state).await;
    match result {
        Ok(()) => {
            state.push_job("unpaired; household data wiped");
            session.sealed_json(StatusCode::OK, &json!({ "wiped": true }))
        }
        Err(r) => session.sealed_reason(r.status, &r.reason),
    }
}

// These are the pairing_flow tests. The crate is a binary, so an integration test under tests/
// could not reach its router; they live here and drive the real router in process.
#[cfg(test)]
mod pairing_flow {
    use super::*;
    use crate::auth::{Clock, CODE_LIFE_MS, NONCE_LIFE_MS};
    use crate::household::{TempDir, FILE_NAME};
    use crate::state::{test_config, test_state_with};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use companion_protocol::pairing::{confirm, Pairing, COMPANION_CONFIRM, TV_CONFIRM};
    use companion_protocol::{proof, seal};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tower::ServiceExt;

    struct Rig {
        dir: TempDir,
        state: Arc<AppState>,
        now: Arc<AtomicU64>,
    }

    fn rig() -> Rig {
        let dir = TempDir::new();
        let now = Arc::new(AtomicU64::new(1_000_000));
        let clock: Clock = {
            let n = now.clone();
            Arc::new(move || n.load(Ordering::SeqCst))
        };
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let state = Arc::new(test_state_with(config, clock));
        Rig { dir, state, now }
    }

    impl Rig {
        fn code(&self) -> String {
            self.state.auth.snapshot().code.expect("an unpaired Companion shows a code")
        }

        fn advance(&self, ms: u64) {
            self.now.fetch_add(ms, Ordering::SeqCst);
        }

        async fn send(&self, req: Request<Body>) -> (StatusCode, Vec<u8>) {
            let res = crate::router(self.state.clone()).oneshot(req).await.expect("infallible");
            let status = res.status();
            (status, to_bytes(res.into_body(), 1 << 20).await.expect("body").to_vec())
        }

        async fn post_json(&self, path: &str, v: Value) -> (StatusCode, Value) {
            let req = Request::builder().method("POST").uri(path).header("x-aurelis-companion", "1").body(Body::from(v.to_string())).unwrap();
            let (s, b) = self.send(req).await;
            (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
        }

        /// Runs the television's side. Returns the final status and body of `finish`.
        async fn tv_pair(&self, code: &str, secret: [u8; 32]) -> (StatusCode, Value, [u8; 32]) {
            let (p, msg) = Pairing::start(code);
            let (s, start) = self.post_json("/v1/pair/start", json!({ "msg": b64(&msg) })).await;
            assert_eq!(s, StatusCode::OK, "{start}");
            let k = p.finish(&unb64(start["msg"].as_str().unwrap()).unwrap()).expect("peer message");
            let (s, fin) = self
                .post_json(
                    "/v1/pair/finish",
                    json!({
                        "pairingId": start["pairingId"],
                        "confirm": b64(&confirm(&k, TV_CONFIRM)),
                        "sealedSecret": b64(&seal::seal(&secret, &k)),
                    }),
                )
                .await;
            (s, fin, k)
        }

        async fn nonce(&self) -> String {
            let req = Request::builder().uri("/v1/challenge").header("x-aurelis-companion", "1").body(Body::empty()).unwrap();
            let (s, b) = self.send(req).await;
            assert_eq!(s, StatusCode::OK);
            serde_json::from_slice::<Value>(&b).unwrap()["nonce"].as_str().unwrap().to_string()
        }

        /// A proved request with a sealed JSON body.
        async fn authed_with(&self, secret: &[u8; 32], nonce: &str, method: &str, path: &str, plain: &Value, proof_secret: &[u8; 32]) -> (StatusCode, Vec<u8>) {
            let sealed = seal::seal(plain.to_string().as_bytes(), secret);
            let p = proof::proof(proof_secret, &unb64(nonce).unwrap(), method, path, &sealed);
            let req = Request::builder()
                .method(method)
                .uri(path)
                .header("x-aurelis-companion", "1")
                .header("x-aurelis-nonce", nonce)
                .header("x-aurelis-proof", b64(&p))
                .body(Body::from(sealed))
                .unwrap();
            self.send(req).await
        }

        async fn authed(&self, secret: &[u8; 32], method: &str, path: &str, plain: &Value) -> (StatusCode, Vec<u8>) {
            let n = self.nonce().await;
            self.authed_with(secret, &n, method, path, plain, secret).await
        }
    }

    const SECRET: [u8; 32] = [42u8; 32];

    #[tokio::test]
    async fn pairing_flow_end_to_end() {
        let r = rig();
        let card: Value = serde_json::from_str(&serde_json::to_string(&r.state.card()).unwrap()).unwrap();
        assert_eq!(card["paired"], false);
        let (s, fin, k) = r.tv_pair(&r.code(), SECRET).await;
        assert_eq!(s, StatusCode::OK, "{fin}");
        assert_eq!(fin["fingerprint"], crate::household::fingerprint(&SECRET));
        assert_eq!(unb64(fin["confirm"].as_str().unwrap()).unwrap(), confirm(&k, COMPANION_CONFIRM).to_vec());
        let card = r.state.card();
        assert!(card.paired);
        assert_eq!(card.household_fingerprint.as_deref(), Some(crate::household::fingerprint(&SECRET).as_str()));
        assert!(r.state.page_view().pairing_code.is_none());
        assert!(r.dir.0.join(FILE_NAME).exists());
        // The secret is kept: a Companion restarted on the same folder is still paired.
        assert!(crate::household::Household::load(&r.dir.0).is_some());
    }

    #[tokio::test]
    async fn loosely_typed_code_pairs() {
        let r = rig();
        let loose = format!("{}-{}", &r.code()[..4].to_lowercase(), &r.code()[4..]);
        assert_eq!(r.tv_pair(&loose, SECRET).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_code_counts_and_fifth_mints_new_code() {
        let r = rig();
        let first = r.code();
        let wrong = if first.starts_with('0') { "1".to_string() + &first[1..] } else { "0".to_string() + &first[1..] };
        for n in 1..5 {
            let (s, fin, _) = r.tv_pair(&wrong, SECRET).await;
            assert_eq!(s, StatusCode::FORBIDDEN);
            assert!(fin["reason"].as_str().unwrap().contains(&format!("{n} of 5")), "{fin}");
            assert_eq!(r.code(), first, "the code stands until the fifth miss");
        }
        let (s, fin, _) = r.tv_pair(&wrong, SECRET).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(fin["reason"].as_str().unwrap().contains("replaced"), "{fin}");
        assert_ne!(r.code(), first);
        assert_ne!(r.state.page_view().pairing_code.as_deref(), Some(first.as_str()));
        assert!(!r.state.card().paired);
        assert_eq!(r.tv_pair(&first, SECRET).await.0, StatusCode::FORBIDDEN, "the old code is dead");
    }

    #[tokio::test]
    async fn code_expires_after_10_minutes() {
        let r = rig();
        let old = r.code();
        r.advance(CODE_LIFE_MS - 1);
        assert_eq!(r.code(), old);
        r.advance(1);
        assert!(r.state.auth.tick(), "the clock tick replaces it");
        r.state.sync_pairing();
        assert_ne!(r.code(), old);
        assert_eq!(r.state.page_view().pairing_code.unwrap(), r.code());
        // Without the tick, a start after expiry also replaces it, so the old code cannot pair.
        let old = r.code();
        r.advance(CODE_LIFE_MS);
        assert_eq!(r.tv_pair(&old, SECRET).await.0, StatusCode::FORBIDDEN);
        assert_ne!(r.code(), old);
    }

    #[tokio::test]
    async fn code_is_single_use() {
        let r = rig();
        let code = r.code();
        assert_eq!(r.tv_pair(&code, SECRET).await.0, StatusCode::OK);
        assert!(r.state.auth.snapshot().code.is_none());
        let (p, msg) = Pairing::start(&code);
        drop(p);
        let (s, v) = r.post_json("/v1/pair/start", json!({ "msg": b64(&msg) })).await;
        assert_eq!(s, StatusCode::CONFLICT, "{v}");
    }

    #[tokio::test]
    async fn already_paired_refuses_pair_start() {
        let r = rig();
        assert_eq!(r.tv_pair(&r.code(), SECRET).await.0, StatusCode::OK);
        let (_, msg) = Pairing::start("00000000");
        let (s, v) = r.post_json("/v1/pair/start", json!({ "msg": b64(&msg) })).await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(v["reason"], format!("already paired to household {} \u{2014} unpair first", crate::household::fingerprint(&SECRET)));
    }

    #[tokio::test]
    async fn pair_requests_with_garbage_say_why() {
        let r = rig();
        for (path, body) in [("/v1/pair/start", json!({})), ("/v1/pair/start", json!({"msg":"!!"})), ("/v1/pair/finish", json!({"pairingId":"nope","confirm":"AA","sealedSecret":"AA"}))] {
            let (s, v) = r.post_json(path, body).await;
            assert!(s.is_client_error(), "{path} {s}");
            assert!(v["reason"].as_str().is_some_and(|x| !x.is_empty()), "{path} {v}");
        }
        let (_, msg) = Pairing::start(&r.code());
        let (s, v) = r.post_json("/v1/pair/start", json!({ "msg": b64(&msg[..5]) })).await;
        assert_eq!((s, v["reason"].as_str()), (StatusCode::BAD_REQUEST, Some("pairing message is not valid")));
    }

    async fn paired() -> Rig {
        let r = rig();
        assert_eq!(r.tv_pair(&r.code(), SECRET).await.0, StatusCode::OK);
        r
    }

    #[tokio::test]
    async fn challenge_while_unpaired_is_401_unpaired() {
        let r = rig();
        let req = Request::builder().uri("/v1/challenge").body(Body::empty()).unwrap();
        let (s, b) = r.send(req).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["reason"], "unpaired");
        let (s, b) = r.authed_with(&SECRET, &b64(&[1u8; 32]), "POST", "/v1/unpair", &json!({}), &SECRET).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["reason"], "unpaired");
    }

    #[tokio::test]
    async fn nonce_is_single_use_and_expires_after_10s() {
        let r = paired().await;
        let plain = json!({});
        // Single use: a replay of the same nonce is stale.
        let n = r.nonce().await;
        // (a GET with an empty body proves the same way)
        let p = proof::proof(&SECRET, &unb64(&n).unwrap(), "GET", "/v1/unpair", b"");
        let get = |n: &str, p: &[u8]| {
            Request::builder().method("GET").uri("/v1/unpair").header("x-aurelis-companion", "1").header("x-aurelis-nonce", n.to_string()).header("x-aurelis-proof", b64(p)).body(Body::empty()).unwrap()
        };
        // GET is not routed for unpair, but auth runs first: 405 proves it passed.
        assert_eq!(r.send(get(&n, &p)).await.0, StatusCode::METHOD_NOT_ALLOWED);
        let (s, b) = r.send(get(&n, &p)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["reason"], "stale-nonce");
        // Never issued.
        let (s, b) = r.authed_with(&SECRET, &b64(&[1u8; 32]), "POST", "/v1/unpair", &plain, &SECRET).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["reason"], "stale-nonce");
        // Life: valid at 9.999 s, stale at 10 s.
        let n = r.nonce().await;
        r.advance(NONCE_LIFE_MS);
        let (s, b) = r.authed_with(&SECRET, &n, "POST", "/v1/unpair", &plain, &SECRET).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["reason"], "stale-nonce");
        assert!(r.state.card().paired, "a refused request changes nothing");
        let n = r.nonce().await;
        r.advance(NONCE_LIFE_MS - 1);
        assert_eq!(r.authed_with(&SECRET, &n, "POST", "/v1/unpair", &plain, &SECRET).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_bad_proof_is_401_and_does_not_spend_the_nonce() {
        let r = paired().await;
        let n = r.nonce().await;
        let (s, b) = r.authed_with(&SECRET, &n, "POST", "/v1/unpair", &json!({}), &[1u8; 32]).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["reason"], "bad-proof");
        assert!(r.state.card().paired);
        // Missing headers: same reasons, and no detail beyond the three.
        let req = Request::builder().method("POST").uri("/v1/unpair").header("x-aurelis-companion", "1").body(Body::empty()).unwrap();
        let (s, b) = r.send(req).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["reason"], "stale-nonce");
        let req = Request::builder().method("POST").uri("/v1/unpair").header("x-aurelis-companion", "1").header("x-aurelis-nonce", n.clone()).body(Body::empty()).unwrap();
        let (_, b) = r.send(req).await;
        assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["reason"], "bad-proof");
        // The good proof on the same nonce still works afterwards.
        assert_eq!(r.authed_with(&SECRET, &n, "POST", "/v1/unpair", &json!({}), &SECRET).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_proof_does_not_cover_a_different_path() {
        let r = paired().await;
        let n = r.nonce().await;
        let sealed = seal::seal(b"{}", &SECRET);
        let p = proof::proof(&SECRET, &unb64(&n).unwrap(), "POST", "/v1/unpair?x=1", &sealed);
        let req = Request::builder().method("POST").uri("/v1/unpair").header("x-aurelis-companion", "1").header("x-aurelis-nonce", n).header("x-aurelis-proof", b64(&p)).body(Body::from(sealed)).unwrap();
        let (s, b) = r.send(req).await;
        assert_eq!((s, serde_json::from_slice::<Value>(&b).unwrap()["reason"].clone()), (StatusCode::UNAUTHORIZED, json!("bad-proof")));
    }

    #[tokio::test]
    async fn authenticated_routes_refuse_a_missing_version_header_with_426() {
        let r = paired().await;
        let req = Request::builder().method("POST").uri("/v1/unpair").body(Body::empty()).unwrap();
        let (s, b) = r.send(req).await;
        assert_eq!(s, StatusCode::UPGRADE_REQUIRED);
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["companionSpeaks"], json!([1]));
        assert!(v["reason"].as_str().is_some());
        // The card and the pair routes keep serving without it.
        let req = Request::builder().uri("/v1/card").body(Body::empty()).unwrap();
        assert_eq!(r.send(req).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_body_that_does_not_open_is_400_and_a_sealed_reply_opens() {
        let r = paired().await;
        // Sealed under another key, though the proof is right: the proof covers the bytes sent.
        let n = r.nonce().await;
        let bad = seal::seal(b"{}", &[9u8; 32]);
        let p = proof::proof(&SECRET, &unb64(&n).unwrap(), "POST", "/v1/unpair", &bad);
        let req = Request::builder().method("POST").uri("/v1/unpair").header("x-aurelis-companion", "1").header("x-aurelis-nonce", n).header("x-aurelis-proof", b64(&p)).body(Body::from(bad)).unwrap();
        let (s, b) = r.send(req).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["reason"], "body could not be opened");
        assert!(r.state.card().paired);
        // A good unpair replies sealed JSON.
        let (s, b) = r.authed(&SECRET, "POST", "/v1/unpair", &json!({})).await;
        assert_eq!(s, StatusCode::OK);
        let plain = seal::open(&b, &SECRET).expect("reply is v3-sealed");
        assert_eq!(serde_json::from_slice::<Value>(&plain).unwrap(), json!({"wiped": true}));
    }

    #[tokio::test]
    async fn handlers_receive_the_opened_body_and_can_reply_sealed() {
        use axum::routing::post;
        let r = paired().await;
        async fn echo(Extension(s): Extension<Session>, body: Bytes) -> Response {
            s.sealed_json(StatusCode::OK, &json!({ "echo": String::from_utf8_lossy(&body) }))
        }
        let app = axum::Router::new()
            .route("/v1/echo", post(echo))
            .layer(axum::middleware::from_fn_with_state(r.state.clone(), crate::auth::require_household));
        let n = r.nonce().await;
        let sealed = seal::seal(br#"{"k":1}"#, &SECRET);
        let p = proof::proof(&SECRET, &unb64(&n).unwrap(), "POST", "/v1/echo?a=b", &sealed);
        let req = Request::builder().method("POST").uri("/v1/echo?a=b").header("x-aurelis-companion", "1").header("x-aurelis-nonce", n).header("x-aurelis-proof", b64(&p)).body(Body::from(sealed)).unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.headers()["content-type"], "application/octet-stream");
        let b = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let v: Value = serde_json::from_slice(&seal::open(&b, &SECRET).unwrap()).unwrap();
        assert_eq!(v["echo"], r#"{"k":1}"#);
    }

    fn peer(n: u32) -> Option<std::net::IpAddr> {
        Some(std::net::IpAddr::V4(std::net::Ipv4Addr::from(0xC0A8_0000 + n)))
    }

    #[tokio::test]
    async fn a_challenge_flood_cannot_evict_a_young_nonce() {
        let r = paired().await;
        let tv = r.nonce().await; // the television's, from "no peer"
        for n in 0..(crate::auth::NONCE_CAP as u32 - 1) {
            r.state.auth.challenge(peer(n)).unwrap();
        }
        // Full of nonces younger than 2 s: refused, nothing evicted.
        let e = r.state.auth.challenge(peer(9999)).unwrap_err();
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.reason, "too many outstanding challenges, try again shortly");
        assert_eq!(r.authed_with(&SECRET, &tv, "POST", "/v1/unpair", &json!({}), &SECRET).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_full_store_makes_room_only_from_nonces_older_than_2s() {
        let r = paired().await;
        let first = r.nonce().await;
        for n in 0..(crate::auth::NONCE_CAP as u32 - 1) {
            r.state.auth.challenge(peer(n)).unwrap();
        }
        r.advance(crate::auth::NONCE_PROTECTED_MS);
        assert!(r.state.auth.challenge(peer(9999)).is_ok(), "the oldest is old enough to go");
        let (s, b) = r.authed_with(&SECRET, &first, "POST", "/v1/unpair", &json!({}), &SECRET).await;
        assert_eq!((s, serde_json::from_slice::<Value>(&b).unwrap()["reason"].clone()), (StatusCode::UNAUTHORIZED, json!("stale-nonce")));
    }

    #[tokio::test]
    async fn one_peer_may_hold_only_32_nonces() {
        let r = paired().await;
        for _ in 0..crate::auth::NONCE_PER_PEER {
            r.state.auth.challenge(peer(1)).unwrap();
        }
        assert_eq!(r.state.auth.challenge(peer(1)).unwrap_err().status, StatusCode::TOO_MANY_REQUESTS);
        assert!(r.state.auth.challenge(peer(2)).is_ok(), "another television is not affected");
        r.advance(NONCE_LIFE_MS);
        assert!(r.state.auth.challenge(peer(1)).is_ok(), "they expire");
    }

    #[tokio::test]
    async fn a_second_finish_on_the_same_pairing_id_is_refused() {
        let r = rig();
        let (p, msg) = Pairing::start(&r.code());
        let (_, start) = r.post_json("/v1/pair/start", json!({ "msg": b64(&msg) })).await;
        let k = p.finish(&unb64(start["msg"].as_str().unwrap()).unwrap()).unwrap();
        let body = json!({"pairingId": start["pairingId"], "confirm": b64(&confirm(&k, TV_CONFIRM)), "sealedSecret": b64(&seal::seal(&SECRET, &k))});
        assert_eq!(r.post_json("/v1/pair/finish", body.clone()).await.0, StatusCode::OK);
        let (s, v) = r.post_json("/v1/pair/finish", body).await;
        assert_eq!(s, StatusCode::CONFLICT, "{v}"); // paired now
        // And on an unpaired Companion the id is gone after one use, right or wrong.
        let r = rig();
        let (p, msg) = Pairing::start(&r.code());
        let (_, start) = r.post_json("/v1/pair/start", json!({ "msg": b64(&msg) })).await;
        let k = p.finish(&unb64(start["msg"].as_str().unwrap()).unwrap()).unwrap();
        let wrong = json!({"pairingId": start["pairingId"], "confirm": b64(&[0u8; 32]), "sealedSecret": b64(&seal::seal(&SECRET, &k))});
        assert_eq!(r.post_json("/v1/pair/finish", wrong).await.0, StatusCode::FORBIDDEN);
        let right = json!({"pairingId": start["pairingId"], "confirm": b64(&confirm(&k, TV_CONFIRM)), "sealedSecret": b64(&seal::seal(&SECRET, &k))});
        assert_eq!(r.post_json("/v1/pair/finish", right).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_right_confirm_with_an_unopenable_secret_does_not_pair_and_counts() {
        let r = rig();
        let (p, msg) = Pairing::start(&r.code());
        let (_, start) = r.post_json("/v1/pair/start", json!({ "msg": b64(&msg) })).await;
        let k = p.finish(&unb64(start["msg"].as_str().unwrap()).unwrap()).unwrap();
        let body = json!({"pairingId": start["pairingId"], "confirm": b64(&confirm(&k, TV_CONFIRM)), "sealedSecret": b64(&seal::seal(&SECRET, &[1u8; 32]))});
        let (s, v) = r.post_json("/v1/pair/finish", body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        let why = v["reason"].as_str().unwrap();
        assert!(why.contains("could not be opened") && why.contains("1 of 5"), "{why}");
        assert!(!r.state.card().paired && !r.dir.0.join(FILE_NAME).exists());
    }

    #[tokio::test]
    async fn a_wrong_length_confirm_counts_as_a_wrong_try() {
        let r = rig();
        let (p, msg) = Pairing::start(&r.code());
        let (_, start) = r.post_json("/v1/pair/start", json!({ "msg": b64(&msg) })).await;
        let k = p.finish(&unb64(start["msg"].as_str().unwrap()).unwrap()).unwrap();
        let body = json!({"pairingId": start["pairingId"], "confirm": b64(&[1u8; 7]), "sealedSecret": b64(&seal::seal(&SECRET, &k))});
        let (s, v) = r.post_json("/v1/pair/finish", body).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(v["reason"].as_str().unwrap().contains("1 of 5"), "{v}");
    }

    #[tokio::test]
    async fn pending_pairings_are_bounded_and_the_oldest_goes_first() {
        let r = rig();
        let code = r.code();
        let mut started = Vec::new();
        for _ in 0..crate::auth::PENDING_CAP + 1 {
            let (p, msg) = Pairing::start(&code);
            let (s, start) = r.post_json("/v1/pair/start", json!({ "msg": b64(&msg) })).await;
            assert_eq!(s, StatusCode::OK);
            started.push((p.finish(&unb64(start["msg"].as_str().unwrap()).unwrap()).unwrap(), start["pairingId"].clone()));
        }
        let finish = |k: [u8; 32], id: Value| json!({"pairingId": id, "confirm": b64(&confirm(&k, TV_CONFIRM)), "sealedSecret": b64(&seal::seal(&SECRET, &k))});
        let (k, id) = started[0].clone();
        assert_eq!(r.post_json("/v1/pair/finish", finish(k, id)).await.0, StatusCode::NOT_FOUND, "evicted");
        let (k, id) = started[started.len() - 1].clone();
        assert_eq!(r.post_json("/v1/pair/finish", finish(k, id)).await.0, StatusCode::OK);
    }

    async fn until_idle(r: &Rig) {
        for _ in 0..3000 {
            if !r.state.auth.is_busy() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the Companion stayed busy after its work should have finished");
    }

    /// Polls a future exactly once, then drops it, as hyper does when the client hangs up.
    ///
    /// Not `timeout(Duration::ZERO, f)`: a zero timer is rounded up to the next millisecond tick,
    /// so on a loaded machine the test task yielded, the spawned work ran to the end, and the
    /// timeout's next poll found the future finished ("it finished before it could be dropped").
    /// One manual poll cannot yield, so the spawned work has not started when the future goes.
    async fn drop_mid_await<F: std::future::Future>(f: F) {
        let mut f = std::pin::pin!(f);
        let first = std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(f.as_mut(), cx))).await;
        assert!(first.is_pending(), "it finished before it could be dropped");
    }

    #[tokio::test]
    async fn a_finish_dropped_mid_await_still_completes_and_clears_busy() {
        let r = rig();
        let (p, msg) = Pairing::start(&r.code());
        let (_, start) = r.post_json("/v1/pair/start", json!({ "msg": b64(&msg) })).await;
        let k = p.finish(&unb64(start["msg"].as_str().unwrap()).unwrap()).unwrap();
        let id = start["pairingId"].as_str().unwrap().to_string();
        drop_mid_await(r.state.auth.finish_pairing(&id, &confirm(&k, TV_CONFIRM), seal::seal(&SECRET, &k))).await;
        until_idle(&r).await;
        let v = r.state.auth.snapshot();
        assert!(v.paired && v.code.is_none());
        assert_eq!(v.fingerprint.as_deref(), Some(crate::household::fingerprint(&SECRET).as_str()));
        assert!(r.dir.0.join(FILE_NAME).exists());
    }

    #[tokio::test]
    async fn an_unpair_dropped_mid_await_still_completes_and_clears_busy() {
        let r = paired().await;
        std::fs::create_dir_all(r.dir.0.join("ai")).unwrap();
        drop_mid_await(r.state.auth.unpair()).await;
        until_idle(&r).await;
        let v = r.state.auth.snapshot();
        assert!(!v.paired && v.code.is_some(), "unpaired with a fresh code");
        assert!(!r.dir.0.join(FILE_NAME).exists() && !r.dir.0.join("ai").exists());
        r.state.sync_pairing();
        assert_eq!(r.tv_pair(&r.code(), [5u8; 32]).await.0, StatusCode::OK, "pairing works again");
    }

    fn with_peer(mut req: Request<Body>, ip: &str) -> Request<Body> {
        let addr: SocketAddr = format!("{ip}:50000").parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    fn page(r: &Rig, ip: &str, ingress: bool) -> Request<Body> {
        let mut b = Request::builder().uri("/").header("host", "nuc:8765");
        if ingress {
            b = b.header("x-ingress-path", "/api/hassio_ingress/abc");
        }
        let _ = r;
        with_peer(b.body(Body::empty()).unwrap(), ip)
    }

    #[tokio::test]
    async fn the_status_page_shows_the_code_only_to_the_operator() {
        let r = rig();
        let code = r.code();
        let html = |(_, b): (StatusCode, Vec<u8>)| String::from_utf8(b).unwrap();
        assert!(html(r.send(page(&r, "127.0.0.1", false)).await).contains(&code));
        assert!(html(r.send(page(&r, "172.30.32.2", true)).await).contains(&code));
        for (ip, ingress) in [("172.30.32.2", false), ("192.168.3.50", true), ("192.168.3.50", false)] {
            let h = html(r.send(page(&r, ip, ingress)).await);
            assert!(!h.contains(&code) && h.contains("Pairing code hidden on the network"), "{ip} {ingress}");
        }
        // No peer information at all: hidden too.
        let bare = Request::builder().uri("/").body(Body::empty()).unwrap();
        assert!(!html(r.send(bare).await).contains(&code));
    }

    #[tokio::test]
    async fn operator_unpair_from_the_status_page_wipes_and_mints_a_new_code() {
        let r = paired().await;
        std::fs::create_dir_all(r.dir.0.join("guide")).unwrap();
        let post = |ip: &str, ingress: bool| {
            let mut b = Request::builder().method("POST").uri("/ui/unpair").header("host", "nuc:8765");
            if ingress {
                b = b.header("x-ingress-path", "/x");
            }
            with_peer(b.body(Body::empty()).unwrap(), ip)
        };
        assert_eq!(r.send(post("192.168.3.50", true)).await.0, StatusCode::FORBIDDEN);
        assert_eq!(r.send(post("172.30.32.2", false)).await.0, StatusCode::FORBIDDEN);
        assert!(r.state.card().paired);
        let req = with_peer(Request::builder().method("POST").uri("/ui/unpair").header("host", "nuc:8765").header("origin", "http://evil.example").body(Body::empty()).unwrap(), "127.0.0.1");
        assert_eq!(r.send(req).await.0, StatusCode::FORBIDDEN, "same-origin check still applies");
        assert!(r.state.card().paired);
        assert_eq!(r.send(post("172.30.32.2", true)).await.0, StatusCode::SEE_OTHER);
        assert!(!r.state.card().paired);
        assert!(!r.dir.0.join(FILE_NAME).exists() && !r.dir.0.join("guide").exists());
        assert!(r.code_or_none().is_some());
        assert_eq!(r.state.page_view().pairing_code, r.code_or_none());
    }

    #[tokio::test]
    async fn unpair_wipes_secret_and_all_stores() {
        let r = paired().await;
        for d in ["ai", "guide", "picture", "ai_jobs"] {
            std::fs::create_dir_all(r.dir.0.join(d)).unwrap();
            std::fs::write(r.dir.0.join(d).join("x.bin"), b"1").unwrap();
        }
        let before = r.code_or_none();
        assert!(before.is_none());
        let (s, b) = r.authed(&SECRET, "POST", "/v1/unpair", &json!({})).await;
        assert_eq!(s, StatusCode::OK);
        assert!(seal::open(&b, &SECRET).is_some());
        for d in ["ai", "guide", "picture", "ai_jobs"] {
            assert!(!r.dir.0.join(d).exists(), "{d}");
        }
        assert!(!r.dir.0.join(FILE_NAME).exists());
        assert!(crate::household::Household::load(&r.dir.0).is_none());
        assert!(!r.state.card().paired && r.state.card().household_fingerprint.is_none());
        let code = r.code_or_none().expect("a new code is on the page");
        assert_eq!(r.state.page_view().pairing_code.as_deref(), Some(code.as_str()));
        // The old secret no longer opens anything; a new pairing works.
        let (s, _) = r.authed_with(&SECRET, &b64(&[1u8; 32]), "POST", "/v1/unpair", &json!({}), &SECRET).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(r.tv_pair(&code, [5u8; 32]).await.0, StatusCode::OK);
    }

    impl Rig {
        fn code_or_none(&self) -> Option<String> {
            self.state.auth.snapshot().code
        }
    }
}
