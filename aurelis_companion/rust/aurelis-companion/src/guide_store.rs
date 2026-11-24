//! `/v1/guide/register` and `/v1/guide/get`: the household's XMLTV guides, downloaded once here.
//!
//! The master television registers its guide URLs. The Companion fetches each on its own schedule
//! (only while its card says available), stores the body exactly as received, and serves it to
//! every television as a v4 sealed stream. A failed refresh keeps the last body and says so in the
//! `X-Source-Status` header.
//!
//! Rules this file keeps:
//! * Guide URLs only. A URL that looks like a stream (`.m3u8`, `.ts`, `/live/`, ...) is refused at
//!   registration, with a reason naming the source id and never the URL.
//! * A registration URL carries the provider's login. It lives only in `guide_sources.json`
//!   (mode 0600, deleted on unpair), and no log line, status header or reply ever repeats it:
//!   logs print the host and the first 12 hex characters of the URL's SHA-256.
//! * A guide is never held in memory: the download streams into the store and the reply streams out
//!   of the stored file in 64 KiB frames.

use crate::auth::{Refusal, Session};
use crate::state::{now_ms, AppState};
use crate::store::{BlobStore, Meta};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use companion_protocol::stream::{StreamSealer, MAX_FRAME_PLAINTEXT};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::error::Error as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub const SOURCES_FILE: &str = "guide_sources.json";
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const GUIDE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// A source asked to refresh more often than this is refreshed this often: a provider is not a
/// clock.
pub const MIN_REFRESH_MINUTES: u64 = 15;
pub const MAX_REFRESH_MINUTES: u64 = 7 * 24 * 60;
pub const MAX_SOURCES: usize = 64;
const MAX_ID_BYTES: usize = 200;
const MAX_REDIRECTS: usize = 10;
/// After a failed refresh the source is tried again after 10 minutes, then twice as long each time
/// it fails again, never more than 6 hours; a success resets it.
const FAILED_RETRY_MS: i64 = 10 * 60 * 1000;
const MAX_BACKOFF_MS: i64 = 6 * 60 * 60 * 1000;
/// A source whose guide is not held (evicted) is due now, but not more often than this: 15 minutes,
/// so a guide that keeps being evicted cannot make the Companion fetch it again and again.
const MIN_REFETCH_GAP_MS: i64 = 15 * 60 * 1000;
const SCHEDULER_TICK: Duration = Duration::from_secs(30);
/// Two guides download at once at most.
const CONCURRENT_FETCHES: usize = 2;
const ERROR_BODY_CHARS: usize = 200;

pub fn guide_key(id: &str) -> String {
    format!("guide:{id}")
}

// ---- helpers shared with the picture store -------------------------------------------------------

/// What an address is for. Guide addresses refuse the Xtream short stream form (`/user/pass/123`);
/// picture addresses do not, because `/img/abc/123` is an ordinary place for a picture to live.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UrlKind {
    Guide,
    Picture,
}

/// The HTTP client for guide fetches: rustls, no OpenSSL. Per-request timeouts override the default.
pub fn http_client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT.get_or_init(|| build_client(UrlKind::Guide)).as_ref().map_err(|e| e.clone())
}

/// The same, with the picture rule applied to every redirect hop.
pub fn picture_http_client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT.get_or_init(|| build_client(UrlKind::Picture)).as_ref().map_err(|e| e.clone())
}

fn build_client(kind: UrlKind) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(GUIDE_TIMEOUT)
        // Every hop gets the same check as the registered address: a provider (or anyone
        // in between) must not be able to send the Companion to a stream.
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            match check_fetchable(attempt.url(), kind) {
                Ok(()) => attempt.follow(),
                Err(why) => attempt.error(format!("a redirect led to an address that is refused: {why}")),
            }
        }))
        .user_agent(concat!("AurelisCompanion/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("the HTTP client could not be built: {e}"))
}

/// What a log line may say about a URL: the host and 12 hex characters of its SHA-256. Never the
/// path or query, which carry `username=` and `password=`.
pub fn url_tag(url: &str) -> String {
    let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| "unparsed".to_string());
    format!("{host} #{}", &sha_hex(url)[..12])
}

pub fn sha_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// A network error in words, without the URL reqwest would otherwise print.
pub fn describe_err(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

/// `2026-11-17T09:30:00Z`.
pub fn iso_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil from days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Printable ASCII only, one line: safe in a header and in a log.
/// Cuts a provider's words at the first `username=` or `password=` (any case): a failure message
/// can echo the login it was refused for, and the status travels to every television and its log.
pub fn scrub_secrets(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let cut = ["username=", "password="].iter().filter_map(|k| lower.find(k)).min();
    match cut {
        Some(i) => format!("{} [login removed]", s[..i].trim_end()),
        None => s.to_string(),
    }
}

/// What a registration did: ids to fetch now, and each entry skipped with its reason.
#[derive(Debug)]
pub struct Registration {
    pub fresh: Vec<String>,
    pub skipped: Vec<(String, String)>,
}

pub fn one_line(s: &str, max_chars: usize) -> String {
    s.chars().map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' }).take(max_chars).collect()
}

/// Is a fetch allowed right now: the card says available and the capability is on.
/// `Err` carries the reason.
pub fn fetch_allowed(state: &AppState, capability: fn(&companion_protocol::card::Capabilities) -> bool, name: &str) -> Result<(), String> {
    let card = state.card();
    if card.state != "available" {
        return Err(format!("busy: {}", card.busy_reason.unwrap_or_else(|| "no reason given".to_string())));
    }
    if !capability(&card.capabilities) {
        return Err(card.declined.get(name).cloned().unwrap_or_else(|| format!("the {name} is not available")));
    }
    Ok(())
}

// ---- the sealed reply stream ----------------------------------------------------------------------

async fn read_chunk(file: &mut tokio::fs::File) -> std::io::Result<Vec<u8>> {
    let mut chunk = vec![0u8; MAX_FRAME_PLAINTEXT];
    let mut n = 0;
    while n < chunk.len() {
        let r = file.read(&mut chunk[n..]).await?;
        if r == 0 {
            break;
        }
        n += r;
    }
    chunk.truncate(n);
    Ok(chunk)
}

struct OutState {
    file: tokio::fs::File,
    sealer: StreamSealer,
    header: Option<Vec<u8>>,
    pending: Option<Vec<u8>>,
    done: bool,
}

/// The stored file as a v4 stream: the header, then 64 KiB frames, the last one flagged. A read
/// error ends the stream without a last frame, which the television rejects as truncated.
pub fn sealed_body(file: tokio::fs::File, secret: &[u8; 32]) -> Body {
    let (sealer, header) = StreamSealer::new(secret);
    let st = OutState { file, sealer, header: Some(header), pending: None, done: false };
    let stream = futures_util::stream::unfold(st, |mut st| async move {
        if st.done {
            return None;
        }
        if let Some(h) = st.header.take() {
            return Some((Ok::<Bytes, std::io::Error>(Bytes::from(h)), st));
        }
        let cur = match st.pending.take() {
            Some(c) => c,
            None => match read_chunk(&mut st.file).await {
                Ok(c) => c,
                Err(e) => {
                    st.done = true;
                    return Some((Err(e), st));
                }
            },
        };
        let next = match read_chunk(&mut st.file).await {
            Ok(n) => n,
            Err(e) => {
                st.done = true;
                return Some((Err(e), st));
            }
        };
        let last = next.is_empty();
        let frame = st.sealer.frame(&cur, last);
        if last {
            st.done = true;
        } else {
            st.pending = Some(next);
        }
        Some((Ok(Bytes::from(frame)), st))
    });
    Body::from_stream(stream)
}

// ---- registrations --------------------------------------------------------------------------------

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Carries the provider's login. Never printed.
    pub url: String,
    pub refresh_minutes: u64,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Source {{ id: {:?}, url: {}, refresh_minutes: {} }}", self.id, url_tag(&self.url), self.refresh_minutes)
    }
}

/// May the Companion connect to this address at all? The reason never repeats the address.
/// Used at registration, on every redirect hop, and for picture addresses.
pub fn check_fetchable(url: &reqwest::Url, kind: UrlKind) -> Result<(), String> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!("only http and https addresses are fetched, not '{}'", one_line(url.scheme(), 20)));
    }
    if url.host_str().is_none() {
        return Err("the url has no host".to_string());
    }
    if let Some(why) = stream_like(url.path(), kind) {
        return Err(format!("it looks like a stream ({why}); the Companion fetches guide files only"));
    }
    Ok(())
}

/// Why a registration is refused, naming the source and never the URL.
pub fn validate_source(s: &Source) -> Result<(), String> {
    let id = one_line(&s.id, 60);
    if s.id.is_empty() || s.id.len() > MAX_ID_BYTES {
        return Err(format!("source '{id}': the id must be 1 to {MAX_ID_BYTES} bytes"));
    }
    let url = reqwest::Url::parse(&s.url).map_err(|_| format!("source '{id}': the url is not a valid address"))?;
    check_fetchable(&url, UrlKind::Guide).map_err(|why| format!("source '{id}': refused, {why}"))
}

const STREAM_EXTENSIONS: [(&str, &str); 6] = [
    (".m3u8", "the path ends in .m3u8"),
    (".ts", "the path ends in .ts"),
    (".mp4", "the path ends in .mp4"),
    (".mkv", "the path ends in .mkv"),
    (".mpd", "the path ends in .mpd"),
    (".flv", "the path ends in .flv"),
];
const STREAM_PARTS: [(&str, &str); 4] = [
    ("/live/", "the path has a /live/ part"),
    ("/movie/", "the path has a /movie/ part"),
    ("/series/", "the path has a /series/ part"),
    ("/timeshift/", "the path has a /timeshift/ part"),
];

pub fn stream_like(path: &str, kind: UrlKind) -> Option<&'static str> {
    let p = path.to_ascii_lowercase();
    for (ext, why) in STREAM_EXTENSIONS {
        if p.ends_with(ext) {
            return Some(why);
        }
    }
    for (part, why) in STREAM_PARTS {
        if p.contains(part) {
            return Some(why);
        }
    }
    if kind == UrlKind::Picture {
        return None;
    }
    // The Xtream short form (guide addresses only): /user/pass/123 or /user/pass/123.ts (three or more segments, the
    // last all digits, with or without an extension). `xmltv.php` and `epg.xml.gz` are not that.
    let segments: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() >= 3 {
        let last = segments[segments.len() - 1];
        let stem = last.split('.').next().unwrap_or("");
        if !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()) {
            return Some("the path ends in a numeric stream id");
        }
    }
    None
}

#[derive(Default, Clone)]
struct Runtime {
    etag: Option<String>,
    last_modified: Option<String>,
    /// Monotonic-agnostic wall ms of the last attempt; 0 = none since start.
    last_attempt_ms: i64,
    failed: bool,
    /// How long to wait after the last failure; 0 when the last fetch worked.
    backoff_ms: i64,
    /// The latest status in words, kept even when nothing is held to carry it.
    last_status: Option<String>,
}

#[derive(Default)]
struct Inner {
    sources: Vec<Source>,
    runtime: HashMap<String, Runtime>,
    inflight: HashSet<String>,
    /// Which stored body each guide holds now: a parse for any other is out of date.
    body_gen: HashMap<String, u64>,
    /// Parses begun and not finished, per guide.
    parsing: HashMap<String, usize>,
}

pub struct GuideSources {
    path: PathBuf,
    inner: Mutex<Inner>,
    reg_lock: tokio::sync::Mutex<()>,
    /// Bumped by `clear` (unpair): a registration that began before it cannot land after it.
    generation: AtomicU64,
    gate: tokio::sync::Semaphore,
    busy_logged: AtomicBool,
    /// One parse at a time: a parse holds a whole guide in memory, and `write_parsed` writes a
    /// fixed temporary name.
    parse_gate: tokio::sync::Semaphore,
    next_gen: AtomicU64,
    #[cfg(test)]
    parse_running: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    parse_overlaps: std::sync::atomic::AtomicUsize,
    /// Test hooks: a parse waits until `parse_release` is set after reading the body; `land_hook` runs once a
    /// body has been stored and before the download returns.
    #[cfg(test)]
    parse_release: Arc<AtomicBool>,
    #[cfg(test)]
    land_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

enum Outcome {
    Stored,
    NotModified,
    /// The status text (it may carry the provider's words) and the log text (it never does).
    Failed(String, String),
}

fn failed(why: String) -> Outcome {
    Outcome::Failed(why.clone(), why)
}

impl GuideSources {
    pub fn load(data_dir: &Path) -> GuideSources {
        let path = data_dir.join(SOURCES_FILE);
        let sources = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<Source>>(&bytes) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("guide store: {} is unreadable ({e}); no guide sources registered until the television registers them again", path.display());
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                eprintln!("guide store: cannot read {}: {e}", path.display());
                Vec::new()
            }
        };
        GuideSources {
            path,
            inner: Mutex::new(Inner { sources, ..Default::default() }),
            reg_lock: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            gate: tokio::sync::Semaphore::new(CONCURRENT_FETCHES),
            busy_logged: AtomicBool::new(false),
            parse_gate: tokio::sync::Semaphore::new(1),
            next_gen: AtomicU64::new(0),
            #[cfg(test)]
            parse_running: Default::default(),
            #[cfg(test)]
            parse_overlaps: Default::default(),
            #[cfg(test)]
            parse_release: Arc::new(AtomicBool::new(true)),
            #[cfg(test)]
            land_hook: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn source(&self, id: &str) -> Option<Source> {
        self.lock().sources.iter().find(|s| s.id == id).cloned()
    }

    /// Every registered guide with what became of its parse ("" when nothing is held yet).
    pub fn parse_rows(&self, store: &BlobStore) -> Vec<(String, String, String)> {
        let sources: Vec<_> = self.lock().sources.iter().map(|s| (s.id.clone(), s.name.clone())).collect();
        sources.into_iter()
            .map(|(id, name)| {
                let status = store.meta(&guide_key(&id)).map(|m| m.parse_status).filter(|s| !s.is_empty()).unwrap_or_else(|| "not held yet".to_string());
                (id, name, status)
            })
            .collect()
    }

    fn status_of(&self, id: &str) -> Option<String> {
        self.lock().runtime.get(id).and_then(|r| r.last_status.clone())
    }

    /// Replaces the whole set. Returns the ids that are new or whose URL changed (to fetch now).
    /// Nothing changes if any source is refused or the file cannot be written.
    #[allow(dead_code)] // the route uses `register_at`; tests and later callers use this
    pub async fn register(&self, incoming: Vec<Source>) -> Result<Vec<String>, Refusal> {
        self.register_at(self.generation.load(Ordering::SeqCst), incoming).await.map(|r| r.fresh)
    }

    /// `generation` is what `generation()` said when the request arrived.
    /// An entry that fails `validate_source` (or repeats an id) is skipped and reported with its reason;
    /// every other entry is registered. One bad address never holds back the household's other guides.
    pub async fn register_at(&self, generation: u64, incoming: Vec<Source>) -> Result<Registration, Refusal> {
        let bad = |reason: String| Refusal::new(StatusCode::BAD_REQUEST, reason);
        if incoming.len() > MAX_SOURCES {
            return Err(bad(format!("{} sources were offered and at most {MAX_SOURCES} are kept", incoming.len())));
        }
        let mut seen = HashSet::new();
        let mut skipped: Vec<(String, String)> = Vec::new();
        let mut kept_in: Vec<Source> = Vec::new();
        for s in incoming {
            if let Err(reason) = validate_source(&s) {
                skipped.push((s.id.clone(), reason));
            } else if !seen.insert(s.id.clone()) {
                skipped.push((s.id.clone(), format!("source '{}': the id is offered twice", one_line(&s.id, 60))));
            } else {
                kept_in.push(s);
            }
        }
        let incoming: Vec<Source> = kept_in
            .into_iter()
            .map(|mut s| {
                s.refresh_minutes = s.refresh_minutes.clamp(MIN_REFRESH_MINUTES, MAX_REFRESH_MINUTES);
                s
            })
            .collect();
        let _serial = self.reg_lock.lock().await;
        if self.generation.load(Ordering::SeqCst) != generation {
            return Err(Refusal::new(StatusCode::CONFLICT, "the household was unpaired while this registration was waiting; nothing was saved"));
        }
        let body = serde_json::to_vec(&incoming).map_err(|e| Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, format!("the registrations could not be encoded: {e}")))?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || write_private(&path, &body))
            .await
            .unwrap_or_else(|e| Err(format!("the save task failed: {e}")))
            .map_err(|e| Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, format!("the registrations could not be saved: {e}")))?;
        let mut inner = self.lock();
        let mut fresh = Vec::new();
        let mut kept: HashMap<String, Runtime> = HashMap::new();
        for s in &incoming {
            let same = inner.sources.iter().any(|o| o.id == s.id && o.url == s.url);
            if same {
                if let Some(r) = inner.runtime.get(&s.id) {
                    kept.insert(s.id.clone(), r.clone());
                }
            } else {
                fresh.push(s.id.clone());
            }
        }
        inner.sources = incoming;
        inner.runtime = kept;
        Ok(Registration { fresh, skipped })
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Unpair: forget every source, delete the file. It waits for a registration in progress, and
    /// any registration that began earlier is refused afterwards, so the file cannot come back.
    pub async fn clear(&self) -> Result<(), String> {
        let _serial = self.reg_lock.lock().await;
        self.generation.fetch_add(1, Ordering::SeqCst);
        {
            let mut inner = self.lock();
            inner.sources.clear();
            inner.runtime.clear();
            inner.body_gen.clear();
        }
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot delete {}: {e}", self.path.display())),
        }
    }

    /// Takes the right to fetch `id`. False if it is not registered or is already being fetched.
    fn claim(&self, id: &str) -> bool {
        let mut inner = self.lock();
        inner.sources.iter().any(|s| s.id == id) && inner.inflight.insert(id.to_string())
    }

    fn release(&self, id: &str) {
        self.lock().inflight.remove(id);
    }

    #[cfg(test)]
    pub fn is_idle(&self) -> bool {
        let inner = self.lock();
        inner.inflight.is_empty() && inner.parsing.is_empty()
    }

    /// A new stored body for `id`: every parse begun for an earlier one is now out of date.
    fn new_body(&self, id: &str) -> u64 {
        let gen = self.next_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.lock().body_gen.insert(id.to_string(), gen);
        gen
    }

    /// Undoes `new_body` for a body that never landed (only if nothing newer was stored since).
    fn restore_body(&self, id: &str, gen: u64, previous: Option<u64>) {
        let mut inner = self.lock();
        if inner.body_gen.get(id) == Some(&gen) {
            match previous {
                Some(p) => inner.body_gen.insert(id.to_string(), p),
                None => inner.body_gen.remove(id),
            };
        }
    }

    fn body_is(&self, id: &str, gen: u64) -> bool {
        self.lock().body_gen.get(id) == Some(&gen)
    }

    fn current_body(&self, id: &str) -> Option<u64> {
        self.lock().body_gen.get(id).copied()
    }

    fn parse_begun(&self, id: &str) {
        *self.lock().parsing.entry(id.to_string()).or_default() += 1;
    }

    fn parse_ended(&self, id: &str) {
        let mut inner = self.lock();
        if let Some(n) = inner.parsing.get_mut(id) {
            *n -= 1;
            if *n == 0 {
                inner.parsing.remove(id);
            }
        }
    }

    fn parse_in_flight(&self, id: &str) -> bool {
        self.lock().parsing.contains_key(id)
    }

    /// Sources whose time has come. `held_at` gives the fetched-at of a held body, so a restart
    /// does not refetch a guide that is still fresh. A guide that is not held at all (evicted, or
    /// never fetched) is due now, whatever the last attempt was, except after a failure, which waits
    /// out its back-off, and for a minute after an attempt that worked (so an evict-refetch loop
    /// cannot hammer a provider).
    fn due(&self, now: i64, held_at: &dyn Fn(&str) -> Option<i64>) -> Vec<String> {
        let inner = self.lock();
        inner
            .sources
            .iter()
            .filter(|s| !inner.inflight.contains(&s.id))
            .filter(|s| {
                let every = s.refresh_minutes as i64 * 60_000;
                let rt = inner.runtime.get(&s.id);
                let last = rt.map(|r| r.last_attempt_ms).filter(|t| *t > 0);
                if let Some(r) = rt.filter(|r| r.failed) {
                    return last.map_or(true, |at| now >= at + r.backoff_ms.max(FAILED_RETRY_MS));
                }
                match held_at(&s.id) {
                    None => last.map_or(true, |at| now >= at + MIN_REFETCH_GAP_MS),
                    Some(held) => now >= last.unwrap_or(held) + every,
                }
            })
            .map(|s| s.id.clone())
            .collect()
    }

    fn note_busy_once(&self, why: &str) {
        if !self.busy_logged.swap(true, Ordering::Relaxed) {
            eprintln!("guide store: not fetching guides now ({why}); the guides held are still served");
        }
    }
}

/// 0600 from the first byte: the file holds URLs with logins. Atomic: temp file, fsync, rename.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
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
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot move {} into place: {e}", path.display()));
    }
    Ok(())
}

// ---- fetching -------------------------------------------------------------------------------------

/// Claims `id` now and fetches it in the background.
pub fn spawn_refresh(state: &Arc<AppState>, id: &str) {
    if state.guides.claim(id) {
        let (state, id) = (state.clone(), id.to_string());
        tokio::spawn(async move { run_claimed(&state, &id).await });
    }
}

/// Fetches `id` now (unless it is being fetched already or is not registered).
pub async fn refresh(state: &Arc<AppState>, id: &str) {
    if state.guides.claim(id) {
        run_claimed(state, id).await;
    }
}

async fn run_claimed(state: &Arc<AppState>, id: &str) {
    fetch_claimed(state, id).await;
    state.guides.release(id);
}

async fn fetch_claimed(state: &Arc<AppState>, id: &str) {
    let guides = &state.guides;
    let Some(source) = guides.source(id) else { return };
    if let Err(why) = fetch_allowed(state, |c| c.guide_store, "guideStore") {
        guides.note_busy_once(&why);
        return;
    }
    guides.busy_logged.store(false, Ordering::Relaxed);
    let Ok(_permit) = guides.gate.acquire().await else { return };
    let key = guide_key(id);
    let started = now_ms();
    let tag = url_tag(&source.url);
    let (etag, last_modified) = {
        let mut inner = guides.lock();
        let rt = inner.runtime.entry(id.to_string()).or_default();
        rt.last_attempt_ms = started;
        (rt.etag.clone(), rt.last_modified.clone())
    };
    let held = state.stores.guide.meta(&key).is_some();
    let outcome = download(state, &source, &key, started, held.then_some((etag, last_modified))).await;
    // A registration that changed or vanished while this ran: its result is not recorded.
    if guides.source(id).map(|s| s.url) != Some(source.url.clone()) {
        return;
    }
    let status = match &outcome {
        Outcome::Stored | Outcome::NotModified => "ok".to_string(),
        Outcome::Failed(status, _) => format!("failed {}: {}", iso_utc(now_ms()), scrub_secrets(status)),
    };
    let shown = one_line(id, 60);
    match &outcome {
        Outcome::Stored => println!("guide store: fetched {shown} from {tag}"),
        Outcome::NotModified => println!("guide store: {shown} from {tag} is unchanged (304)"),
        // The log says what failed, never what the provider's body said (it can echo a username).
        Outcome::Failed(_, log) => eprintln!("guide store: {shown} from {tag}: {log}; keeping what is held"),
    }
    let short_id = one_line(id, 12);
    let label = if source.name.is_empty() { short_id.as_str() } else { &source.name };
    match &outcome {
        Outcome::Stored => state.push_job(&format!("Guide {label}: downloaded")),
        Outcome::NotModified => state.push_job(&format!("Guide {label}: unchanged (HTTP 304)")),
        Outcome::Failed(_, _) => state.push_job(&format!("Guide {label}: refresh failed; kept any cached copy")),
    }
    if matches!(outcome, Outcome::NotModified | Outcome::Failed(..)) {
        let unchanged = matches!(outcome, Outcome::NotModified);
        state.stores.guide.edit_meta(&key, |meta| {
            if unchanged {
                meta.stored_at_ms = started;
            }
            meta.status = Some(status.clone());
        });
    }
    let parse_now = match &outcome {
        Outcome::Stored => true,
        // A held body whose parse is missing, out of date or gone is parsed again (see `needs_parse`);
        // one the parser read as empty is not tried every hour.
        Outcome::NotModified => !guides.parse_in_flight(id) && state.stores.guide.meta(&key).is_some_and(|m| needs_parse(&m, artefact_held(state, &key))),
        Outcome::Failed(..) => false,
    };
    if parse_now {
        let gen = match &outcome {
            Outcome::Stored => guides.current_body(id).unwrap_or_else(|| guides.new_body(id)),
            _ => guides.new_body(id),
        };
        start_parse(state, id, &key, gen).await;
    }
    let mut inner = guides.lock();
    if let Some(rt) = inner.runtime.get_mut(id) {
        rt.failed = matches!(outcome, Outcome::Failed(..));
        rt.backoff_ms = if rt.failed { if rt.backoff_ms == 0 { FAILED_RETRY_MS } else { (rt.backoff_ms * 2).min(MAX_BACKOFF_MS) } } else { 0 };
        rt.last_status = Some(status);
    }
}

async fn download(state: &Arc<AppState>, source: &Source, key: &str, started: i64, validators: Option<(Option<String>, Option<String>)>) -> Outcome {
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    let mut req = client.get(&source.url);
    if let Some((etag, last_modified)) = &validators {
        if let Some(v) = etag {
            req = req.header(header::IF_NONE_MATCH, v);
        }
        if let Some(v) = last_modified {
            req = req.header(header::IF_MODIFIED_SINCE, v);
        }
    }
    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let redirect = e.is_redirect();
            let why = describe_err(e);
            return failed(if redirect { format!("source '{}': {why}", one_line(&source.id, 60)) } else { why });
        }
    };
    let code = resp.status();
    if code == StatusCode::NOT_MODIFIED && validators.is_some() {
        return Outcome::NotModified;
    }
    if !code.is_success() {
        let mut snippet = Vec::new();
        while snippet.len() < 4 * ERROR_BODY_CHARS {
            match resp.chunk().await {
                Ok(Some(c)) => snippet.extend_from_slice(&c),
                _ => break,
            }
        }
        let text = String::from_utf8_lossy(&snippet);
        let text = one_line(text.trim(), ERROR_BODY_CHARS);
        return Outcome::Failed(format!("HTTP {} {}", code.as_u16(), text).trim_end().to_string(), format!("HTTP {}", code.as_u16()));
    }
    let content_type = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(|s| one_line(s.trim(), 80)).unwrap_or_default();
    if is_stream_content_type(&content_type) {
        return failed(format!("source '{}': refused, the reply is a stream or playlist (Content-Type '{content_type}'), not a guide file", one_line(&source.id, 60)));
    }
    let header_text = |name: header::HeaderName| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let new_etag = header_text(header::ETAG);
    let new_modified = header_text(header::LAST_MODIFIED);
    let hint = resp.content_length();
    let reader = tokio_util::io::StreamReader::new(resp.bytes_stream().map_err(|e| std::io::Error::other(describe_err(e))));
    let meta = Meta { stored_at_ms: started, status: Some("ok".to_string()), ..Meta::default() };
    // Parses begun for an earlier body are out of date from before the new one lands (a parse that
    // ends between the landing and a later bump would record the old summary against the new body).
    let previous = state.guides.current_body(&source.id);
    let gen = state.guides.new_body(&source.id);
    match state.stores.guide.put_sized(key, reader, hint, meta).await {
        Ok(()) => {}
        // Stored, with the index write to be retried: still a held guide.
        Err(r) if r.reason.starts_with("stored, but") => eprintln!("guide store: {}", r.reason),
        Err(r) => {
            // The old body is still the one held.
            state.guides.restore_body(&source.id, gen, previous);
            return failed(r.reason);
        }
    }
    #[cfg(test)]
    if let Some(hook) = state.guides.land_hook.lock().unwrap().as_ref() {
        hook();
    }

    if let Some(rt) = state.guides.lock().runtime.get_mut(&source.id) {
        rt.etag = new_etag;
        rt.last_modified = new_modified;
    }
    Outcome::Stored
}

/// Whether a held guide must be parsed (again): never parsed (unless the parser read it as empty, or
/// failed in a way a retry would not mend — only a parse that never ran, was left "parsing" or lacked the
/// memory is retried), parsed by other parser logic, or its artefact is gone from the disk.
pub fn needs_parse(m: &Meta, artefact_exists: bool) -> bool {
    match &m.parsed {
        Some(p) => p.logic != xmltv_parser::LOGIC_VERSION || !artefact_exists,
        None => m.parse_status.is_empty() || m.parse_status == "parsing" || m.parse_status.starts_with("not parsed: needs"),
    }
}

fn artefact_held(state: &AppState, key: &str) -> bool {
    crate::store::artefact_path(&state.stores.guide.path_of(key)).exists()
}

/// Queues a parse of `id`'s held body (generation `gen`) through the one-permit gate. Whatever artefact
/// sits beside the body is dropped first: it is out of date, or missing, or about to be replaced.
async fn start_parse(state: &Arc<AppState>, id: &str, key: &str, gen: u64) {
    state.stores.guide.drop_artefact(key).await;
    state.stores.guide.edit_meta(key, |m| {
        m.parsed = None;
        m.parse_status = "parsing".to_string();
    });
    state.guides.parse_begun(id);
    let (state2, id2, key2) = (state.clone(), id.to_string(), key.to_string());
    tokio::spawn(async move { parse_after_store(state2, id2, key2, gen).await });
}

/// A "parsing" status no parse stands behind (the Companion stopped mid-parse) is cleared, so it is
/// never reported to a television or on the status page. Returns how many were cleared.
pub fn clear_stale_parsing(state: &AppState) -> usize {
    let ids: Vec<String> = state.guides.lock().sources.iter().map(|s| s.id.clone()).collect();
    let mut cleared = 0;
    for id in ids {
        if state.guides.parse_in_flight(&id) {
            continue;
        }
        state.stores.guide.edit_meta(&guide_key(&id), |m| {
            if m.parse_status == "parsing" {
                m.parse_status.clear();
                cleared += 1;
            }
        });
    }
    cleared
}

/// How long the start-up sweep waits for the card to say available before it leaves the re-parse to
/// each guide's next refresh (which re-parses on a 304 as well as a 200).
const SWEEP_WAIT_MS: i64 = 30 * 60_000;

/// Once, after the scheduler starts: clears a stale "parsing", then queues a parse of every held guide
/// that needs one (`needs_parse`) — after an add-on rebuild every artefact is another logic's, and
/// waiting for each guide's next 200 could take a week. Fetches nothing; parses one at a time.
pub async fn startup_sweep(state: Arc<AppState>) {
    let cleared = clear_stale_parsing(&state);
    if cleared > 0 {
        println!("guide store: start-up: {cleared} guide(s) said \"parsing\" with no parse running; cleared");
    }
    let started = now_ms();
    loop {
        match fetch_allowed(&state, |c| c.guide_store, "guideStore") {
            Ok(()) => break,
            Err(why) if now_ms() - started >= SWEEP_WAIT_MS => {
                println!("guide store: start-up re-parse not done ({why}); each guide's next refresh re-parses it");
                return;
            }
            Err(_) => tokio::time::sleep(SCHEDULER_TICK).await,
        }
    }
    let ids: Vec<String> = state.guides.lock().sources.iter().map(|s| s.id.clone()).collect();
    let mut queued = 0;
    for id in ids {
        let key = guide_key(&id);
        if state.guides.lock().inflight.contains(&id) || state.guides.parse_in_flight(&id) {
            continue; // a fetch or parse under way decides for itself
        }
        let Some(meta) = state.stores.guide.meta(&key) else { continue };
        if needs_parse(&meta, artefact_held(&state, &key)) {
            let gen = state.guides.new_body(&id);
            start_parse(&state, &id, &key, gen).await;
            queued += 1;
        }
    }
    if queued > 0 {
        println!("guide store: start-up: {queued} held guide(s) queued to be parsed (never parsed, parsed by other parser logic, or the parse file missing)");
    }
}

/// Ends a parse's claim on its guide however the task leaves.
struct ParseTicket<'a>(&'a GuideSources, &'a str);

impl Drop for ParseTicket<'_> {
    fn drop(&mut self) {
        self.0.parse_ended(self.1);
    }
}

/// Parses the stored body of `id` into the artefact beside it and records what came of it.
/// `gen` names the body this was started for; if another has been stored since, nothing is kept.
async fn parse_after_store(state: Arc<AppState>, id: String, key: String, gen: u64) {
    let guides = &state.guides;
    let _ticket = ParseTicket(guides, &id);
    let Ok(_permit) = guides.parse_gate.acquire().await else { return };
    if !guides.body_is(&id, gen) {
        return; // a newer body has its own parse
    }
    let tag = guides.source(&id).map(|s| url_tag(&s.url)).unwrap_or_else(|| "(no longer registered)".to_string());
    let shown = one_line(&id, 60);
    let body = state.stores.guide.path_of(&key);
    let out = crate::store::artefact_path(&body);
    let record = |parsed: Option<crate::guide_parse::ParsedSummary>, status: String| {
        state.stores.guide.edit_meta(&key, |m| {
            m.parsed = parsed;
            m.parse_status = status;
        });
    };
    let body_bytes = match tokio::fs::metadata(&body).await {
        Ok(m) => m.len(),
        Err(e) => {
            let why = format!("not parsed: the stored guide cannot be read ({e})");
            eprintln!("guide store: {shown} from {tag}: {why}");
            state.stores.guide.drop_artefact(&key).await;
            record(None, why);
            return;
        }
    };
    if let Err(why) = crate::guide_parse::parse_budget_ok(body_bytes, state.facts().free_ram_mb) {
        let why = format!("not parsed: {why}");
        eprintln!("guide store: {shown} from {tag}: {why}");
        state.stores.guide.drop_artefact(&key).await;
        record(None, why);
        return;
    }
    let (body2, out2, id2) = (body.clone(), out.clone(), id.clone());
    #[cfg(test)]
    let hold = guides.parse_release.clone();
    #[cfg(test)]
    if guides.parse_running.fetch_add(1, Ordering::SeqCst) > 0 {
        guides.parse_overlaps.fetch_add(1, Ordering::SeqCst);
    }
    let result = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        std::thread::sleep(Duration::from_millis(40));
        #[cfg(test)]
        let after_read = || {
            while !hold.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        #[cfg(test)]
        let summary = crate::guide_parse::write_parsed_hooked(&body2, &id2, &out2, &after_read)?;
        #[cfg(not(test))]
        let summary = crate::guide_parse::write_parsed(&body2, &id2, &out2)?;
        let held_sha = crate::guide_parse::file_sha256(&body2)?;
        Ok((summary, held_sha))
    })
    .await
    .unwrap_or_else(|e| Err(format!("the parse task failed: {e}")));
    #[cfg(test)]
    guides.parse_running.fetch_sub(1, Ordering::SeqCst);
    if !guides.body_is(&id, gen) {
        // The body was replaced while this ran: what it wrote describes the old one.
        state.stores.guide.drop_artefact(&key).await;
        println!("guide store: {shown} from {tag}: a newer body arrived during the parse; its result is dropped");
        return;
    }
    match result {
        Ok((summary, held_sha)) if summary.body_sha256 != held_sha => {
            // What was parsed is not the body held now: never recorded as its parse.
            state.stores.guide.drop_artefact(&key).await;
            let why = "not parsed: the guide changed while it was being parsed".to_string();
            eprintln!("guide store: {shown} from {tag}: {why}");
            record(None, why);
        }
        Ok((summary, _)) => {
            if !state.stores.guide.count_artefact(&key) {
                // The guide was evicted meanwhile: the artefact goes with it.
                let _ = tokio::fs::remove_file(&out).await;
                return;
            }
            let secs = summary.parse_ms.div_ceil(1000).max(1);
            let status = format!("parsed {} programmes in {} s", summary.entries, secs);
            println!("guide store: {shown} from {tag}: {status}");
            record(Some(summary), status);
            if !guides.body_is(&id, gen) {
                // A newer body began landing between the check and the record: take it back.
                state.stores.guide.drop_artefact(&key).await;
                record(None, "parsing".to_string());
            }
        }
        Err(e) => {
            state.stores.guide.drop_artefact(&key).await;
            let why = format!("not parsed: {}", scrub_secrets(&e));
            eprintln!("guide store: {shown} from {tag}: {why}");
            record(None, why);
        }
    }
}

/// A video, or an HLS playlist: never a guide.
fn is_stream_content_type(content_type: &str) -> bool {
    let c = content_type.to_ascii_lowercase();
    c.starts_with("video/") || c.contains("mpegurl")
}

/// Runs for the life of the Companion: every 30 s, fetches the sources that are due.
pub async fn run_scheduler(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(SCHEDULER_TICK);
    loop {
        tick.tick().await;
        let store: &BlobStore = &state.stores.guide;
        let due = state.guides.due(now_ms(), &|id| store.meta(&guide_key(id)).map(|m| m.stored_at_ms));
        for id in due {
            spawn_refresh(&state, &id);
        }
    }
}

// ---- routes ---------------------------------------------------------------------------------------

fn parse(body: &Bytes) -> Result<Value, Refusal> {
    serde_json::from_slice(body).map_err(|e| Refusal::new(StatusCode::BAD_REQUEST, format!("request body is not JSON: {e}")))
}

fn sources_of(v: &Value) -> Result<Vec<Source>, Refusal> {
    let bad = |m: String| Refusal::new(StatusCode::BAD_REQUEST, m);
    let list = v.get("sources").and_then(Value::as_array).ok_or_else(|| bad("'sources' must be a list".to_string()))?;
    list.iter()
        .enumerate()
        .map(|(i, s)| {
            let id = s.get("id").and_then(Value::as_str).ok_or_else(|| bad(format!("source #{}: 'id' must be text", i + 1)))?;
            let url = s.get("url").and_then(Value::as_str).ok_or_else(|| bad(format!("source '{}': 'url' must be text", one_line(id, 60))))?;
            let refresh_minutes =
                s.get("refreshMinutes").and_then(Value::as_u64).ok_or_else(|| bad(format!("source '{}': 'refreshMinutes' must be a whole number", one_line(id, 60))))?;
            let name = s.get("name").and_then(Value::as_str).unwrap_or_default().trim()
                .chars().filter(|c| !c.is_control()).take(80).collect::<String>();
            let name = if name.contains("://") { String::new() } else { name };
            Ok(Source { id: id.to_string(), name, url: url.to_string(), refresh_minutes })
        })
        .collect()
}

pub async fn register_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>, body: Bytes) -> Response {
    let generation = state.guides.generation();
    let sources = match parse(&body).and_then(|v| sources_of(&v)) {
        Ok(s) => s,
        Err(r) => return session.sealed_reason(r.status, &r.reason),
    };
    let offered = sources.len();
    match state.guides.register_at(generation, sources).await {
        Ok(Registration { fresh, skipped }) => {
            let n = offered - skipped.len();
            for id in fresh {
                spawn_refresh(&state, &id);
            }
            for (id, reason) in &skipped {
                eprintln!("guide store: source '{}' skipped: {reason}", one_line(id, 12));
            }
            state.push_job(&format!("guide sources registered: {n}, skipped: {}", skipped.len()));
            let skipped: Vec<Value> = skipped.iter().map(|(id, reason)| json!({ "id": id, "reason": reason })).collect();
            session.sealed_json(StatusCode::OK, &json!({ "registered": n, "skipped": skipped }))
        }
        Err(r) => {
            eprintln!("guide store: registration refused: {}", r.reason);
            session.sealed_reason(r.status, &r.reason)
        }
    }
}

pub async fn get_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>, body: Bytes) -> Response {
    let id = match parse(&body).and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string).ok_or_else(|| Refusal::new(StatusCode::BAD_REQUEST, "'id' must be text"))) {
        Ok(id) => id,
        Err(r) => return session.sealed_reason(r.status, &r.reason),
    };
    let wanted = parse(&body).ok().and_then(|v| v.get("parsedLogic").and_then(Value::as_str).map(str::to_string));
    let key = guide_key(&id);
    match state.stores.guide.get(&key).await {
        Some((meta, mut file)) => {
            // Parsed form only for a television that names this Companion's parser version, and
            // only when the copy is really there; otherwise the XML, with the reason when asked.
            let mut form = "xml";
            let mut why: Option<String> = None;
            if let Some(w) = wanted.as_deref() {
                let well_formed = w.len() == 64 && w.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                match &meta.parsed {
                    Some(p) if well_formed && p.logic == w => match tokio::fs::File::open(crate::store::artefact_path(&state.stores.guide.path_of(&key))).await {
                        Ok(af) => {
                            file = af;
                            form = "parsed";
                        }
                        Err(e) => why = Some(format!("the parsed copy is missing ({}); serving the XML", e.kind())),
                    },
                    Some(_) => why = Some("the Companion parses with a different parser version".to_string()),
                    None if meta.parse_status == "parsing" => why = Some("still parsing".to_string()),
                    None if meta.parse_status.is_empty() => why = Some("not parsed: no parse has run for this guide yet".to_string()),
                    None => why = Some(meta.parse_status.clone()),
                }
            }
            let status = meta.status.clone().or_else(|| state.guides.status_of(&id)).unwrap_or_else(|| "ok".to_string());
            let status = HeaderValue::from_str(&one_line(&status, 400)).unwrap_or_else(|_| HeaderValue::from_static("ok"));
            let mut res = Response::new(sealed_body(file, &session.secret));
            let h = res.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            h.insert("x-fetched-at-ms", HeaderValue::from(meta.stored_at_ms.max(0) as u64));
            h.insert("x-source-status", status);
            h.insert("x-guide-form", HeaderValue::from_static(form));
            if form == "parsed" {
                if let Some(p) = &meta.parsed {
                    h.insert("x-parsed-entries", HeaderValue::from(p.entries));
                }
            }
            if let Some(w) = why {
                if let Ok(v) = HeaderValue::from_str(&one_line(&w, 300)) {
                    h.insert("x-parsed-why", v);
                }
            }
            res
        }
        None => {
            let shown = one_line(&id, 60);
            let reason = match (state.guides.source(&id), state.guides.status_of(&id)) {
                (Some(_), Some(status)) if status == "ok" => format!("the guide for source '{shown}' is no longer held (evicted, refetching)"),
                (Some(_), Some(status)) => format!("source '{shown}': last fetch failed: {}", status.strip_prefix("failed ").unwrap_or(&status)),
                (Some(_), None) => format!("source '{shown}' is registered but its first download has not finished"),
                (None, _) => format!("no guide source '{shown}' is registered and none is held"),
            };
            session.sealed_reason(StatusCode::NOT_FOUND, &reason).into_response()
        }
    }
}

/// Rigs and a tiny provider on 127.0.0.1, shared with the picture store's tests.
#[cfg(test)]
pub mod testkit {
    use super::*;
    use crate::auth::{b64, unb64};
    use crate::household::{Household, TempDir};
    use crate::state::{test_config, test_state};
    use axum::body::to_bytes;
    use axum::extract::State as AxState;
    use axum::http::{HeaderMap, Request};
    use axum::routing::get;
    use axum::Router;
    use companion_protocol::stream::StreamOpener;
    use companion_protocol::{proof, seal};
    use std::sync::atomic::AtomicUsize;
    use tower::ServiceExt;

    pub const SECRET: [u8; 32] = [9u8; 32];

    pub struct Rig {
        pub dir: TempDir,
        pub state: Arc<AppState>,
    }

    pub fn rig_with(edit: impl FnOnce(&mut crate::config::Config)) -> Rig {
        let dir = TempDir::new();
        Household::create(&dir.0, SECRET).expect("household");
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        edit(&mut config);
        Rig { state: Arc::new(test_state(config)), dir }
    }

    pub fn rig() -> Rig {
        rig_with(|_| {})
    }

    impl Rig {
        pub async fn send(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
            let res = crate::router(self.state.clone()).oneshot(req).await.expect("infallible");
            let (status, headers) = (res.status(), res.headers().clone());
            (status, headers, to_bytes(res.into_body(), 64 << 20).await.expect("body").to_vec())
        }

        /// The raw authenticated reply: status, headers, body bytes.
        pub async fn authed_raw(&self, path: &str, plain: &Value) -> (StatusCode, HeaderMap, Vec<u8>) {
            let req = Request::builder().uri("/v1/challenge").header("x-aurelis-companion", "1").body(Body::empty()).unwrap();
            let (s, _, b) = self.send(req).await;
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
            self.send(req).await
        }

        /// A sealed JSON reply, opened.
        pub async fn authed(&self, path: &str, plain: &Value) -> (StatusCode, Value) {
            let (status, _, body) = self.authed_raw(path, plain).await;
            let opened = seal::open(&body, &SECRET).unwrap_or(body);
            (status, serde_json::from_slice(&opened).unwrap_or(Value::Null))
        }

        pub async fn wait_idle(&self) {
            for _ in 0..500 {
                if self.state.guides.is_idle() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("a fetch never finished");
        }
    }

    /// Opens a v4 stream body and returns its plaintext; panics if it is truncated or tampered.
    pub fn open_stream(body: &[u8]) -> Vec<u8> {
        assert!(body.len() >= companion_protocol::stream::HEADER_BYTES, "no stream header");
        let (header, mut rest) = body.split_at(companion_protocol::stream::HEADER_BYTES);
        let mut opener = StreamOpener::new(&SECRET, header).expect("a v4 header");
        let mut out = Vec::new();
        while !rest.is_empty() {
            let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            let (frame, tail) = rest.split_at(4 + len);
            rest = tail;
            out.extend(opener.open_frame(frame, rest.is_empty()).expect("a frame that opens"));
        }
        assert!(opener.is_finished(), "the stream had no last frame");
        out
    }

    /// A tiny provider on 127.0.0.1.
    #[derive(Default)]
    pub struct OriginState {
        pub guide_hits: AtomicUsize,
        pub picture_hits: AtomicUsize,
        pub fail: AtomicBool,
        pub conditionals: Mutex<Vec<Option<String>>>,
        pub guide_body: Mutex<Vec<u8>>,
        /// The ETag the guide carries; a test changes it with the body so a refetch is not a 304.
        pub etag: Mutex<String>,
    }

    pub async fn origin() -> (String, Arc<OriginState>) {
        let st = Arc::new(OriginState::default());
        *st.guide_body.lock().unwrap() = b"<tv><channel id=\"a\"/></tv>".to_vec();
        *st.etag.lock().unwrap() = "\"v1\"".to_string();
        async fn guide(AxState(st): AxState<Arc<OriginState>>, headers: axum::http::HeaderMap) -> Response {
            st.guide_hits.fetch_add(1, Ordering::SeqCst);
            st.conditionals.lock().unwrap().push(headers.get("if-none-match").and_then(|v| v.to_str().ok()).map(str::to_string));
            if st.fail.load(Ordering::SeqCst) {
                return (StatusCode::SERVICE_UNAVAILABLE, "provider is down for maintenance").into_response();
            }
            let etag = st.etag.lock().unwrap().clone();
            if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
                return StatusCode::NOT_MODIFIED.into_response();
            }
            let body = st.guide_body.lock().unwrap().clone();
            ([(header::ETAG, etag)], body).into_response()
        }
        async fn picture(AxState(st): AxState<Arc<OriginState>>) -> Response {
            st.picture_hits.fetch_add(1, Ordering::SeqCst);
            ([(header::CONTENT_TYPE, "image/png")], vec![0x89u8, b'P', b'N', b'G', 1, 2, 3]).into_response()
        }
        async fn page() -> Response {
            ([(header::CONTENT_TYPE, "text/html")], "<html>not a picture</html>").into_response()
        }
        async fn redirect_to(to: &'static str) -> Response {
            (StatusCode::FOUND, [(header::LOCATION, to)]).into_response()
        }
        let app = Router::new()
            .route("/redir-live", get(|| redirect_to("/live/u/p/1.xml")))
            .route("/redir-m3u8", get(|| redirect_to("/hop/playlist.m3u8")))
            .route("/redir-ftp", get(|| redirect_to("ftp://elsewhere.example/epg.xml")))
            .route("/redir-ok", get(|| redirect_to("/epg.xml")))
            .route("/video", get(|| async { ([(header::CONTENT_TYPE, "video/mp2t")], "not a guide").into_response() }))
            .route("/playlist", get(|| async { ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], "#EXTM3U").into_response() }))
            .route("/gz.xml", get(|| async {
                use std::io::Write;
                let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                z.write_all(b"<tv><channel id=\"gz\"/></tv>").unwrap();
                ([(header::CONTENT_ENCODING, "gzip")], z.finish().unwrap()).into_response()
            }))
            .route("/leaky-fail", get(|| async { (StatusCode::FORBIDDEN, "bad login for username=bob&password=hunter2").into_response() }))
            .route("/echo-fail", get(|| async { (StatusCode::FORBIDDEN, "unknown user bob-secret-login").into_response() }))
            .route("/epg.xml", get(guide)).route("/pic.png", get(picture)).route("/page", get(page)).with_state(st.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), st)
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    fn reg(id: &str, url: &str) -> Value {
        json!({"sources": [{"id": id, "url": url, "refreshMinutes": 60}]})
    }

    async fn register_and_wait(r: &Rig, id: &str, url: &str) {
        let (s, v) = r.authed("/v1/guide/register", &reg(id, url)).await;
        assert_eq!((s, &v["registered"]), (StatusCode::OK, &json!(1)), "{v}");
        r.wait_idle().await;
    }

    #[tokio::test]
    async fn registered_guide_is_fetched_once_and_served_to_two_readers() {
        let (base, origin) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml?username=bob&password=hunter2")).await;
        assert_eq!(origin.guide_hits.load(Ordering::SeqCst), 1);
        for _ in 0..2 {
            let (s, headers, body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1"})).await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(open_stream(&body), b"<tv><channel id=\"a\"/></tv>");
            assert_eq!(headers["x-source-status"], "ok");
            let at: i64 = headers["x-fetched-at-ms"].to_str().unwrap().parse().unwrap();
            assert!((crate::state::now_ms() - at).abs() < 60_000, "fetched-at is a current stamp: {at}");
        }
        assert_eq!(origin.guide_hits.load(Ordering::SeqCst), 1, "serving never refetches");
        assert!(r.state.card().disk.used_mb.guide >= 1);
    }

    const THREE: &str = r#"<tv>
      <channel id="bbc1"><icon src="http://logo.test/bbc1.png"/></channel>
      <programme start="20260101120000 +0000" stop="20260101130000 +0000" channel="bbc1"><title>One</title></programme>
      <programme start="20260101130000 +0000" stop="20260101140000 +0000" channel="bbc1"><title>Two</title></programme>
      <programme start="20260101140000 +0000" stop="20260101150000 +0000" channel="bbc1"><title>Three</title></programme>
    </tv>"#;

    fn held(r: &Rig, id: &str) -> Meta {
        r.state.stores.guide.meta(&guide_key(id)).expect("the guide is held")
    }

    fn artefact_of(r: &Rig, id: &str) -> PathBuf {
        crate::store::artefact_path(&r.state.stores.guide.path_of(&guide_key(id)))
    }

    async fn parsed_rig() -> Rig {
        let (base, origin) = origin().await;
        *origin.guide_body.lock().unwrap() = THREE.as_bytes().to_vec();
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        assert!(held(&r, "src-1").parsed.is_some());
        r
    }

    fn why(headers: &axum::http::HeaderMap) -> String {
        headers.get("x-parsed-why").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
    }

    #[tokio::test]
    async fn parsed_when_logic_matches() {
        use std::io::BufRead;
        let r = parsed_rig().await;
        let (s, headers, body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1", "parsedLogic": xmltv_parser::LOGIC_VERSION})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(headers["x-guide-form"], "parsed");
        assert_eq!(headers["x-parsed-entries"], "3");
        assert!(headers.get("x-parsed-why").is_none());
        let stamp: i64 = headers["x-fetched-at-ms"].to_str().unwrap().parse().unwrap();
        assert_eq!(stamp, held(&r, "src-1").stored_at_ms, "the stamp is the body's");
        let plain = open_stream(&body);
        let mut first = String::new();
        std::io::BufReader::new(flate2::read::GzDecoder::new(&plain[..])).read_line(&mut first).unwrap();
        assert_eq!(first.trim_end(), crate::guide_parse::MAGIC);
    }

    #[tokio::test]
    async fn parsed_needs_the_same_logic() {
        let r = parsed_rig().await;
        let other = "0".repeat(64);
        let (s, headers, body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1", "parsedLogic": other})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(headers["x-guide-form"], "xml");
        assert!(why(&headers).contains("different parser version"), "{}", why(&headers));
        assert!(open_stream(&body).starts_with(b"<tv>"));
        // A malformed version is a different version, not a refusal.
        let (s, headers, _) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1", "parsedLogic": "ABC"})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(headers["x-guide-form"], "xml");
        assert!(why(&headers).contains("different parser version"), "{}", why(&headers));
    }

    #[tokio::test]
    async fn no_logic_means_xml_as_before() {
        let r = parsed_rig().await;
        let (s, headers, body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1"})).await;
        assert_eq!(s, StatusCode::OK);
        assert!(headers.get("x-parsed-why").is_none() && headers.get("x-parsed-entries").is_none());
        assert_eq!(headers["x-guide-form"], "xml");
        assert_eq!(open_stream(&body), THREE.as_bytes());
    }

    #[tokio::test]
    async fn still_parsing_serves_xml() {
        let r = parsed_rig().await;
        r.state.stores.guide.edit_meta(&guide_key("src-1"), |m| {
            m.parsed = None;
            m.parse_status = "parsing".to_string();
        });
        let (_, headers, body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1", "parsedLogic": xmltv_parser::LOGIC_VERSION})).await;
        assert_eq!(headers["x-guide-form"], "xml");
        assert_eq!(why(&headers), "still parsing");
        assert_eq!(open_stream(&body), THREE.as_bytes());
    }

    #[tokio::test]
    async fn not_parsed_says_why_and_a_missing_artefact_is_not_a_500() {
        let r = parsed_rig().await;
        let want = json!({"id": "src-1", "parsedLogic": xmltv_parser::LOGIC_VERSION});
        std::fs::remove_file(artefact_of(&r, "src-1")).unwrap();
        let (s, headers, body) = r.authed_raw("/v1/guide/get", &want).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(headers["x-guide-form"], "xml");
        assert!(why(&headers).contains("parsed copy is missing"), "{}", why(&headers));
        assert_eq!(open_stream(&body), THREE.as_bytes());
        r.state.stores.guide.edit_meta(&guide_key("src-1"), |m| {
            m.parsed = None;
            m.parse_status = "not parsed: needs 1290 MB free, 850 MB free".to_string();
        });
        let (_, headers, _) = r.authed_raw("/v1/guide/get", &want).await;
        assert_eq!(why(&headers), "not parsed: needs 1290 MB free, 850 MB free");
    }

    #[tokio::test]
    async fn parse_follows_a_fetch() {
        let (base, origin) = origin().await;
        *origin.guide_body.lock().unwrap() = THREE.as_bytes().to_vec();
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml?username=bob&password=hunter2")).await;
        let meta = held(&r, "src-1");
        let parsed = meta.parsed.expect("parsed after the fetch");
        assert_eq!(parsed.entries, 3);
        assert_eq!(parsed.source_id, "src-1");
        assert!(meta.parse_status.starts_with("parsed 3 programmes in "), "{}", meta.parse_status);
        let artefact = artefact_of(&r, "src-1");
        assert!(artefact.is_file());
        let (read, _) = crate::guide_parse::read_parsed(&artefact).unwrap();
        assert_eq!(read.entries, 3);
        let body_len = std::fs::metadata(r.state.stores.guide.path_of(&guide_key("src-1"))).unwrap().len();
        let artefact_len = std::fs::metadata(&artefact).unwrap().len();
        assert_eq!(r.state.stores.guide.used_bytes(), body_len + artefact_len, "the artefact counts as guide disk use");
    }

    #[tokio::test]
    async fn a_guide_too_big_for_the_free_memory_says_why_and_writes_nothing() {
        let (base, origin) = origin().await;
        *origin.guide_body.lock().unwrap() = THREE.as_bytes().to_vec();
        let r = rig();
        let mut facts = r.state.facts();
        facts.free_ram_mb = 800;
        r.state.update_facts(facts);
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        let meta = held(&r, "src-1");
        assert!(meta.parsed.is_none());
        assert_eq!(meta.parse_status, "not parsed: needs 1024 MB free, 800 MB free");
        assert!(!artefact_of(&r, "src-1").exists());
        // The memory comes back: the next refresh of the unchanged guide parses it.
        let mut facts = r.state.facts();
        facts.free_ram_mb = 2048;
        r.state.update_facts(facts);
        refresh(&r.state, "src-1").await;
        r.wait_idle().await;
        assert_eq!(held(&r, "src-1").parsed.map(|p| p.entries), Some(3));
    }

    #[tokio::test]
    async fn a_guide_that_is_not_a_guide_records_the_parsers_reason() {
        let (base, _origin) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        let meta = held(&r, "src-1");
        assert!(meta.parsed.is_none());
        assert_eq!(meta.parse_status, "not parsed: the guide parsed to no programmes");
        assert!(!artefact_of(&r, "src-1").exists());
    }

    #[tokio::test]
    async fn refetch_drops_the_old_parse() {
        let (base, origin) = origin().await;
        *origin.guide_body.lock().unwrap() = THREE.as_bytes().to_vec();
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        let a = held(&r, "src-1").parsed.expect("A parsed");
        let two = THREE.replace("<title>Three</title>", "<title>Three</title><desc>changed</desc>").replace("<title>Two</title>", "<title>Deux</title>");
        *origin.guide_body.lock().unwrap() = two.as_bytes().to_vec();
        *origin.etag.lock().unwrap() = "\"v2\"".to_string();
        // Hold the parse gate so the old artefact can be seen gone before B is parsed.
        let gate = r.state.guides.parse_gate.acquire().await.unwrap();
        refresh(&r.state, "src-1").await;
        assert!(!artefact_of(&r, "src-1").exists(), "the old artefact went with the old body");
        let mid = held(&r, "src-1");
        assert!(mid.parsed.is_none() && mid.parse_status == "parsing", "{mid:?}");
        assert_eq!(r.state.stores.guide.used_bytes(), std::fs::metadata(r.state.stores.guide.path_of(&guide_key("src-1"))).unwrap().len());
        drop(gate);
        r.wait_idle().await;
        let b = held(&r, "src-1").parsed.expect("B parsed");
        assert_ne!(b.body_sha256, a.body_sha256);
        let (read, _) = crate::guide_parse::read_parsed(&artefact_of(&r, "src-1")).unwrap();
        assert_eq!(read.body_sha256, b.body_sha256);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_parse_finishing_while_the_next_body_lands_is_never_recorded() {
        let (base, origin) = origin().await;
        *origin.guide_body.lock().unwrap() = THREE.as_bytes().to_vec();
        let r = rig();
        // Parse A reads body A, then waits.
        let release = r.state.guides.parse_release.clone();
        release.store(false, Ordering::SeqCst);
        let (s, v) = r.authed("/v1/guide/register", &reg("src-1", &format!("{base}/epg.xml"))).await;
        assert_eq!((s, &v["registered"]), (StatusCode::OK, &json!(1)), "{v}");
        for _ in 0..500 {
            if r.state.guides.parse_running.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(r.state.guides.parse_running.load(Ordering::SeqCst), 1, "parse A is under way");
        let sha_a = crate::guide_parse::file_sha256(&r.state.stores.guide.path_of(&guide_key("src-1"))).unwrap();
        // Body B arrives; B's parse will be declined for memory.
        let two = THREE.replace("<title>Two</title>", "<title>Deux</title>");
        *origin.guide_body.lock().unwrap() = two.as_bytes().to_vec();
        *origin.etag.lock().unwrap() = "\"v2\"".to_string();
        let mut facts = r.state.facts();
        facts.free_ram_mb = 800;
        r.state.update_facts(facts);
        // The moment B has landed, let A finish, and see what the meta says about A.
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let (st, seen2) = (r.state.clone(), seen.clone());
        *r.state.guides.land_hook.lock().unwrap() = Some(Box::new(move || {
            release.store(true, Ordering::SeqCst);
            for _ in 0..300 {
                if st.guides.parse_running.load(Ordering::SeqCst) == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(50));
            seen2.lock().unwrap().push(st.stores.guide.meta(&guide_key("src-1")).and_then(|m| m.parsed).map(|p| p.body_sha256));
        }));
        refresh(&r.state, "src-1").await;
        r.wait_idle().await;
        let sha_b = crate::guide_parse::file_sha256(&r.state.stores.guide.path_of(&guide_key("src-1"))).unwrap();
        assert_ne!(sha_a, sha_b);
        for s in seen.lock().unwrap().iter() {
            assert_ne!(s.as_deref(), Some(sha_a.as_str()), "A's summary was recorded against body B");
        }
        let meta = held(&r, "src-1");
        assert!(meta.parsed.is_none(), "{meta:?}");
        assert_eq!(meta.parse_status, "not parsed: needs 1024 MB free, 800 MB free");
        assert!(!artefact_of(&r, "src-1").exists(), "no artefact survives beside body B");
        assert_eq!(r.state.stores.guide.used_bytes(), std::fs::metadata(r.state.stores.guide.path_of(&guide_key("src-1"))).unwrap().len());
    }

    #[tokio::test]
    async fn an_old_logic_parse_is_redone_on_a_304() {
        let r = parsed_rig().await;
        r.state.stores.guide.edit_meta(&guide_key("src-1"), |m| {
            if let Some(p) = m.parsed.as_mut() {
                p.logic = "old-logic".to_string();
            }
        });
        refresh(&r.state, "src-1").await; // the origin answers 304
        r.wait_idle().await;
        let meta = held(&r, "src-1");
        assert_eq!(meta.parsed.as_ref().map(|p| p.logic.as_str()), Some(xmltv_parser::LOGIC_VERSION), "{meta:?}");
        assert!(meta.parse_status.starts_with("parsed 3 programmes in "), "{}", meta.parse_status);
        assert!(artefact_of(&r, "src-1").is_file());
    }

    #[tokio::test]
    async fn a_missing_artefact_is_repaired_on_a_304() {
        let r = parsed_rig().await;
        std::fs::remove_file(artefact_of(&r, "src-1")).unwrap();
        assert!(held(&r, "src-1").parsed.is_some(), "the meta still claims a parse");
        refresh(&r.state, "src-1").await; // the origin answers 304
        r.wait_idle().await;
        assert!(artefact_of(&r, "src-1").is_file(), "re-parsed");
        assert_eq!(held(&r, "src-1").parsed.map(|p| p.entries), Some(3));
        let body_len = std::fs::metadata(r.state.stores.guide.path_of(&guide_key("src-1"))).unwrap().len();
        let artefact_len = std::fs::metadata(artefact_of(&r, "src-1")).unwrap().len();
        assert_eq!(r.state.stores.guide.used_bytes(), body_len + artefact_len, "counted once");
    }

    #[tokio::test]
    async fn a_current_parse_is_not_redone_on_a_304() {
        let r = parsed_rig().await;
        let before = held(&r, "src-1").parsed.expect("parsed");
        refresh(&r.state, "src-1").await;
        r.wait_idle().await;
        assert_eq!(held(&r, "src-1").parsed.map(|p| p.parse_ms), Some(before.parse_ms));
        assert_eq!(r.state.guides.lock().body_gen.get("src-1").copied(), Some(1), "no new parse generation");
    }

    #[tokio::test]
    async fn a_restart_with_parsing_persisted_clears_it_then_reparses() {
        let r = parsed_rig().await;
        // What a Companion stopped mid-parse leaves on disk: no summary, "parsing", nothing running.
        r.state.stores.guide.drop_artefact(&guide_key("src-1")).await;
        r.state.stores.guide.edit_meta(&guide_key("src-1"), |m| {
            m.parsed = None;
            m.parse_status = "parsing".to_string();
        });
        assert_eq!(clear_stale_parsing(&r.state), 1);
        assert_eq!(held(&r, "src-1").parse_status, "", "a status no parse stands behind is not reported");
        startup_sweep(r.state.clone()).await;
        r.wait_idle().await;
        let meta = held(&r, "src-1");
        assert_eq!(meta.parsed.map(|p| p.entries), Some(3));
        assert!(meta.parse_status.starts_with("parsed 3 programmes in "), "{}", meta.parse_status);
    }

    #[tokio::test]
    async fn the_startup_sweep_parses_held_guides_that_need_it_and_leaves_the_rest() {
        let (base, origin) = origin().await;
        *origin.guide_body.lock().unwrap() = THREE.as_bytes().to_vec();
        let r = rig();
        let three = json!({"sources": [
            {"id": "src-1", "url": format!("{base}/epg.xml?a=1"), "refreshMinutes": 60},
            {"id": "src-2", "url": format!("{base}/epg.xml?a=2"), "refreshMinutes": 60},
            {"id": "src-3", "url": format!("{base}/epg.xml?a=3"), "refreshMinutes": 60}]});
        let (s, v) = r.authed("/v1/guide/register", &three).await;
        assert_eq!((s, &v["registered"]), (StatusCode::OK, &json!(3)), "{v}");
        r.wait_idle().await;
        // src-1: held but never parsed; src-2: parsed by an older parser; src-3: current.
        r.state.stores.guide.drop_artefact(&guide_key("src-1")).await;
        r.state.stores.guide.edit_meta(&guide_key("src-1"), |m| {
            m.parsed = None;
            m.parse_status = String::new();
        });
        r.state.stores.guide.edit_meta(&guide_key("src-2"), |m| m.parsed.as_mut().unwrap().logic = "old-logic".to_string());
        let src3 = held(&r, "src-3").parsed.unwrap();
        startup_sweep(r.state.clone()).await;
        r.wait_idle().await;
        for id in ["src-1", "src-2"] {
            let meta = held(&r, id);
            assert_eq!(meta.parsed.as_ref().map(|p| p.logic.as_str()), Some(xmltv_parser::LOGIC_VERSION), "{id}: {meta:?}");
            assert!(artefact_of(&r, id).is_file(), "{id}");
        }
        assert_eq!(held(&r, "src-3").parsed.map(|p| p.parse_ms), Some(src3.parse_ms), "a current parse is left alone");
        assert_eq!(r.state.guides.parse_overlaps.load(Ordering::SeqCst), 0, "through the one-permit gate");
        assert_eq!(origin.guide_hits.load(Ordering::SeqCst), 3, "the sweep parses what is held; it fetches nothing");
    }

    #[tokio::test]
    async fn one_parse_at_a_time() {
        let (base, origin) = origin().await;
        *origin.guide_body.lock().unwrap() = THREE.as_bytes().to_vec();
        let r = rig();
        let both = json!({"sources": [
            {"id": "src-1", "url": format!("{base}/epg.xml?a=1"), "refreshMinutes": 60},
            {"id": "src-2", "url": format!("{base}/epg.xml?a=2"), "refreshMinutes": 60},
            {"id": "src-3", "url": format!("{base}/epg.xml?a=3"), "refreshMinutes": 60}]});
        let (s, v) = r.authed("/v1/guide/register", &both).await;
        assert_eq!((s, &v["registered"]), (StatusCode::OK, &json!(3)), "{v}");
        r.wait_idle().await;
        for id in ["src-1", "src-2", "src-3"] {
            assert_eq!(held(&r, id).parsed.map(|p| p.entries), Some(3), "{id}");
        }
        assert_eq!(r.state.guides.parse_overlaps.load(Ordering::SeqCst), 0, "parses never overlapped");
    }

    #[tokio::test]
    async fn the_status_page_shows_each_guides_parse() {
        let (base, origin) = origin().await;
        *origin.guide_body.lock().unwrap() = THREE.as_bytes().to_vec();
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        let html = crate::status_page::render(&r.state.page_view(), true);
        assert!(html.contains("<th>Parsed</th>") && html.contains("parsed 3 programmes in "), "{html}");
    }

    #[tokio::test]
    async fn a_gzip_encoded_guide_is_stored_decompressed() {
        let (base, _origin) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-gz", &format!("{base}/gz.xml")).await;
        let (s, _h, body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-gz"})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(open_stream(&body), b"<tv><channel id=\"gz\"/></tv>", "plain XML, as OkHttp would deliver it");
    }

    #[tokio::test]
    async fn a_failure_status_never_carries_a_login() {
        let (base, _origin) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        // First a good fetch so a body is held, then point the same id at a refusing URL.
        let (s, v) = r.authed("/v1/guide/register", &reg("src-1", &format!("{base}/leaky-fail"))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        r.wait_idle().await;
        // A stream body is not JSON: read the header the television reads, not a parsed body.
        let (s, headers, _body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1"})).await;
        assert_eq!(s, StatusCode::OK, "the last good body is still held");
        let status = headers["x-source-status"].to_str().unwrap().to_string();
        assert!(status.starts_with("failed "), "the failed fetch is what the status says: {status}");
        assert!(status.contains("HTTP 403"), "{status}");
        assert!(!status.contains("hunter2") && !status.contains("bob") && !status.contains("password"), "{status}");
        assert_eq!(scrub_secrets("x Password=abc"), "x [login removed]");
    }

    #[tokio::test]
    async fn refetch_sends_if_none_match_and_keeps_body_on_304() {
        let (base, origin) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        let before: i64 = r.authed_raw("/v1/guide/get", &json!({"id": "src-1"})).await.1["x-fetched-at-ms"].to_str().unwrap().parse().unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        refresh(&r.state, "src-1").await;
        assert_eq!(origin.conditionals.lock().unwrap().as_slice(), &[None, Some("\"v1\"".to_string())]);
        let (s, headers, body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1"})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(open_stream(&body), b"<tv><channel id=\"a\"/></tv>", "the 304 keeps the body");
        let after: i64 = headers["x-fetched-at-ms"].to_str().unwrap().parse().unwrap();
        assert!(after > before, "a 304 updates the fetched time ({before} -> {after})");
        assert_eq!(headers["x-source-status"], "ok");
    }

    #[tokio::test]
    async fn provider_failure_keeps_last_body_and_reports_status() {
        let (base, origin) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml?password=hunter2")).await;
        origin.fail.store(true, Ordering::SeqCst);
        refresh(&r.state, "src-1").await;
        let (s, headers, body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1"})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(open_stream(&body), b"<tv><channel id=\"a\"/></tv>", "the last good body is kept");
        let status = headers["x-source-status"].to_str().unwrap().to_string();
        assert!(status.starts_with("failed 20") && status.contains("Z: HTTP 503 provider is down"), "{status}");
        assert!(!status.contains("hunter2") && !status.contains("127.0.0.1"), "{status}");
        // The provider recovers: status is ok again.
        origin.fail.store(false, Ordering::SeqCst);
        refresh(&r.state, "src-1").await;
        assert_eq!(r.authed_raw("/v1/guide/get", &json!({"id": "src-1"})).await.1["x-source-status"], "ok");
    }

    #[tokio::test]
    async fn an_unreachable_provider_reports_an_io_error_without_the_url() {
        let r = rig();
        // Nothing listens on port 9 of loopback.
        register_and_wait(&r, "src-1", "http://127.0.0.1:9/epg.xml?password=hunter2").await;
        let (s, v) = r.authed("/v1/guide/get", &json!({"id": "src-1"})).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let reason = v["reason"].as_str().unwrap();
        assert!(reason.contains("last fetch failed: 20") && !reason.contains("hunter2"), "{reason}");
    }

    #[tokio::test]
    async fn refuses_stream_like_urls() {
        // The rule itself, on every shape (PBKDF2 makes each sealed round trip slow in a debug build).
        for url in [
            "http://p.example/a/playlist.m3u8?password=hunter2",
            "http://p.example/segment.TS?password=hunter2",
            "http://p.example/vid.mp4",
            "http://p.example/vid.mkv",
            "http://p.example/live/user/hunter2/1.xml",
            "http://p.example/movie/user/hunter2/1.xml",
            "http://p.example/series/user/hunter2/1.xml",
            "http://p.example/timeshift/user/hunter2/1.xml",
            "ftp://p.example/epg.xml",
            "not a url",
        ] {
            let reason = validate_source(&Source { name: String::new(), id: "src-9".into(), url: url.into(), refresh_minutes: 60 }).expect_err(url);
            assert!(reason.contains("src-9"), "{url}: {reason}");
            assert!(!reason.contains("hunter2") && !reason.contains("p.example"), "the reason never repeats the url: {reason}");
        }
        assert!(validate_source(&Source { name: String::new(), id: "ok".into(), url: "https://p.example/xmltv.php?username=a&password=b".into(), refresh_minutes: 60 }).is_ok());
        // Through the route: a refusal is a sealed 400 naming the source, and saves nothing.
        let r = rig();
        let (s, v) = r.authed("/v1/guide/register", &reg("src-9", "http://p.example/a/playlist.m3u8?password=hunter2")).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["registered"], json!(0), "{v}");
        let skipped = v["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1, "{v}");
        let reason = skipped[0]["reason"].as_str().unwrap();
        assert_eq!(skipped[0]["id"], json!("src-9"));
        assert!(reason.contains("src-9") && !reason.contains("hunter2") && !reason.contains("p.example"), "{reason}");
        assert!(r.state.guides.source("src-9").is_none());
        // A good source beside a bad one: the good one is registered, the bad one is reported.
        let both = json!({"sources": [{"id": "ok", "url": "http://p.example/epg.xml", "refreshMinutes": 60}, {"id": "bad", "url": "http://p.example/x.ts", "refreshMinutes": 60}, {"id": "ok", "url": "http://p.example/again.xml", "refreshMinutes": 60}]});
        let (s, v) = r.authed("/v1/guide/register", &both).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["registered"], json!(1), "{v}");
        let ids: Vec<&str> = v["skipped"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["bad", "ok"], "the bad address and the repeated id are skipped: {v}");
        assert_eq!(r.state.guides.source("ok").unwrap().url, "http://p.example/epg.xml", "the first of a repeated id stands");
        assert!(r.state.guides.source("bad").is_none());
    }

    #[tokio::test]
    async fn guide_get_unknown_id_is_404_with_reason() {
        let r = rig();
        let (s, v) = r.authed("/v1/guide/get", &json!({"id": "nope"})).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(v["reason"].as_str().unwrap().contains("'nope'"), "{v}");
        assert_eq!(r.authed("/v1/guide/get", &json!({})).await.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn busy_refuses_new_fetches_but_serves_held() {
        let (base, origin) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        r.state.set_paused(true);
        refresh(&r.state, "src-1").await;
        let (s, _) = r.authed("/v1/guide/register", &json!({"sources": [
            {"id": "src-1", "url": format!("{base}/epg.xml"), "refreshMinutes": 60},
            {"id": "src-2", "url": format!("{base}/epg.xml?other=1"), "refreshMinutes": 60}]})).await;
        assert_eq!(s, StatusCode::OK);
        r.wait_idle().await;
        assert_eq!(origin.guide_hits.load(Ordering::SeqCst), 1, "no fetch while busy, new source included");
        let (s, _, body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1"})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(open_stream(&body), b"<tv><channel id=\"a\"/></tv>", "held guides are still served");
        let (s, v) = r.authed("/v1/guide/get", &json!({"id": "src-2"})).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
        r.state.set_paused(false);
        refresh(&r.state, "src-2").await;
        assert_eq!(origin.guide_hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn registration_is_saved_private_and_reloads_and_a_new_set_replaces_the_old() {
        let (base, _) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml?password=hunter2")).await;
        let path = r.dir.0.join(SOURCES_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let reloaded = GuideSources::load(&r.dir.0);
        assert_eq!(reloaded.source("src-1").map(|s| s.refresh_minutes), Some(60));
        // A new registration replaces the set; the dropped source's body stays until evicted.
        register_and_wait(&r, "src-2", &format!("{base}/epg.xml?second=1")).await;
        assert!(r.state.guides.source("src-1").is_none());
        let (s, _, _) = r.authed_raw("/v1/guide/get", &json!({"id": "src-1"})).await;
        assert_eq!(s, StatusCode::OK, "a dropped source's stored file stays");
        assert!(r.state.guides.due(i64::MAX / 2, &|_| None).iter().all(|id| id != "src-1"), "a dropped source is not refreshed");
    }

    #[tokio::test]
    async fn due_follows_refresh_minutes_and_a_fresh_held_guide_is_not_refetched_after_restart() {
        let (base, _) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        let now = crate::state::now_ms();
        let held = |_: &str| Some(now);
        assert!(r.state.guides.due(now + 59 * 60_000, &held).is_empty());
        assert_eq!(r.state.guides.due(now + 61 * 60_000, &held), vec!["src-1".to_string()]);
        let fresh = GuideSources::load(&r.dir.0);
        assert!(fresh.due(now, &|_| Some(now - 10 * 60_000)).is_empty(), "held 10 minutes, due in an hour");
        assert_eq!(fresh.due(now, &|_| Some(now - 2 * 3_600_000)), vec!["src-1".to_string()]);
        assert_eq!(fresh.due(now, &|_| None), vec!["src-1".to_string()], "nothing held: due now");
    }

    #[tokio::test]
    async fn a_guide_that_is_not_held_is_due_now_whatever_the_last_attempt_was() {
        let (base, _) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        let now = crate::state::now_ms();
        // Evicted right after a good fetch: not hammered inside 15 minutes, then due, long before its hour.
        assert!(r.state.guides.due(now + 30_000, &|_| None).is_empty());
        assert!(r.state.guides.due(now + 14 * 60_000, &|_| None).is_empty());
        assert_eq!(r.state.guides.due(now + 16 * 60_000, &|_| None), vec!["src-1".to_string()]);
    }

    #[tokio::test]
    async fn a_failing_source_backs_off_10_minutes_doubling_to_6_hours_and_success_resets_it() {
        let (base, origin) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        origin.fail.store(true, Ordering::SeqCst);
        let backoff = |r: &Rig| r.state.guides.lock().runtime.get("src-1").map(|x| x.backoff_ms).unwrap();
        let mut seen = Vec::new();
        for _ in 0..8 {
            refresh(&r.state, "src-1").await;
            seen.push(backoff(&r) / 60_000);
        }
        assert_eq!(seen, vec![10, 20, 40, 80, 160, 320, 360, 360], "10 min, doubling, capped at 6 h");
        let now = crate::state::now_ms();
        assert!(r.state.guides.due(now + 359 * 60_000, &|_| None).is_empty(), "not before the back-off is up, even with nothing held");
        assert_eq!(r.state.guides.due(now + 361 * 60_000, &|_| None), vec!["src-1".to_string()]);
        origin.fail.store(false, Ordering::SeqCst);
        refresh(&r.state, "src-1").await;
        assert_eq!(backoff(&r), 0, "a success resets it");
    }

    #[tokio::test]
    async fn get_says_which_reason_a_registered_guide_is_not_held() {
        let (base, origin) = origin().await;
        let r = rig();
        origin.fail.store(true, Ordering::SeqCst);
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        let (s, v) = r.authed("/v1/guide/get", &json!({"id": "src-1"})).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(v["reason"].as_str().unwrap().contains("last fetch failed: 20") && v["reason"].as_str().unwrap().contains("HTTP 503"), "{v}");
        origin.fail.store(false, Ordering::SeqCst);
        refresh(&r.state, "src-1").await;
        // Evict it: the guide is registered, was fetched, and is gone.
        let hash = sha_hex(&guide_key("src-1"));
        let path = r.dir.0.join("guide").join(&hash[..2]).join(&hash);
        assert!(path.is_file());
        r.state.stores.guide.forget(&guide_key("src-1"), "test eviction");
        let (_, v) = r.authed("/v1/guide/get", &json!({"id": "src-1"})).await;
        assert!(v["reason"].as_str().unwrap().contains("evicted, refetching"), "{v}");
        // Registered, never attempted (the Companion was busy): the first download is not finished.
        r.state.set_paused(true);
        let both = json!({"sources": [{"id": "src-1", "url": format!("{base}/epg.xml"), "refreshMinutes": 60}, {"id": "src-3", "url": format!("{base}/epg.xml?x=1"), "refreshMinutes": 60}]});
        r.authed("/v1/guide/register", &both).await;
        r.wait_idle().await;
        let (_, v) = r.authed("/v1/guide/get", &json!({"id": "src-3"})).await;
        assert!(v["reason"].as_str().unwrap().contains("first download has not finished"), "{v}");
    }

    #[tokio::test]
    async fn a_redirect_to_a_stream_or_another_scheme_is_stopped_naming_the_source_not_the_url() {
        let (base, origin) = origin().await;
        let r = rig();
        for path in ["redir-live", "redir-m3u8", "redir-ftp"] {
            let (s, v) = r.authed("/v1/guide/register", &reg("src-1", &format!("{base}/{path}?password=hunter2"))).await;
            assert_eq!((s, &v["registered"]), (StatusCode::OK, &json!(1)), "{v}");
            r.wait_idle().await;
            let status = r.state.guides.status_of("src-1").expect("a status");
            assert!(status.starts_with("failed 20") && status.contains("source 'src-1'") && status.contains("refused"), "{path}: {status}");
            assert!(!status.contains("hunter2") && !status.contains("127.0.0.1") && !status.contains("playlist.m3u8"), "{path}: {status}");
            let (s, _) = r.authed("/v1/guide/get", &json!({"id": "src-1"})).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "nothing was stored from a refused redirect");
            // Re-register the same id with a changed url next round, so it is fetched afresh.
        }
        assert_eq!(origin.guide_hits.load(Ordering::SeqCst), 0);
        // A harmless redirect still works.
        let (s, _) = r.authed("/v1/guide/register", &reg("src-2", &format!("{base}/redir-ok"))).await;
        assert_eq!(s, StatusCode::OK);
        r.wait_idle().await;
        let (s, _, body) = r.authed_raw("/v1/guide/get", &json!({"id": "src-2"})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(open_stream(&body), b"<tv><channel id=\"a\"/></tv>");
    }

    #[tokio::test]
    async fn a_video_or_playlist_reply_is_never_stored_as_a_guide() {
        let (base, _) = origin().await;
        let r = rig();
        for path in ["video", "playlist"] {
            r.authed("/v1/guide/register", &reg("src-1", &format!("{base}/{path}"))).await;
            r.wait_idle().await;
            let status = r.state.guides.status_of("src-1").expect("a status");
            assert!(status.contains("source 'src-1'") && status.contains("not a guide file"), "{path}: {status}");
            assert_eq!(r.authed("/v1/guide/get", &json!({"id": "src-1"})).await.0, StatusCode::NOT_FOUND);
        }
        assert_eq!(r.state.stores.guide.used_bytes(), 0);
    }

    #[tokio::test]
    async fn the_status_keeps_the_providers_words_for_the_header() {
        let (base, _) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/echo-fail")).await;
        let status = r.state.guides.status_of("src-1").unwrap();
        assert!(status.contains("HTTP 403 unknown user bob-secret-login"), "{status}");
    }

    #[test]
    fn stream_like_catches_every_shape_and_spares_guide_files() {
        for path in [
            "/a/playlist.m3u8", "/seg.TS", "/v.mp4", "/v.mkv", "/manifest.mpd", "/clip.flv", "/live/u/p/1", "/movie/u/p/1", "/series/u/p/1", "/timeshift/u/p/1",
            "/user/pass/123", "/user/pass/123.ts", "/user/pass/123.mp4", "/a/b/c/99",
        ] {
            assert!(stream_like(path, UrlKind::Guide).is_some(), "{path} must be refused");
        }
        for path in ["/xmltv.php", "/epg.xml.gz", "/epg.xml", "/guide/xmltv.xml", "/x/epg.xml", "/", "/a/b/guide", "/2026"] {
            assert!(stream_like(path, UrlKind::Guide).is_none(), "{path} must pass");
        }
        assert!(check_fetchable(&reqwest::Url::parse("http://p.example/xmltv.php?username=a&password=b").unwrap(), UrlKind::Guide).is_ok());
    }

    #[test]
    fn the_short_stream_form_is_a_guide_rule_only_pictures_keep_the_other_checks() {
        let ok = |u: &str, k| check_fetchable(&reqwest::Url::parse(u).unwrap(), k);
        assert!(ok("http://p.example/user/pass/123", UrlKind::Guide).is_err());
        assert!(ok("http://p.example/img/abc/123", UrlKind::Picture).is_ok());
        assert!(ok("http://p.example/a/b/c/99.png", UrlKind::Picture).is_ok());
        // Everything else still applies to pictures.
        for u in ["http://p.example/live/u/p/1.png", "http://p.example/seg.ts", "http://p.example/movie/x.png", "ftp://p.example/a.png"] {
            assert!(ok(u, UrlKind::Picture).is_err(), "{u}");
        }
    }

    #[test]
    fn an_evicted_guide_waits_fifteen_minutes_between_fetches() {
        assert_eq!(MIN_REFETCH_GAP_MS, 15 * 60 * 1000);
    }

    #[tokio::test]
    async fn unpair_and_register_are_exclusive_and_a_waiting_registration_cannot_recreate_the_file() {
        let (base, _) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        let path = r.dir.0.join(SOURCES_FILE);
        assert!(path.exists());
        // A clear waits for a registration that holds the lock.
        let guard = r.state.guides.reg_lock.lock().await;
        let state = r.state.clone();
        let clearing = tokio::spawn(async move { state.guides.clear().await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!clearing.is_finished() && path.exists(), "clear waits for the registration in progress");
        drop(guard);
        clearing.await.unwrap().unwrap();
        assert!(!path.exists());
        // A registration that began before the unpair is refused afterwards, and saves nothing.
        let stale = r.state.guides.generation() - 1;
        let e = r.state.guides.register_at(stale, vec![Source { name: String::new(), id: "late".into(), url: format!("{base}/epg.xml"), refresh_minutes: 60 }]).await.unwrap_err();
        assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.reason);
        assert!(!path.exists() && r.state.guides.source("late").is_none());
    }

    #[tokio::test]
    async fn unpair_deletes_the_registrations_file_and_forgets_the_sources() {
        let (base, _) = origin().await;
        let r = rig();
        register_and_wait(&r, "src-1", &format!("{base}/epg.xml")).await;
        assert!(r.dir.0.join(SOURCES_FILE).exists());
        let (s, v) = r.authed("/v1/unpair", &json!({})).await;
        assert_eq!((s, &v["wiped"]), (StatusCode::OK, &json!(true)));
        assert!(!r.dir.0.join(SOURCES_FILE).exists());
        assert!(r.state.guides.source("src-1").is_none());
        assert_eq!(r.state.stores.guide.used_bytes(), 0);
    }

    #[tokio::test]
    async fn a_guide_larger_than_one_frame_streams_and_an_empty_one_is_one_last_frame() {
        let r = rig();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        r.state.stores.guide.put_bytes(&guide_key("big"), &big, Meta::at(5)).await.unwrap();
        r.state.stores.guide.put_bytes(&guide_key("empty"), b"", Meta::at(5)).await.unwrap();
        let (s, _, body) = r.authed_raw("/v1/guide/get", &json!({"id": "big"})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(open_stream(&body), big);
        let (s, _, body) = r.authed_raw("/v1/guide/get", &json!({"id": "empty"})).await;
        assert_eq!(s, StatusCode::OK);
        assert!(open_stream(&body).is_empty());
    }

    #[test]
    fn iso_time_and_log_tags() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_700_000_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso_utc(1_709_164_800_000), "2024-02-29T00:00:00Z");
        let tag = url_tag("http://prov.example:8080/xmltv.php?username=bob&password=hunter2");
        assert!(tag.starts_with("prov.example #") && !tag.contains("hunter2") && tag.len() == "prov.example #".len() + 12, "{tag}");
    }
}
