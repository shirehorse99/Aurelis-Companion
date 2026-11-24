//! `/v1/picture/get`: pictures fetched through the Companion once and kept for the household.
//!
//! Off by default: the card's `pictureStore` capability is false until the household turns it on
//! (and the picture folder is on suitable storage), and while it is false the answer is a sealed 404
//! carrying the card's decline reason. When it is on, a picture is fetched on first request, stored
//! under `picture:<sha256(url)>` with its content type, and served from the store ever after.
//! Only `image/*` replies up to 20 MB are kept. Logs name the host and 12 hex characters of the
//! URL's hash, never the URL.

use crate::auth::Session;
use crate::guide_store::{check_fetchable, describe_err, fetch_allowed, one_line, picture_http_client, sealed_body, sha_hex, url_tag, UrlKind};
use crate::state::{now_ms, AppState};
use crate::store::{BlobStore, Meta};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use axum::Extension;
use futures_util::TryStreamExt;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, ReadBuf};

pub const PICTURE_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_PICTURE_BYTES: u64 = 20 * 1024 * 1024;
/// A picture is kept at most 90 days, then fetched again if it is still wanted. TMDB's terms allow a
/// cache of at most six months (docs/LEGAL_SOURCE_AUDIT_2026-10-09.md); 90 days keeps well inside it.
pub const MAX_PICTURE_AGE_MS: i64 = 90 * 24 * 3600 * 1000;
/// How often the held pictures are checked for age.
pub const AGE_SWEEP_EVERY: Duration = Duration::from_secs(24 * 3600);
const CONCURRENT_FETCHES: usize = 4;

fn gate() -> &'static tokio::sync::Semaphore {
    static GATE: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Semaphore::new(CONCURRENT_FETCHES))
}

pub fn picture_key(url: &str) -> String {
    format!("picture:{}", sha_hex(url))
}

/// Fails the read once more than `limit` bytes have passed, so the store aborts and says why.
struct Limited<R> {
    inner: R,
    seen: u64,
    limit: u64,
}

impl<R: AsyncRead + Unpin> AsyncRead for Limited<R> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &polled {
            self.seen += (buf.filled().len() - before) as u64;
            if self.seen > self.limit {
                buf.set_filled(before); // an error must not leave bytes behind
                return Poll::Ready(Err(std::io::Error::other(format!("the picture is over the {} MB limit", self.limit / (1024 * 1024)))));
            }
        }
        polled
    }
}

fn refuse(session: &Session, status: StatusCode, reason: &str) -> Response {
    session.sealed_reason(status, reason)
}

/// Fetches and stores one picture. `Err` is a reason for the 502.
async fn fetch_through(state: &Arc<AppState>, store: &BlobStore, url: &str, key: &str) -> Result<(), String> {
    fetch_allowed(state, |c| c.picture_store, "pictureStore").map_err(|why| format!("not fetching new pictures now ({why})"))?;
    let client = picture_http_client()?;
    let _permit = gate().acquire().await.map_err(|e| format!("the fetch queue failed: {e}"))?;
    let started = now_ms();
    let resp = client.get(url).timeout(PICTURE_TIMEOUT).send().await.map_err(describe_err)?;
    let code = resp.status();
    if !code.is_success() {
        return Err(format!("the picture's server answered HTTP {}", code.as_u16()));
    }
    let content_type = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(|s| one_line(s.trim(), 120));
    let Some(content_type) = content_type.filter(|c| c.to_ascii_lowercase().starts_with("image/")) else {
        return Err(format!("refused: the reply is not an image (Content-Type was '{}')", content_type_for_reason(&resp)));
    };
    let hint = resp.content_length();
    if hint.is_some_and(|n| n > MAX_PICTURE_BYTES) {
        return Err(format!("refused: the picture is {} bytes and at most {MAX_PICTURE_BYTES} (20 MB) are kept", hint.unwrap_or(0)));
    }
    let reader = tokio_util::io::StreamReader::new(resp.bytes_stream().map_err(|e| std::io::Error::other(describe_err(e))));
    let reader = Limited { inner: reader, seen: 0, limit: MAX_PICTURE_BYTES };
    let meta = Meta { stored_at_ms: started, content_type: Some(content_type), ..Meta::default() };
    match store.put_sized(key, reader, hint, meta).await {
        Ok(()) => Ok(()),
        Err(r) if r.reason.starts_with("stored, but") => {
            eprintln!("picture store: {}", r.reason);
            Ok(())
        }
        Err(r) => Err(r.reason),
    }
}

/// Deletes the pictures held past [MAX_PICTURE_AGE_MS]; run at start and every [AGE_SWEEP_EVERY].
pub async fn sweep_expired(store: &BlobStore, now_ms: i64) -> usize {
    let n = store.expire_older_than(now_ms - MAX_PICTURE_AGE_MS).await;
    if n > 0 {
        eprintln!("picture store: {n} picture(s) older than 90 days deleted");
    }
    n
}

fn content_type_for_reason(resp: &reqwest::Response) -> String {
    resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(|s| one_line(s, 60)).unwrap_or_else(|| "missing".to_string())
}

pub async fn get_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>, body: Bytes) -> Response {
    let bad = |m: &str| refuse(&session, StatusCode::BAD_REQUEST, m);
    let Ok(v) = serde_json::from_slice::<Value>(&body) else { return bad("request body is not JSON") };
    let Some(url) = v.get("url").and_then(Value::as_str) else { return bad("'url' must be text") };

    let card = state.card();
    let store = match state.stores.picture.as_ref().filter(|_| card.capabilities.picture_store) {
        Some(s) => s,
        None => {
            let why = card.declined.get("pictureStore").cloned().unwrap_or_else(|| "the picture store is not available".to_string());
            return refuse(&session, StatusCode::NOT_FOUND, &format!("the picture store is off: {why}"));
        }
    };
    // The same rule as a guide address, checked before any connection: no streams, http(s) only.
    match reqwest::Url::parse(url) {
        Ok(u) => {
            if let Err(why) = check_fetchable(&u, UrlKind::Picture) {
                return bad(&format!("refused: {why}"));
            }
        }
        Err(_) => return bad("'url' must be an http or https address"),
    }
    let key = picture_key(url);
    if store.meta(&key).is_some_and(|m| now_ms().saturating_sub(m.stored_at_ms) > MAX_PICTURE_AGE_MS) {
        // Past the age limit: never served; fetched again below.
        store.expire_older_than(now_ms() - MAX_PICTURE_AGE_MS).await;
    }
    if store.meta(&key).is_none() {
        if let Err(why) = fetch_through(&state, store, url, &key).await {
            eprintln!("picture store: {}: {why}", url_tag(url));
            return refuse(&session, StatusCode::BAD_GATEWAY, &why);
        }
    }
    match store.get(&key).await {
        Some((meta, file)) => {
            let mut res = Response::new(sealed_body(file, &session.secret));
            let ct = meta.content_type.as_deref().and_then(|c| HeaderValue::from_str(c).ok()).unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));
            res.headers_mut().insert(header::CONTENT_TYPE, ct);
            res
        }
        None => refuse(&session, StatusCode::BAD_GATEWAY, "the picture was stored but could not be read back (it may have been evicted); ask again"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guide_store::testkit::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    fn on() -> Rig {
        rig_with(|c| c.picture_store_enabled = true)
    }

    #[tokio::test]
    async fn picture_fetched_through_then_served_from_store() {
        let (base, origin) = origin().await;
        let r = on();
        let url = format!("{base}/pic.png?token=secret-token");
        for _ in 0..2 {
            let (s, headers, body) = r.authed_raw("/v1/picture/get", &json!({"url": url})).await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_TYPE], "image/png", "the original content type comes back");
            assert_eq!(open_stream(&body), vec![0x89u8, b'P', b'N', b'G', 1, 2, 3]);
        }
        assert_eq!(origin.picture_hits.load(Ordering::SeqCst), 1, "the second ask is served from the store");
        let meta = r.state.stores.picture.as_ref().unwrap().meta(&picture_key(&url)).unwrap();
        assert_eq!(meta.content_type.as_deref(), Some("image/png"));
    }

    #[tokio::test]
    async fn picture_store_off_returns_404_with_reason() {
        let (base, origin) = origin().await;
        let r = rig();
        let (s, v) = r.authed("/v1/picture/get", &json!({"url": format!("{base}/pic.png")})).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(v["reason"].as_str().unwrap().contains("switched off in the Companion's options (pictures)"), "{v}");
        assert_eq!(origin.picture_hits.load(Ordering::SeqCst), 0, "nothing is fetched while it is off");
    }

    #[tokio::test]
    async fn only_images_are_kept_and_the_refusal_says_why() {
        let (base, _) = origin().await;
        let r = on();
        let (s, v) = r.authed("/v1/picture/get", &json!({"url": format!("{base}/page")})).await;
        assert_eq!(s, StatusCode::BAD_GATEWAY);
        assert!(v["reason"].as_str().unwrap().contains("not an image") && v["reason"].as_str().unwrap().contains("text/html"), "{v}");
        assert_eq!(r.state.stores.picture.as_ref().unwrap().used_bytes(), 0);
        let (s, v) = r.authed("/v1/picture/get", &json!({"url": format!("{base}/missing.png")})).await;
        assert_eq!(s, StatusCode::BAD_GATEWAY);
        assert!(v["reason"].as_str().unwrap().contains("HTTP 404"), "{v}");
        assert_eq!(r.authed("/v1/picture/get", &json!({"url": "ftp://x.example/a.png"})).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(r.authed("/v1/picture/get", &json!({})).await.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn busy_serves_held_pictures_and_declines_new_ones_with_the_reason() {
        let (base, origin) = origin().await;
        let r = on();
        let held = format!("{base}/pic.png");
        assert_eq!(r.authed_raw("/v1/picture/get", &json!({"url": held})).await.0, StatusCode::OK);
        r.state.set_paused(true);
        assert_eq!(r.authed_raw("/v1/picture/get", &json!({"url": held})).await.0, StatusCode::OK, "held pictures are served while busy");
        let (s, v) = r.authed("/v1/picture/get", &json!({"url": format!("{base}/pic.png?other=1")})).await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{v}");
        assert!(v["reason"].as_str().unwrap().contains("paused on the status page"), "{v}");
        assert_eq!(origin.picture_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_picture_over_the_limit_is_refused_while_streaming() {
        let mut limited = Limited { inner: &[0u8; 100][..], seen: 0, limit: 50 };
        let mut sink = Vec::new();
        let e = tokio::io::AsyncReadExt::read_to_end(&mut limited, &mut sink).await.unwrap_err();
        assert!(e.to_string().contains("limit"), "{e}");
    }

    #[tokio::test]
    async fn unpair_wipes_only_the_companions_subfolder_of_the_picture_folder() {
        use crate::household::TempDir;
        let pics = TempDir::new();
        let r = rig_with({
            let p = pics.0.clone();
            move |c| c.picture_dir = Some(p)
        });
        let ours = pics.0.join(crate::store::PICTURE_SUBDIR);
        std::fs::create_dir_all(ours.join("ab")).unwrap();
        std::fs::write(ours.join("ab").join("blob"), b"x").unwrap();
        std::fs::write(pics.0.join("family-photos.jpg"), b"not ours").unwrap();
        std::fs::create_dir_all(pics.0.join("holiday")).unwrap();
        let (s, v) = r.authed("/v1/unpair", &json!({})).await;
        assert_eq!((s, &v["wiped"]), (StatusCode::OK, &json!(true)));
        assert!(!ours.exists(), "the Companion's own subfolder is gone");
        assert!(pics.0.join("family-photos.jpg").exists(), "the operator's own file survives");
        assert!(pics.0.join("holiday").is_dir(), "so does the operator's own folder");
    }

    #[tokio::test]
    async fn with_a_picture_dir_the_store_lives_in_its_subfolder() {
        use crate::household::TempDir;
        let pics = TempDir::new();
        let r = rig_with({
            let p = pics.0.clone();
            move |c| {
                c.picture_dir = Some(p);
                c.picture_store_enabled = true;
            }
        });
        // The test host cannot tell what storage the folder is on, so the capability is off here; the
        // store itself is still opened under the subfolder.
        assert!(pics.0.join(crate::store::PICTURE_SUBDIR).is_dir());
        assert!(r.state.stores.picture.is_some());
        assert!(!pics.0.join("tmp").exists() && !pics.0.join(crate::store::INDEX_FILE).exists(), "nothing is written beside the operator's files");
    }

    #[tokio::test]
    async fn stream_like_picture_urls_are_refused_before_connecting() {
        let (base, origin) = origin().await;
        let r = on();
        for path in ["/live/u/p/1.png", "/seg.ts"] {
            let (s, v) = r.authed("/v1/picture/get", &json!({"url": format!("{base}{path}")})).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{path}: {v}");
            let reason = v["reason"].as_str().unwrap();
            assert!(reason.contains("looks like a stream") && !reason.contains("127.0.0.1"), "{reason}");
        }
        assert_eq!(origin.picture_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_picture_older_than_90_days_is_fetched_again() {
        let (base, origin) = origin().await;
        let r = on();
        let url = format!("{base}/pic.png");
        let (s, _, _) = r.authed_raw("/v1/picture/get", &json!({"url": url})).await;
        assert_eq!(s, StatusCode::OK);
        let store = r.state.stores.picture.as_ref().unwrap();
        let key = picture_key(&url);
        store.edit_meta(&key, |m| m.stored_at_ms = now_ms() - MAX_PICTURE_AGE_MS - 60_000);
        let (s, _, body) = r.authed_raw("/v1/picture/get", &json!({"url": url})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(open_stream(&body), vec![0x89u8, b'P', b'N', b'G', 1, 2, 3]);
        assert_eq!(origin.picture_hits.load(Ordering::SeqCst), 2, "a copy past 90 days is not served; it is fetched again");
        assert!(now_ms() - store.meta(&key).unwrap().stored_at_ms < 60_000, "the new copy is dated now");
    }

    #[tokio::test]
    async fn the_age_sweep_drops_pictures_past_90_days() {
        let (base, _origin) = origin().await;
        let r = on();
        let old = format!("{base}/pic.png");
        let fresh = format!("{base}/pic.png?fresh=1");
        for url in [&old, &fresh] {
            let (s, _, _) = r.authed_raw("/v1/picture/get", &json!({"url": url})).await;
            assert_eq!(s, StatusCode::OK);
        }
        let store = r.state.stores.picture.as_ref().unwrap();
        store.edit_meta(&picture_key(&old), |m| m.stored_at_ms = now_ms() - MAX_PICTURE_AGE_MS - 60_000);
        let old_path = store.path_of(&picture_key(&old));
        assert!(old_path.exists());
        assert_eq!(sweep_expired(store, now_ms()).await, 1);
        assert!(store.meta(&picture_key(&old)).is_none(), "the old picture is forgotten");
        assert!(!old_path.exists(), "and its file deleted");
        assert!(store.meta(&picture_key(&fresh)).is_some(), "a fresh picture stays");
    }
}
