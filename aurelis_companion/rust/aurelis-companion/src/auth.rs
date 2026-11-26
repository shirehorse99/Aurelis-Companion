//! Who may speak to this Companion: the pairing code, the household secret, single-use nonces and
//! the middleware that checks every authenticated request.

use crate::household::Household;
use crate::state::AppState;
use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use companion_protocol::pairing::{self, COMPANION_CONFIRM, TV_CONFIRM};
use companion_protocol::{proof, seal};
use rand::rngs::OsRng;
use rand::RngCore;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

pub const NONCE_LIFE_MS: u64 = 10_000;
pub const NONCE_CAP: usize = 256;
/// A nonce this young is never evicted to make room.
pub const NONCE_PROTECTED_MS: u64 = 2_000;
pub const NONCE_PER_PEER: usize = 32;
pub const CODE_LIFE_MS: u64 = 10 * 60 * 1000;
pub const MAX_WRONG: u32 = 5;
pub const PENDING_CAP: usize = 8;
/// Largest request body an authenticated route reads (sealed JSON, not streams).
pub const AUTH_BODY_LIMIT: usize = 1024 * 1024;

/// Milliseconds on a monotonic clock (arbitrary start), so a wall-clock jump cannot revive or kill
/// a code, a pairing or a nonce. Injected so tests can move time.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    let start = std::time::Instant::now();
    Arc::new(move || start.elapsed().as_millis() as u64)
}

/// A refusal with the words the caller will read.
#[derive(Debug, PartialEq)]
pub struct Refusal {
    pub status: StatusCode,
    pub reason: String,
}

impl Refusal {
    pub fn new(status: StatusCode, reason: impl Into<String>) -> Refusal {
        Refusal { status, reason: reason.into() }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        crate::json_response(self.status, serde_json::json!({ "reason": self.reason }))
    }
}

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Standard base64, padded or not.
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(s.trim().trim_end_matches('=')).ok()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct Code {
    value: String,
    issued_ms: u64,
    wrong: u32,
    generation: u64,
}

struct Pending {
    id: String,
    k: [u8; 32],
    at: u64,
    generation: u64,
}

struct Inner {
    household: Option<Household>,
    nonces: VecDeque<Nonce>,
    code: Option<Code>,
    pending: VecDeque<Pending>,
    generation: u64,
    /// A finish or an unpair is doing file work outside the lock; nothing else may pair meanwhile.
    busy: bool,
}

struct Nonce {
    value: [u8; 32],
    at: u64,
    peer: Option<IpAddr>,
}

/// What the card and status page show.
pub struct PairingView {
    pub paired: bool,
    pub fingerprint: Option<String>,
    pub code: Option<String>,
}

pub struct Auth {
    dir: PathBuf,
    clock: Clock,
    inner: Mutex<Inner>,
}

fn hhmm_utc(ms: u64) -> String {
    let secs = ms / 1000 % 86_400;
    format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn busy_refusal() -> Refusal {
    Refusal::new(StatusCode::SERVICE_UNAVAILABLE, "another pairing or unpair is in progress; try again shortly")
}

impl Inner {
    /// Makes a new code (the old one and every half-done pairing die with it) and prints it.
    /// `now` is the monotonic clock; the printed expiry uses wall time.
    fn mint(&mut self, now: u64) {
        let value = pairing::new_code(&mut OsRng);
        self.generation += 1;
        self.pending.clear();
        println!("Aurelis Companion pairing code: {value} (expires {})", hhmm_utc(wall_ms() + CODE_LIFE_MS));
        self.code = Some(Code { value, issued_ms: now, wrong: 0, generation: self.generation });
    }

    fn code_expired(&self, now: u64) -> bool {
        self.code.as_ref().is_some_and(|c| now.saturating_sub(c.issued_ms) >= CODE_LIFE_MS)
    }

    /// Counts a wrong try against the current code; the fifth replaces it.
    fn wrong_try(&mut self, now: u64, what: &str) -> Refusal {
        let wrong = match self.code.as_mut() {
            Some(c) => {
                c.wrong += 1;
                c.wrong
            }
            None => 0,
        };
        if wrong >= MAX_WRONG {
            self.mint(now);
            return Refusal::new(StatusCode::FORBIDDEN, format!("{what} {MAX_WRONG} times; the code has been replaced"));
        }
        Refusal::new(StatusCode::FORBIDDEN, format!("{what} ({wrong} of {MAX_WRONG} tries used)"))
    }
}

impl Auth {
    pub fn new(dir: PathBuf, clock: Clock) -> Auth {
        let household = Household::load(&dir);
        let mut inner = Inner { household, nonces: VecDeque::new(), code: None, pending: VecDeque::new(), generation: 0, busy: false };
        if inner.household.is_none() {
            inner.mint(clock());
        }
        Auth { dir, clock, inner: Mutex::new(inner) }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> PairingView {
        let i = self.lock();
        PairingView {
            paired: i.household.is_some(),
            fingerprint: i.household.as_ref().map(|h| h.fingerprint.clone()),
            code: if i.household.is_some() { None } else { i.code.as_ref().map(|c| c.value.clone()) },
        }
    }

    /// The household secret, for opening what is sealed at rest (the AI runner); `None` when unpaired.
    pub fn household_secret(&self) -> Option<[u8; 32]> {
        self.lock().household.as_ref().map(|h| h.secret)
    }

    /// Replaces an expired code. True when a new one was made.
    pub fn tick(&self) -> bool {
        let now = (self.clock)();
        let mut i = self.lock();
        if i.household.is_none() && !i.busy && i.code_expired(now) {
            i.mint(now);
            return true;
        }
        false
    }

    /// A fresh single-use nonce, or `unpaired`. A nonce younger than `NONCE_PROTECTED_MS` is never
    /// pushed out, and one peer may hold only `NONCE_PER_PEER`, so a flood of unauthenticated
    /// challenges cannot starve the television's own.
    pub fn challenge(&self, peer: Option<IpAddr>) -> Result<[u8; 32], Refusal> {
        let now = (self.clock)();
        let mut i = self.lock();
        if i.household.is_none() {
            return Err(Refusal::new(StatusCode::UNAUTHORIZED, "unpaired"));
        }
        let crowded = || Refusal::new(StatusCode::TOO_MANY_REQUESTS, "too many outstanding challenges, try again shortly");
        i.nonces.retain(|n| now.saturating_sub(n.at) < NONCE_LIFE_MS);
        if i.nonces.iter().filter(|n| n.peer == peer).count() >= NONCE_PER_PEER {
            return Err(crowded());
        }
        if i.nonces.len() >= NONCE_CAP {
            match i.nonces.front() {
                Some(oldest) if now.saturating_sub(oldest.at) >= NONCE_PROTECTED_MS => {
                    i.nonces.pop_front();
                }
                _ => return Err(crowded()),
            }
        }
        let mut n = [0u8; 32];
        OsRng.fill_bytes(&mut n);
        i.nonces.push_back(Nonce { value: n, at: now, peer });
        Ok(n)
    }

    /// Checks nonce and proof; on success the nonce is spent and the secret returned.
    pub fn authenticate(&self, nonce_b64: Option<&str>, proof_b64: Option<&str>, method: &str, path: &str, body: &[u8]) -> Result<[u8; 32], Refusal> {
        let now = (self.clock)();
        let stale = || Refusal::new(StatusCode::UNAUTHORIZED, "stale-nonce");
        let bad = || Refusal::new(StatusCode::UNAUTHORIZED, "bad-proof");
        let (secret, nonce) = {
            let i = self.lock();
            let Some(h) = i.household.as_ref() else { return Err(Refusal::new(StatusCode::UNAUTHORIZED, "unpaired")) };
            let nonce = nonce_b64.and_then(unb64).ok_or_else(stale)?;
            let live = i.nonces.iter().any(|n| n.value[..] == nonce[..] && now.saturating_sub(n.at) < NONCE_LIFE_MS);
            if !live {
                return Err(stale());
            }
            (h.secret, nonce)
        };
        let offered = proof_b64.and_then(unb64).ok_or_else(bad)?;
        if !proof::verify(&secret, &nonce, method, path, body, &offered) {
            return Err(bad());
        }
        // Spend it now; a concurrent copy of the same request that got here first wins.
        let mut i = self.lock();
        let pos = i.nonces.iter().position(|n| n.value[..] == nonce[..]).ok_or_else(stale)?;
        i.nonces.remove(pos);
        Ok(secret)
    }

    /// Step one of pairing: the television's SPAKE2 message in, ours and a pairing id out.
    /// The curve maths runs on a blocking thread, outside the lock.
    pub async fn start_pairing(self: &Arc<Self>, tv_msg: Vec<u8>) -> Result<(String, Vec<u8>), Refusal> {
        let (value, generation) = {
            let now = (self.clock)();
            let mut i = self.lock();
            if let Some(h) = i.household.as_ref() {
                return Err(already_paired(h));
            }
            if i.busy {
                return Err(busy_refusal());
            }
            if i.code_expired(now) || i.code.is_none() {
                i.mint(now);
            }
            match i.code.as_ref() {
                Some(c) => (c.value.clone(), c.generation),
                None => return Err(Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, "no pairing code is available")),
            }
        };
        let joined = tokio::task::spawn_blocking(move || {
            let (p, my_msg) = pairing::Pairing::start(&value);
            p.finish(&tv_msg).map(|k| (k, my_msg))
        })
        .await;
        let (k, my_msg) = match joined {
            Ok(Some(x)) => x,
            Ok(None) => return Err(Refusal::new(StatusCode::BAD_REQUEST, "pairing message is not valid")),
            Err(e) => return Err(Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, format!("pairing maths failed: {e}"))),
        };
        let mut raw = [0u8; 16];
        OsRng.fill_bytes(&mut raw);
        raw[6] = raw[6] & 0x0f | 0x40;
        raw[8] = raw[8] & 0x3f | 0x80;
        let h: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let id = format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]);
        let now = (self.clock)();
        let mut i = self.lock();
        if i.code.as_ref().map(|c| c.generation) != Some(generation) {
            return Err(Refusal::new(StatusCode::CONFLICT, "the pairing code changed while this was starting; start again"));
        }
        while i.pending.len() >= PENDING_CAP {
            i.pending.pop_front();
        }
        i.pending.push_back(Pending { id: id.clone(), k, at: now, generation });
        Ok((id, my_msg))
    }

    /// Step two: the television proves it knew the code and hands over the household secret.
    /// Returns our confirmation and the household fingerprint. Opening the secret and writing the
    /// file happen on a blocking thread, outside the lock.
    pub async fn finish_pairing(self: &Arc<Self>, id: &str, confirm: &[u8], sealed_secret: Vec<u8>) -> Result<([u8; 32], String), Refusal> {
        let (k, generation) = {
            let now = (self.clock)();
            let mut i = self.lock();
            if let Some(h) = i.household.as_ref() {
                return Err(already_paired(h));
            }
            if i.busy {
                return Err(busy_refusal());
            }
            let gone = || Refusal::new(StatusCode::NOT_FOUND, "pairing not found or expired; start again");
            let pos = i.pending.iter().position(|p| p.id == id).ok_or_else(gone)?;
            let Some(p) = i.pending.remove(pos) else { return Err(gone()) };
            let current = i.code.as_ref().map(|c| c.generation);
            if current != Some(p.generation) || now.saturating_sub(p.at) >= CODE_LIFE_MS || i.code_expired(now) {
                return Err(gone());
            }
            if !ct_eq(&pairing::confirm(&p.k, TV_CONFIRM), confirm) {
                return Err(i.wrong_try(now, "wrong code"));
            }
            i.busy = true;
            (p.k, p.generation)
        };
        // Everything after the lock runs in its own task and the handler only waits for it, so a
        // client that hangs up (hyper drops the handler future) cannot abandon a half-done change.
        let this = self.clone();
        let sealed = sealed_secret;
        tokio::spawn(async move { this.complete_finish(k, generation, sealed).await })
            .await
            .unwrap_or_else(|e| {
                // The task died before it could clear `busy`; without this every later pairing and
                // unpair would refuse until the Companion restarted.
                self.lock().busy = false;
                eprintln!("pairing task failed: {e}");
                Err(Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, format!("pairing failed: {e}")))
            })
    }

    /// Blocking work, then applying its result and clearing `busy`, on every exit path.
    async fn complete_finish(self: Arc<Self>, k: [u8; 32], generation: u64, sealed_secret: Vec<u8>) -> Result<([u8; 32], String), Refusal> {
        let dir = self.dir.clone();
        let made = tokio::task::spawn_blocking(move || -> Result<Household, Refusal> {
            let secret: [u8; 32] = seal::open(&sealed_secret, &k)
                .and_then(|v| v.try_into().ok())
                .ok_or_else(|| Refusal::new(StatusCode::BAD_REQUEST, "the household secret could not be opened with the pairing key"))?;
            Household::create(&dir, secret).map_err(|e| {
                eprintln!("pairing could not be saved: {e}");
                Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, format!("pairing could not be saved: {e}"))
            })
        })
        .await
        .unwrap_or_else(|e| {
            eprintln!("pairing work failed: {e}");
            Err(Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, format!("pairing failed: {e}")))
        });
        let now = (self.clock)();
        let mut i = self.lock();
        i.busy = false;
        match made {
            Ok(household) => {
                let fp = household.fingerprint.clone();
                i.household = Some(household);
                i.code = None;
                i.pending.clear();
                i.nonces.clear();
                Ok((pairing::confirm(&k, COMPANION_CONFIRM), fp))
            }
            Err(r) => {
                // A secret that will not open under the right key is a wrong try; a failed save is ours.
                if r.status == StatusCode::BAD_REQUEST && i.code.as_ref().map(|c| c.generation) == Some(generation) {
                    let counted = i.wrong_try(now, "unusable secret");
                    return Err(Refusal::new(r.status, format!("{}; {}", r.reason, counted.reason)));
                }
                Err(r)
            }
        }
    }

    /// Wipes the secret and every store, then makes a new code. The state flips under the lock; the
    /// deleting runs on a blocking thread in its own task. A failed wipe puts the household back.
    pub async fn unpair(self: &Arc<Self>) -> Result<(), Refusal> {
        let household = {
            let mut i = self.lock();
            if i.busy {
                return Err(busy_refusal());
            }
            i.busy = true;
            i.nonces.clear();
            i.household.take()
        };
        let this = self.clone();
        tokio::spawn(async move { this.complete_unpair(household).await })
            .await
            .unwrap_or_else(|e| {
                self.lock().busy = false;
                eprintln!("unpair task failed: {e}");
                Err(Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, format!("unpair failed: {e}")))
            })
    }

    async fn complete_unpair(self: Arc<Self>, household: Option<Household>) -> Result<(), Refusal> {
        let wiped = match household.clone() {
            None => Ok(()),
            Some(h) => tokio::task::spawn_blocking(move || h.wipe()).await.unwrap_or_else(|e| Err(format!("the wipe task failed: {e}"))),
        };
        let now = (self.clock)();
        let mut i = self.lock();
        i.busy = false;
        match wiped {
            Ok(()) => {
                i.mint(now);
                Ok(())
            }
            Err(e) => {
                eprintln!("unpair failed: {e}");
                i.household = household;
                if i.household.is_none() {
                    i.mint(now);
                }
                Err(Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, format!("unpair failed: {e}")))
            }
        }
    }

    #[cfg(test)]
    pub fn is_busy(&self) -> bool {
        self.lock().busy
    }
}

fn already_paired(h: &Household) -> Refusal {
    Refusal::new(StatusCode::CONFLICT, format!("already paired to household {} \u{2014} unpair first", h.fingerprint))
}

/// Handed to authenticated handlers (as an `Extension`): the household secret for sealing replies.
#[derive(Clone)]
pub struct Session {
    pub secret: [u8; 32],
}

impl Session {
    /// A v3-sealed JSON reply (`application/octet-stream`).
    pub fn sealed_json(&self, status: StatusCode, value: &serde_json::Value) -> Response {
        let sealed = seal::seal(value.to_string().as_bytes(), &self.secret);
        (status, [(header::CONTENT_TYPE, "application/octet-stream")], sealed).into_response()
    }

    /// A sealed `{"reason": ...}` error.
    pub fn sealed_reason(&self, status: StatusCode, reason: &str) -> Response {
        self.sealed_json(status, &serde_json::json!({ "reason": reason }))
    }
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// Every authenticated route sits behind this. It needs the version header, the nonce and the
/// proof, opens the sealed body and hands the handler the plain bytes and a [`Session`].
pub async fn require_household(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    if !parts.headers.contains_key(crate::VERSION_HEADER) {
        return crate::json_response(
            StatusCode::UPGRADE_REQUIRED,
            serde_json::json!({
                "reason": "this request needs the X-Aurelis-Companion header",
                "companionSpeaks": [crate::state::PROTOCOL_VERSION],
            }),
        );
    }
    let path_str = parts.uri.path();
    let limit = if matches!(path_str, crate::bundle_store::PATH | crate::bundle_store::OWNERSHIP_PATH)
        || path_str == "/v1/lineup/tv/channels/report"
        || path_str.starts_with("/v1/lineup/tv/arbiter/")
    {
        crate::bundle_store::MAX_REQUEST_BYTES
    } else {
        AUTH_BODY_LIMIT
    };
    let raw = match to_bytes(body, limit).await {
        Ok(b) => b,
        Err(_) => return Refusal::new(StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };
    let path = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| parts.uri.path().to_string());
    let secret = match state.auth.authenticate(
        header_str(&parts.headers, "x-aurelis-nonce"),
        header_str(&parts.headers, "x-aurelis-proof"),
        parts.method.as_str(),
        &path,
        &raw,
    ) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    // A request with no body (a GET) has nothing sealed; anything else must open.
    let opened = if raw.is_empty() {
        Vec::new()
    } else {
        match seal::open(&raw, &secret) {
            Some(p) => p,
            None => return crate::json_response(StatusCode::BAD_REQUEST, serde_json::json!({ "reason": "body could not be opened" })),
        }
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_TYPE);
    parts.extensions.insert(Session { secret });
    next.run(Request::from_parts(parts, Body::from(opened))).await
}
