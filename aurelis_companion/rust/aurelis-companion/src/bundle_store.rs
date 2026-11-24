//! `PUT/GET /v1/household/bundle`: the master television's sealed household bundle, held for the
//! followers to read when the master is silent.
//!
//! The Companion treats the bundle as opaque. The television seals it under the household secret
//! (`HouseholdCipher`, v3) before sending; the transport seal of the authenticated route is peeled
//! by the middleware, and what reaches this module is still sealed. It is never opened here and no
//! byte of it is ever logged. Each accepted put replaces the held copy whole, so a key or login the
//! master no longer holds is gone with it, and an older (or equal) `capturedAtMs` is refused so a
//! late push from a restarted master cannot roll removals back.

use crate::auth::Session;
use crate::guide_store::iso_utc;
use crate::state::{now_ms, AppState};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const DIR_NAME: &str = "household";
pub const SEALED_FILE: &str = "bundle.sealed";
pub const META_FILE: &str = "bundle.meta.json";
/// The largest bundle kept. A real one is kilobytes; this is a guard, not a budget.
pub const MAX_BUNDLE_BYTES: usize = 2 * 1024 * 1024;
/// What the authenticated middleware may read for this route: the bundle plus its transport seal.
pub const MAX_REQUEST_BYTES: usize = MAX_BUNDLE_BYTES + 1024;
pub const PATH: &str = "/v1/household/bundle";
pub const OWNERSHIP_PATH: &str = "/v1/household/settings";
/// How far ahead of this Companion's own clock a capture stamp may be before it is refused. Two
/// televisions' clocks drift by seconds; a stamp an hour ahead would out-rank every honest push.
pub const MAX_CLOCK_LEAD_MS: u64 = 10 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleMeta {
    pub captured_at_ms: u64,
    pub stored_at_ms: u64,
    pub bytes: u64,
    #[serde(default)]
    pub companion_owned: bool,
    #[serde(default)]
    revision_file: bool,
}

impl BundleMeta {
    fn payload_name(self) -> String {
        if self.revision_file {
            format!("bundle-{}.sealed", self.captured_at_ms)
        } else {
            SEALED_FILE.to_string()
        }
    }
}

#[derive(Clone, Copy)]
struct OwnershipWrite {
    expected_revision: u64,
    companion_owned: bool,
}

#[derive(Debug, PartialEq)]
pub struct BundleRefusal {
    pub status: StatusCode,
    pub reason: String,
}

fn refuse<T>(status: StatusCode, reason: impl Into<String>) -> Result<T, BundleRefusal> {
    Err(BundleRefusal { status, reason: reason.into() })
}

pub struct BundleStore {
    dir: PathBuf,
    /// What is held, mirrored in memory so the card can say how old it is without touching disk.
    held: Mutex<Option<BundleMeta>>,
    /// Serialises puts and the unpair reset.
    gate: tokio::sync::Mutex<()>,
}

pub(crate) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = dir.join(format!("{name}.tmp"));
    let path = dir.join(name);
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let written = opts.open(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", tmp.display()));
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot move {} into place: {e}", path.display())
    })
}

impl BundleStore {
    /// Reads what is held, if anything. A meta file that disagrees with the sealed file (a torn
    /// write) is reported on stderr and read as nothing held, so the master simply pushes again.
    pub fn open(data_dir: &Path) -> BundleStore {
        let dir = data_dir.join(DIR_NAME);
        let held = match std::fs::read(dir.join(META_FILE)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                eprintln!("household bundle: cannot read {}: {e}; reading it as none held", dir.join(META_FILE).display());
                None
            }
            Ok(raw) => match serde_json::from_slice::<BundleMeta>(&raw) {
                Err(e) => {
                    eprintln!("household bundle: the meta file is damaged ({e}); reading it as none held");
                    None
                }
                Ok(meta) => match std::fs::metadata(dir.join(meta.payload_name())) {
                    Ok(m) if m.len() == meta.bytes => Some(meta),
                    Ok(m) => {
                        eprintln!("household bundle: the sealed file is {} bytes but its meta says {}; reading it as none held", m.len(), meta.bytes);
                        None
                    }
                    Err(e) => {
                        eprintln!("household bundle: the meta file has no sealed file beside it ({e}); reading it as none held");
                        None
                    }
                },
            },
        };
        BundleStore { dir, held: Mutex::new(held), gate: tokio::sync::Mutex::new(()) }
    }

    fn held_lock(&self) -> std::sync::MutexGuard<'_, Option<BundleMeta>> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// When the held copy was captured on the master, for the card. `None` when nothing is held.
    pub fn captured_at_ms(&self) -> Option<u64> {
        self.held_lock().map(|m| m.captured_at_ms)
    }

    /// Replaces the held copy. `still_paired` is asked under the lock, so a put cannot land after
    /// an unpair has taken the household away. `check_floor` is the host's disk floor.
    async fn put(
        &self,
        sealed: Vec<u8>,
        captured_at_ms: u64,
        ownership: Option<OwnershipWrite>,
        still_paired: impl Fn() -> bool,
        check_floor: impl std::future::Future<Output = Result<(), String>>,
    ) -> Result<BundleMeta, BundleRefusal> {
        if captured_at_ms == 0 {
            return refuse(StatusCode::BAD_REQUEST, "'capturedAtMs' must be the master's capture time in milliseconds (a positive whole number)");
        }
        if sealed.is_empty() {
            return refuse(StatusCode::BAD_REQUEST, "the bundle is empty");
        }
        if sealed.len() > MAX_BUNDLE_BYTES {
            return refuse(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("the bundle is {} bytes and the largest kept is {MAX_BUNDLE_BYTES} (2 MiB)", sealed.len()),
            );
        }
        let ahead = captured_at_ms.saturating_sub(now_ms().max(0) as u64);
        if ahead > MAX_CLOCK_LEAD_MS {
            return refuse(
                StatusCode::BAD_REQUEST,
                format!("captured in the future by {} min — check this television's clock", ahead / 60_000),
            );
        }
        let _gate = self.gate.lock().await;
        if !still_paired() {
            return refuse(StatusCode::CONFLICT, "this Companion is no longer paired; the bundle was not kept");
        }
        let held = *self.held_lock();
        if let Some(write) = ownership {
            if held.map(|meta| meta.captured_at_ms) != Some(write.expected_revision) {
                return refuse(StatusCode::CONFLICT, "household settings revision changed; reload before saving");
            }
        } else if held.is_some_and(|meta| meta.companion_owned) {
            return refuse(StatusCode::CONFLICT, "Companion owns household settings; a revision is required");
        }
        if let Some(held) = held {
            if captured_at_ms <= held.captured_at_ms {
                return refuse(
                    StatusCode::CONFLICT,
                    format!("older than the bundle already held (captured {})", iso_utc(held.captured_at_ms as i64)),
                );
            }
        }
        if let Err(why) = check_floor.await {
            return refuse(StatusCode::INSUFFICIENT_STORAGE, why);
        }
        let meta = BundleMeta {
            captured_at_ms,
            stored_at_ms: now_ms().max(0) as u64,
            bytes: sealed.len() as u64,
            companion_owned: ownership.is_some_and(|write| write.companion_owned),
            revision_file: true,
        };
        let dir = self.dir.clone();
        let written = tokio::task::spawn_blocking(move || -> Result<(), String> {
            std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            // Publish the new payload before replacing the pointer to the committed revision.
            let payload = meta.payload_name();
            write_atomic(&dir, &payload, &sealed)?;
            let body = serde_json::to_vec(&meta).map_err(|e| format!("cannot encode the meta file: {e}"))?;
            if let Err(error) = write_atomic(&dir, META_FILE, &body) {
                let _ = std::fs::remove_file(dir.join(payload));
                return Err(error);
            }
            if let Some(previous) = held {
                let _ = std::fs::remove_file(dir.join(previous.payload_name()));
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|e| Err(format!("the write task failed: {e}")));
        match written {
            Ok(()) => {
                *self.held_lock() = Some(meta);
                Ok(meta)
            }
            Err(why) => {
                eprintln!("household bundle: {why}");
                refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("the bundle could not be kept: {why}"))
            }
        }
    }

    /// The held bytes and their stamps, or `None` when nothing is held.
    pub async fn get(&self) -> Result<Option<(Vec<u8>, BundleMeta)>, String> {
        let _gate = self.gate.lock().await;
        let Some(meta) = *self.held_lock() else { return Ok(None) };
        let path = self.dir.join(meta.payload_name());
        match tokio::fs::read(&path).await {
            Ok(b) if b.len() as u64 == meta.bytes => Ok(Some((b, meta))),
            Ok(b) => Err(format!("the held bundle is {} bytes but was stored as {}", b.len(), meta.bytes)),
            Err(e) => Err(format!("the held bundle could not be read: {e}")),
        }
    }

    /// Unpair: forget it and delete the folder (the household wipe deletes it too; this makes the
    /// running Companion agree and catches a put that was already in flight).
    pub async fn reset(&self) {
        let _gate = self.gate.lock().await;
        *self.held_lock() = None;
        match tokio::fs::remove_dir_all(&self.dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("unpair: cannot delete {}: {e}", self.dir.display()),
        }
    }
}

fn refusal_response(session: &Session, r: BundleRefusal) -> Response {
    session.sealed_reason(r.status, &r.reason)
}

pub async fn put_route(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<Session>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let param = |name: &str| {
        uri.query()?.split('&').filter_map(|kv| kv.split_once('='))
            .find_map(|(key, value)| (key == name).then_some(value))
    };
    let captured = param("capturedAtMs").and_then(|v| v.parse::<u64>().ok());
    let Some(captured) = captured else {
        return session.sealed_reason(StatusCode::BAD_REQUEST, "'capturedAtMs' must be given in the query as whole milliseconds");
    };
    let ownership = match (uri.path(), param("expectedRevision"), param("owner")) {
        (PATH, None, None) => None,
        (OWNERSHIP_PATH, Some(revision), Some(owner @ ("companion" | "box"))) => {
            let Ok(expected_revision) = revision.parse::<u64>() else {
                return session.sealed_reason(StatusCode::BAD_REQUEST, "'expectedRevision' must be a whole number");
            };
            Some(OwnershipWrite { expected_revision, companion_owned: owner == "companion" })
        }
        _ => return session.sealed_reason(StatusCode::BAD_REQUEST, "provide 'expectedRevision' and an owner of 'companion' or 'box'"),
    };
    let floor_state = state.clone();
    let paired_state = state.clone();
    let size = body.len() as u64;
    let result = state
        .bundle
        .put(
            body.to_vec(),
            captured,
            ownership,
            move || paired_state.auth.snapshot().paired,
            async move { floor_state.stores.budget.check_floor(size).await.map_err(|r| r.reason) },
        )
        .await;
    match result {
        Ok(meta) => {
            state.push_job(&format!("household bundle held, captured {}", iso_utc(meta.captured_at_ms as i64)));
            session.sealed_json(StatusCode::OK, &serde_json::json!({ "held": true, "capturedAtMs": meta.captured_at_ms, "storedAtMs": meta.stored_at_ms, "owner": if meta.companion_owned { "companion" } else { "box" } }))
        }
        Err(r) => {
            if r.status != StatusCode::CONFLICT {
                eprintln!("household bundle refused: {}", r.reason);
            }
            refusal_response(&session, r)
        }
    }
}

pub async fn get_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>) -> Response {
    match state.bundle.get().await {
        Ok(Some((bytes, meta))) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::HeaderName::from_static("x-captured-at-ms"), meta.captured_at_ms.to_string()),
                (header::HeaderName::from_static("x-stored-at-ms"), meta.stored_at_ms.to_string()),
                (header::HeaderName::from_static("x-household-owner"), if meta.companion_owned { "companion" } else { "box" }.to_string()),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => session.sealed_reason(StatusCode::NOT_FOUND, "no bundle from the master yet"),
        Err(why) => {
            eprintln!("household bundle: {why}");
            session.sealed_reason(StatusCode::INTERNAL_SERVER_ERROR, &why)
        }
    }
}

/// Seal the ownership and revision together with the opaque bundle so a reader can trust them.
pub async fn settings_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>) -> Response {
    match state.bundle.get().await {
        Ok(Some((bytes, meta))) => session.sealed_json(StatusCode::OK, &serde_json::json!({
            "sealed": crate::auth::b64(&bytes),
            "capturedAtMs": meta.captured_at_ms,
            "storedAtMs": meta.stored_at_ms,
            "owner": if meta.companion_owned { "companion" } else { "box" },
        })),
        Ok(None) => session.sealed_reason(StatusCode::NOT_FOUND, "no household settings are held"),
        Err(_) => session.sealed_reason(StatusCode::INTERNAL_SERVER_ERROR, "the held household settings could not be read"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{b64, unb64};
    use crate::household::{Household, TempDir};
    use crate::state::{test_config, test_state};
    use axum::body::{to_bytes, Body};
    use axum::http::{HeaderMap, Request};
    use companion_protocol::{proof, seal};
    use tower::ServiceExt;

    const SECRET: [u8; 32] = [9u8; 32];

    struct Rig {
        dir: TempDir,
        state: Arc<AppState>,
    }

    fn rig() -> Rig {
        let dir = TempDir::new();
        Household::create(&dir.0, SECRET).expect("household");
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        Rig { state: Arc::new(test_state(config)), dir }
    }

    impl Rig {
        async fn send(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
            let res = crate::router(self.state.clone()).oneshot(req).await.expect("infallible");
            let (status, headers) = (res.status(), res.headers().clone());
            (status, headers, to_bytes(res.into_body(), 8 << 20).await.expect("body").to_vec())
        }

        /// A signed request; `plain` is sealed for transport exactly as the television does.
        async fn authed(&self, method: &str, path: &str, plain: &[u8]) -> (StatusCode, HeaderMap, Vec<u8>) {
            let req = Request::builder().uri("/v1/challenge").header("x-aurelis-companion", "1").body(Body::empty()).unwrap();
            let (s, _, b) = self.send(req).await;
            assert_eq!(s, StatusCode::OK);
            let nonce = serde_json::from_slice::<serde_json::Value>(&b).unwrap()["nonce"].as_str().unwrap().to_string();
            let sealed = if plain.is_empty() { Vec::new() } else { seal::seal(plain, &SECRET) };
            let p = proof::proof(&SECRET, &unb64(&nonce).unwrap(), method, path, &sealed);
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

        async fn put(&self, bundle: &[u8], captured: u64) -> (StatusCode, serde_json::Value) {
            let (s, _, b) = self.authed("PUT", &format!("{PATH}?capturedAtMs={captured}"), bundle).await;
            let opened = seal::open(&b, &SECRET).unwrap_or(b);
            (s, serde_json::from_slice(&opened).unwrap_or(serde_json::Value::Null))
        }
    }

    /// Stands in for `HouseholdCipher.sealText(payload, secret)`: any bytes the Companion must not read.
    fn bundle_bytes(tag: u8) -> Vec<u8> {
        seal::seal(format!("{{\"secret-ish\":\"{tag}\",\"apiKey\":\"sk-must-never-be-read\"}}").as_bytes(), &SECRET)
    }

    #[tokio::test]
    async fn ownership_transfer_fences_legacy_and_stale_writes_until_handback() {
        let r = rig();
        let bundle = bundle_bytes(1);
        assert_eq!(r.put(&bundle, 10).await.0, StatusCode::OK);
        let takeover = format!("{OWNERSHIP_PATH}?capturedAtMs=11&expectedRevision=10&owner=companion");
        assert_eq!(r.authed("PUT", &takeover, &bundle).await.0, StatusCode::OK);
        let (_, headers, _) = r.authed("GET", PATH, &[]).await;
        assert_eq!(headers["x-household-owner"], "companion");
        let (status, _, sealed_reply) = r.authed("GET", OWNERSHIP_PATH, &[]).await;
        assert_eq!(status, StatusCode::OK);
        let reply: serde_json::Value = serde_json::from_slice(&seal::open(&sealed_reply, &SECRET).unwrap()).unwrap();
        assert_eq!(reply["owner"], "companion");
        assert_eq!(reply["capturedAtMs"], 11);
        assert_eq!(reply["sealed"], b64(&bundle));
        assert_eq!(r.put(&bundle, 12).await.0, StatusCode::CONFLICT);

        let handback = format!("{OWNERSHIP_PATH}?capturedAtMs=12&expectedRevision=10&owner=box");
        assert_eq!(r.authed("PUT", &handback, &bundle).await.0, StatusCode::CONFLICT);
        let handback = format!("{OWNERSHIP_PATH}?capturedAtMs=12&expectedRevision=11&owner=box");
        assert_eq!(r.authed("PUT", &handback, &bundle).await.0, StatusCode::OK);
        assert_eq!(r.put(&bundle, 13).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn bundle_put_get_round_trip_is_opaque() {
        let r = rig();
        let bundle = bundle_bytes(1);
        let (s, v) = r.put(&bundle, 1_700_000_000_000).await;
        assert_eq!((s, &v["held"]), (StatusCode::OK, &serde_json::json!(true)), "{v}");
        // On disk: exactly the bytes sent, never opened, plus a meta file naming them.
        assert_eq!(std::fs::read(r.dir.0.join("household/bundle-1700000000000.sealed")).unwrap(), bundle);
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(r.dir.0.join("household/bundle.meta.json")).unwrap()).unwrap();
        assert_eq!((meta["capturedAtMs"].as_u64(), meta["bytes"].as_u64()), (Some(1_700_000_000_000), Some(bundle.len() as u64)));
        assert!(meta["storedAtMs"].as_u64().unwrap() > 0);
        let (s, headers, body) = r.authed("GET", PATH, b"").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body, bundle, "returned verbatim");
        assert_eq!(headers["x-captured-at-ms"], "1700000000000");
        assert_eq!(headers["x-stored-at-ms"], meta["storedAtMs"].as_u64().unwrap().to_string().as_str());
        // A newer put replaces it whole.
        let newer = bundle_bytes(2);
        assert_eq!(r.put(&newer, 1_700_000_000_001).await.0, StatusCode::OK);
        assert_eq!(r.authed("GET", PATH, b"").await.2, newer);
        // None held reads as an honest 404.
        let fresh = rig();
        let (s, _, b) = fresh.authed("GET", PATH, b"").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let reason: serde_json::Value = serde_json::from_slice(&seal::open(&b, &SECRET).unwrap()).unwrap();
        assert_eq!(reason["reason"], "no bundle from the master yet");
    }

    #[tokio::test]
    async fn older_bundle_is_refused_with_reason() {
        let r = rig();
        let base = (now_ms() as u64) - 3_600_000;
        let first = bundle_bytes(1);
        assert_eq!(r.put(&first, base).await.0, StatusCode::OK);
        for at in [base - 1, base] {
            let (s, v) = r.put(&bundle_bytes(2), at).await;
            assert_eq!(s, StatusCode::CONFLICT, "{at}");
            let reason = v["reason"].as_str().unwrap();
            let want = format!("older than the bundle already held (captured {})", iso_utc(base as i64));
            assert!(reason.starts_with(&want), "{reason}");
        }
        assert_eq!(r.authed("GET", PATH, b"").await.2, first, "the held copy is untouched");
        let (s, v) = r.put(&bundle_bytes(3), 0).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    }

    #[tokio::test]
    async fn a_bundle_captured_in_the_future_is_refused_with_reason() {
        let r = rig();
        let now = now_ms() as u64;
        // A television whose clock runs an hour fast cannot park a stamp nothing honest can beat.
        let (s, v) = r.put(&bundle_bytes(1), now + 60 * 60_000).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        let reason = v["reason"].as_str().unwrap();
        assert!(reason.starts_with("captured in the future by ") && reason.contains("check this television's clock"), "{reason}");
        assert!(reason.contains("min"), "{reason}");
        assert!(r.state.bundle.captured_at_ms().is_none(), "nothing was kept");
        // A few minutes of drift between two clocks is ordinary and is kept.
        assert_eq!(r.put(&bundle_bytes(2), now + 5 * 60_000).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn oversize_bundle_is_413() {
        let r = rig();
        let (s, v) = r.put(&vec![7u8; MAX_BUNDLE_BYTES + 1], 5).await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(v["reason"].as_str().unwrap().contains("2 MiB"), "{v}");
        assert!(r.state.bundle.captured_at_ms().is_none());
        // Exactly at the limit is kept.
        assert_eq!(r.put(&vec![7u8; MAX_BUNDLE_BYTES], 6).await.0, StatusCode::OK);
        // Far over is stopped by the transport's own cap, still a 413 with a reason.
        let (s, _, b) = r.authed("PUT", &format!("{PATH}?capturedAtMs=7"), &vec![7u8; MAX_BUNDLE_BYTES + 4096]).await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(String::from_utf8_lossy(&b).contains("reason"));
    }

    #[tokio::test]
    async fn unpair_wipes_household_bundle() {
        let r = rig();
        assert_eq!(r.put(&bundle_bytes(1), 10).await.0, StatusCode::OK);
        assert!(r.dir.0.join("household").is_dir());
        crate::state::unpair(&r.state).await.expect("unpaired");
        assert!(!r.dir.0.join("household").exists(), "folder gone");
        assert!(r.state.bundle.captured_at_ms().is_none());
        assert_eq!(r.state.card().household_bundle_at_ms, None);
        // The folder is on the household wipe list too (a Companion that restarts mid-unpair).
        assert!(crate::household::STORE_DIRS.contains(&DIR_NAME));
    }

    #[tokio::test]
    async fn card_reports_bundle_age() {
        let r = rig();
        assert_eq!(r.state.card().household_bundle_at_ms, None);
        r.put(&bundle_bytes(1), 1_700_000_000_000).await;
        assert_eq!(r.state.card().household_bundle_at_ms, Some(1_700_000_000_000));
        let page = crate::status_page::render(&r.state.page_view(), false);
        assert!(page.contains("Household settings held: captured 2023-11-14T22:13:20Z"), "{page}");
        let none = crate::status_page::render(&rig().state.page_view(), false);
        assert!(none.contains("Household settings held: none yet"), "{none}");
        // It survives a restart (opened again from the folder).
        let reopened = BundleStore::open(&r.dir.0);
        assert_eq!(reopened.captured_at_ms(), Some(1_700_000_000_000));
    }

    #[tokio::test]
    async fn a_torn_write_reads_as_none_held() {
        let r = rig();
        r.put(&bundle_bytes(1), 10).await;
        std::fs::write(r.dir.0.join("household/bundle-10.sealed"), b"short").unwrap();
        assert_eq!(BundleStore::open(&r.dir.0).captured_at_ms(), None);
    }

    #[tokio::test]
    async fn bundle_routes_require_auth() {
        let r = rig();
        for (method, path) in [("PUT", format!("{PATH}?capturedAtMs=5")), ("GET", PATH.to_string())] {
            let req = Request::builder().method(method).uri(&path).header("x-aurelis-companion", "1").body(Body::from("x")).unwrap();
            assert_eq!(r.send(req).await.0, StatusCode::UNAUTHORIZED, "{method}");
        }
        assert!(r.state.bundle.captured_at_ms().is_none());
    }

    #[tokio::test]
    async fn a_put_is_refused_when_the_disk_floor_is_breached() {
        let r = rig();
        let why = r
            .state
            .bundle
            .put(vec![1, 2, 3], 5, None, || true, async { Err("not stored: the host has 1 MB free".to_string()) })
            .await
            .unwrap_err();
        assert_eq!(why.status, StatusCode::INSUFFICIENT_STORAGE);
        assert!(why.reason.contains("1 MB free"));
        assert!(r.state.bundle.captured_at_ms().is_none());
    }

    #[tokio::test]
    async fn a_put_after_unpair_is_not_kept() {
        let r = rig();
        let why = r.state.bundle.put(vec![1], 5, None, || false, async { Ok(()) }).await.unwrap_err();
        assert_eq!(why.status, StatusCode::CONFLICT);
        assert!(!r.dir.0.join("household").exists());
    }
}
