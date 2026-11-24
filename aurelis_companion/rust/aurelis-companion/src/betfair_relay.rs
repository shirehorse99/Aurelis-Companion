//! The Betfair relay: a paired television behind a router VPN hands the Companion one Betfair HTTP
//! request; the Companion sends it to Betfair from its own connection and returns Betfair's answer
//! unchanged.
//!
//! * Only `https://identitysso.betfair.com/…` and `https://api.betfair.com/…`, `POST` only, on the
//!   default port, with no user name or password in the address. The address is read with a real
//!   URL parser and the host compared exactly; anything else is 400 with a reason and nothing is sent.
//! * Betfair's own status, `Content-Type` and body come back in a 200 envelope, whatever the status
//!   (a Betfair 403 stays a 403 inside it). Redirects are never followed and nothing is decompressed,
//!   so the bytes are Betfair's.
//! * A fault of the relay itself (Betfair unreachable from here, no reply in time, a reply too big)
//!   is 502 with `X-Aurelis-Relay-Error: <reason>`, so the television can tell it from Betfair's answer.
//! * Nothing is kept. In memory: the last call's host, the path's last segment, the status code, the
//!   time taken and when. Log lines carry exactly those, never headers, bodies, user names,
//!   passwords, app keys or session tokens.

use crate::auth::Session;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use axum::Extension;
use reqwest::Url;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const PATH: &str = "/v1/relay/betfair";
/// The card's capability and `declined` key.
pub const CAPABILITY: &str = "betfairRelay";
pub const RELAY_OFF_REASON: &str = "switched off in the Companion's options (betfair_relay)";
/// The response header that marks a relay fault (not Betfair's answer).
pub const ERROR_HEADER: &str = "x-aurelis-relay-error";
/// The only hosts relayed to, compared exactly.
pub const BETFAIR_HOSTS: [&str; 2] = ["identitysso.betfair.com", "api.betfair.com"];
/// The largest Betfair request body relayed (a login form or a JSON-RPC call is a few hundred bytes).
pub const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;
/// The largest Betfair reply relayed (a `listMarketBook` batch is far smaller).
pub const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;
/// Companion to Betfair, the whole call. Below the television's 15 s read timeout, so a slow Betfair
/// comes back as this relay's fault (with its reason) rather than as the Companion going silent.
pub const TIMEOUT: Duration = Duration::from_secs(14);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEADERS: usize = 32;
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Headers a television's request never carries on to Betfair: the hop-by-hop ones, those the
/// relay sets itself, and anything of the Companion protocol's own (`X-Aurelis-*`, by prefix).
const DROPPED_HEADERS: [&str; 12] = [
    "host",
    "connection",
    "content-length",
    "accept-encoding",
    "keep-alive",
    "proxy-authorization",
    "proxy-authenticate",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];
const DROPPED_PREFIX: &str = "x-aurelis-";

/// What one call to Betfair came back with.
#[derive(Debug, Clone, PartialEq)]
pub enum BetfairOutcome {
    /// Betfair answered: its status, `Content-Type` and body, untouched.
    Reply { status: u16, content_type: Option<String>, body: Vec<u8> },
    /// The relay could not get an answer; the reason is the relay's own wording (never Betfair's
    /// words, never the address).
    Fault(String),
}

pub type SendFuture<'a> = Pin<Box<dyn Future<Output = BetfairOutcome> + Send + 'a>>;

/// Makes one call to Betfair. Production uses [`ReqwestBetfairSender`]; tests use a fake.
pub trait BetfairSender: Send + Sync {
    fn send<'a>(&'a self, url: &'a str, headers: &'a [(String, String)], body: &'a str, timeout: Duration) -> SendFuture<'a>;
}

/// The real thing: its own client, no redirects, no decompression (so `Accept-Encoding` is not
/// sent at all and the body the television parses is exactly Betfair's).
pub struct ReqwestBetfairSender {
    client: reqwest::Client,
}

impl ReqwestBetfairSender {
    pub fn new() -> Result<ReqwestBetfairSender, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            // Never a proxy from the environment (HTTPS_PROXY, ALL_PROXY): the call must leave from
            // this machine's own connection, and nothing of it may pass through anyone else.
            .no_proxy()
            .user_agent(concat!("AurelisCompanion/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("the relay's HTTP client could not be built: {}", crate::guide_store::describe_err(e)))?;
        Ok(ReqwestBetfairSender { client })
    }
}

fn whole_seconds_or_ms(d: Duration) -> String {
    if d.as_millis() % 1000 == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", d.as_millis())
    }
}

fn too_big(n: &str) -> BetfairOutcome {
    BetfairOutcome::Fault(format!("Betfair's reply was {n} bytes; the relay carries at most {MAX_REPLY_BYTES}"))
}

impl BetfairSender for ReqwestBetfairSender {
    fn send<'a>(&'a self, url: &'a str, headers: &'a [(String, String)], body: &'a str, timeout: Duration) -> SendFuture<'a> {
        Box::pin(async move {
            let mut req = self.client.post(url).timeout(timeout).body(body.to_string());
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if e.is_timeout() => return BetfairOutcome::Fault(format!("no reply from Betfair within {}", whole_seconds_or_ms(timeout))),
                // `describe_err` drops the address before printing.
                Err(e) => return BetfairOutcome::Fault(format!("Betfair could not be reached from the Companion: {}", crate::guide_store::describe_err(e))),
            };
            let status = resp.status().as_u16();
            let content_type = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
            // Refused before reading when the length is declared; otherwise read in chunks and stop
            // at the limit, so a large body is never held whole.
            if let Some(len) = resp.content_length().filter(|n| *n > MAX_REPLY_BYTES as u64) {
                return too_big(&len.to_string());
            }
            let mut resp = resp;
            let mut got: Vec<u8> = Vec::new();
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        if got.len() + chunk.len() > MAX_REPLY_BYTES {
                            return too_big(&format!("more than {MAX_REPLY_BYTES}"));
                        }
                        got.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(e) if e.is_timeout() => return BetfairOutcome::Fault(format!("Betfair's reply did not finish within {}", whole_seconds_or_ms(timeout))),
                    Err(e) => return BetfairOutcome::Fault(format!("Betfair's reply was cut off: {}", crate::guide_store::describe_err(e))),
                }
            }
            BetfairOutcome::Reply { status, content_type, body: got }
        })
    }
}

/// Stands in for a client that could not be built: every call is a relay fault with that reason.
struct BrokenSender(String);

impl BetfairSender for BrokenSender {
    fn send<'a>(&'a self, _: &'a str, _: &'a [(String, String)], _: &'a str, _: Duration) -> SendFuture<'a> {
        let why = self.0.clone();
        Box::pin(async move { BetfairOutcome::Fault(why) })
    }
}

/// The last call, as the status page prints it. Nothing else of a call is kept.
#[derive(Debug, Clone, PartialEq)]
pub struct LastCall {
    pub at_ms: i64,
    pub host: String,
    /// The path's last segment: `login`, `listMarketCatalogue`, `listMarketBook`.
    pub call: String,
    /// The status the television was given: Betfair's, or 502 for a relay fault.
    pub http_code: u16,
    /// The relay's own reason when it was a relay fault.
    pub fault: Option<String>,
    pub took_ms: u64,
}

/// The relay's state on [`AppState`]: its sender (built on first use) and the last call.
pub struct Relay {
    sender: OnceLock<Arc<dyn BetfairSender>>,
    timeout: Mutex<Duration>,
    last: Mutex<Option<LastCall>>,
}

impl Default for Relay {
    fn default() -> Self {
        Relay::new()
    }
}

impl Relay {
    pub fn new() -> Relay {
        Relay { sender: OnceLock::new(), timeout: Mutex::new(TIMEOUT), last: Mutex::new(None) }
    }

    fn sender(&self) -> Arc<dyn BetfairSender> {
        self.sender
            .get_or_init(|| match ReqwestBetfairSender::new() {
                Ok(s) => Arc::new(s),
                Err(why) => {
                    eprintln!("betfair relay: {why}");
                    Arc::new(BrokenSender(why))
                }
            })
            .clone()
    }

    fn timeout(&self) -> Duration {
        *self.timeout.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn last_call(&self) -> Option<LastCall> {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn record(&self, call: LastCall) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(call);
    }

    #[cfg(test)]
    pub fn set_sender(&self, sender: Arc<dyn BetfairSender>) {
        assert!(self.sender.set(sender).is_ok(), "the relay's sender was already chosen");
    }

    #[cfg(test)]
    pub fn set_timeout(&self, d: Duration) {
        *self.timeout.lock().unwrap_or_else(|e| e.into_inner()) = d;
    }
}

/// A request the relay will send, already checked.
#[derive(Debug, PartialEq)]
pub struct Checked {
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Why a request was refused before anything was sent. `reason` may quote what the caller sent and
/// goes only into the sealed reply; `logged` is fixed wording, the only part printed.
#[derive(Debug, PartialEq)]
pub struct Refused {
    pub status: StatusCode,
    pub reason: String,
    pub logged: &'static str,
}

const LOG_NOT_BETFAIR: &str = "not an allowed Betfair address";
const LOG_NOT_POST: &str = "not a POST";
const LOG_MALFORMED: &str = "not a well-formed relay request";
const LOG_TOO_BIG: &str = "request body too large";

fn bad(reason: impl Into<String>) -> Refused {
    Refused { status: StatusCode::BAD_REQUEST, reason: reason.into(), logged: LOG_MALFORMED }
}

fn not_betfair(reason: impl Into<String>) -> Refused {
    Refused { status: StatusCode::BAD_REQUEST, reason: reason.into(), logged: LOG_NOT_BETFAIR }
}

/// At most `n` characters, for echoing something the caller sent into a reason.
fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Only `https`, only Betfair's two hosts exactly, the default port, no user name or password.
pub fn check_url(raw: &str) -> Result<Url, Refused> {
    let url = Url::parse(raw.trim()).map_err(|_| not_betfair("the address could not be read as a URL"))?;
    if url.scheme() != "https" {
        return Err(not_betfair(format!("only https addresses are relayed, not {}", clip(url.scheme(), 20))));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(not_betfair("an address carrying a user name or password is not relayed"));
    }
    // `Url` drops the scheme's default port, so any port left is another one.
    if let Some(port) = url.port() {
        return Err(not_betfair(format!("only the https port is relayed, not {port}")));
    }
    // The parser lower-cases the host, IDNA-encodes any Unicode name to its `xn--` form and leaves
    // an `xn--` name as it is, so a look-alike never equals either ASCII name; an address such as
    // 127.0.0.1 or [::1] is a host string too, and matches neither name.
    match url.host_str() {
        Some(h) if BETFAIR_HOSTS.contains(&h) => Ok(url),
        Some(h) => Err(not_betfair(format!("only {} are relayed, not {}", BETFAIR_HOSTS.join(" and "), clip(h, 100)))),
        None => Err(not_betfair("the address names no host")),
    }
}

fn dropped(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    DROPPED_HEADERS.contains(&lower.as_str()) || lower.starts_with(DROPPED_PREFIX)
}

/// Reads the opened request: `{"url", "headers": [[name, value]…], "body", "method"?}`.
pub fn check_request(plain: &[u8]) -> Result<Checked, Refused> {
    let v: Value = serde_json::from_slice(plain).map_err(|_| bad("the relay request is not valid JSON"))?;
    if let Some(m) = v.get("method").filter(|m| !m.is_null()) {
        match m.as_str() {
            Some(m) if m.eq_ignore_ascii_case("POST") => {}
            Some(m) => return Err(Refused { status: StatusCode::BAD_REQUEST, reason: format!("only POST is relayed, not {}", clip(m, 20)), logged: LOG_NOT_POST }),
            None => return Err(bad("'method' must be text")),
        }
    }
    let url = check_url(v.get("url").and_then(Value::as_str).ok_or_else(|| bad("the relay request names no 'url'"))?)?;
    let body = match v.get("body") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(bad("'body' must be text")),
    };
    if body.len() > MAX_REQUEST_BODY_BYTES {
        return Err(Refused {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            reason: format!("the Betfair request body is {} bytes; the relay carries at most {MAX_REQUEST_BODY_BYTES}", body.len()),
            logged: LOG_TOO_BIG,
        });
    }
    let listed = match v.get("headers") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err(bad("'headers' must be a list of [name, value] pairs")),
    };
    if listed.len() > MAX_HEADERS {
        return Err(bad(format!("{} headers sent; the relay carries at most {MAX_HEADERS}", listed.len())));
    }
    let mut headers = Vec::with_capacity(listed.len());
    let mut total = 0usize;
    for (i, pair) in listed.iter().enumerate() {
        let (name, value) = match pair.as_array().map(Vec::as_slice) {
            Some([Value::String(n), Value::String(v)]) => (n, v),
            _ => return Err(bad(format!("header {i} is not a [name, value] pair of text"))),
        };
        // Checked here, so a bad one is the caller's 400 and never a fault at send time. The value
        // is never echoed: it may be a token.
        if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
            return Err(bad(format!("header {i} has a name that is not allowed")));
        }
        if HeaderValue::from_str(value).is_err() {
            return Err(bad(format!("header {i} has a value that is not allowed")));
        }
        total += name.len() + value.len();
        if total > MAX_HEADER_BYTES {
            return Err(bad(format!("the headers come to more than {MAX_HEADER_BYTES} bytes")));
        }
        if !dropped(name) {
            headers.push((name.clone(), value.clone()));
        }
    }
    Ok(Checked { url, headers, body })
}

/// The Betfair calls the relay may name in a log line or on the status page. Anything else prints
/// as `other`: the path is the caller's, and only a name from this list is known to carry nothing.
pub const KNOWN_CALLS: [&str; 11] = [
    "login",
    "logout",
    "keepAlive",
    "listEventTypes",
    "listEvents",
    "listMarketCatalogue",
    "listMarketBook",
    "listRunnerBook",
    "listMarketProfitAndLoss",
    "listCurrentOrders",
    "listClearedOrders",
];

/// The path's last non-empty segment when it is one of [`KNOWN_CALLS`] (exact match), else `other`:
/// `…/rest/v1.0/listMarketBook/` → `listMarketBook`.
pub fn call_name(url: &Url) -> String {
    let seg = url.path_segments().and_then(|s| s.filter(|p| !p.is_empty()).last()).unwrap_or("");
    KNOWN_CALLS.iter().find(|c| **c == seg).map(|c| c.to_string()).unwrap_or_else(|| "other".to_string())
}

/// A reason as a header value: printable ASCII only.
fn header_safe(reason: &str) -> HeaderValue {
    let text: String = reason.chars().map(|c| if (' '..='~').contains(&c) { c } else { '?' }).take(300).collect();
    HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static("relay fault"))
}

/// Every line the relay prints goes through here: host, call, status and time taken, or a refusal
/// in the relay's own words. Tests read what was printed.
fn log(line: String) {
    #[cfg(test)]
    tests::LOGGED.lock().unwrap_or_else(|e| e.into_inner()).push(line.clone());
    println!("{line}");
}

pub fn log_line(host: &str, call: &str, outcome: &str, took_ms: u64) -> String {
    format!("betfair relay: {host} {call}: {outcome} in {took_ms} ms")
}

/// A relay fault: 502, the reason sealed as every reply is, and the same reason in the clear in
/// [`ERROR_HEADER`] (it is the relay's own wording and carries nothing of the call).
fn fault_response(session: &Session, reason: &str) -> Response {
    let mut res = session.sealed_reason(StatusCode::BAD_GATEWAY, reason);
    res.headers_mut().insert(ERROR_HEADER, header_safe(reason));
    res
}

pub async fn relay_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>, body: Bytes) -> Response {
    if let Some(why) = state.betfair_relay_declined() {
        return session.sealed_reason(StatusCode::SERVICE_UNAVAILABLE, &why);
    }
    let checked = match check_request(&body) {
        Ok(c) => c,
        Err(r) => {
            // Fixed wording only: the detailed reason can quote the caller's address and goes back sealed.
            log(format!("betfair relay refused ({}): {}", r.status.as_u16(), r.logged));
            return session.sealed_reason(r.status, &r.reason);
        }
    };
    let host = checked.url.host_str().unwrap_or("").to_string();
    let call = call_name(&checked.url);
    let timeout = state.betfair.timeout();
    let sender = state.betfair.sender();
    let started = Instant::now();
    // The sender has the same limit; this one also holds a sender that ignores it.
    let outcome = match tokio::time::timeout(timeout, sender.send(checked.url.as_str(), &checked.headers, &checked.body, timeout)).await {
        Ok(o) => o,
        Err(_) => BetfairOutcome::Fault(format!("no reply from Betfair within {}", whole_seconds_or_ms(timeout))),
    };
    let took_ms = started.elapsed().as_millis() as u64;
    let (status, content_type, text) = match outcome {
        BetfairOutcome::Reply { status, content_type, body } => match String::from_utf8(body) {
            Ok(text) => (status, content_type, text),
            Err(_) => return fault(&state, &session, host, call, took_ms, "Betfair's reply is not UTF-8 text, so it cannot be relayed unchanged".to_string()),
        },
        BetfairOutcome::Fault(why) => return fault(&state, &session, host, call, took_ms, why),
    };
    log(log_line(&host, &call, &format!("HTTP {status}"), took_ms));
    state.push_job(&format!("Betfair {call}: HTTP {status} in {}", whole_seconds_or_ms(Duration::from_millis(took_ms))));
    state.betfair.record(LastCall { at_ms: state.wall_ms(), host, call, http_code: status, fault: None, took_ms });
    session.sealed_json(StatusCode::OK, &serde_json::json!({ "status": status, "contentType": content_type, "body": text }))
}

fn fault(state: &AppState, session: &Session, host: String, call: String, took_ms: u64, why: String) -> Response {
    log(log_line(&host, &call, &format!("relay fault ({why})"), took_ms));
    state.push_job(&format!("Betfair {call}: relay failed after {} ({why})", whole_seconds_or_ms(Duration::from_millis(took_ms))));
    let res = fault_response(session, &why);
    state.betfair.record(LastCall { at_ms: state.wall_ms(), host, call, http_code: StatusCode::BAD_GATEWAY.as_u16(), fault: Some(why), took_ms });
    res
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::auth::{b64, unb64};
    use crate::household::{Household, TempDir};
    use crate::state::{test_config, test_state};
    use axum::body::{to_bytes, Body};
    use axum::http::{HeaderMap, Request};
    use companion_protocol::{proof, seal};
    use serde_json::json;
    use tower::ServiceExt;

    /// Every line the relay printed, from every test (they run in parallel, so a test asserts what
    /// must be true of all of them).
    pub static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    const SECRET: [u8; 32] = [7u8; 32];
    const LOGIN: &str = "https://identitysso.betfair.com/api/login";
    const BOOK: &str = "https://api.betfair.com/exchange/betting/rest/v1.0/listMarketBook/";
    const PASSWORD: &str = "relay-test-password-Zq81";
    const APP_KEY: &str = "relay-test-app-key-Hk27";
    const TOKEN: &str = "relay-test-session-token-Pw55";
    const LOGIN_REPLY: &str = r#"{"token":"relay-test-session-token-Pw55","product":"relay-test-app-key-Hk27","status":"SUCCESS","error":""}"#;

    #[derive(Debug, Clone)]
    struct Call {
        url: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    /// Answers every call with `reply` (after `delay`) and records what it was asked.
    struct Fake {
        reply: BetfairOutcome,
        delay: Duration,
        calls: Mutex<Vec<Call>>,
    }

    impl Fake {
        fn new(reply: BetfairOutcome) -> Arc<Fake> {
            Arc::new(Fake { reply, delay: Duration::ZERO, calls: Mutex::new(Vec::new()) })
        }

        fn slow(reply: BetfairOutcome, delay: Duration) -> Arc<Fake> {
            Arc::new(Fake { reply, delay, calls: Mutex::new(Vec::new()) })
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl BetfairSender for Fake {
        fn send<'a>(&'a self, url: &'a str, headers: &'a [(String, String)], body: &'a str, _timeout: Duration) -> SendFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(Call { url: url.to_string(), headers: headers.to_vec(), body: body.to_string() });
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                self.reply.clone()
            })
        }
    }

    fn json_reply(status: u16, body: &str) -> BetfairOutcome {
        BetfairOutcome::Reply { status, content_type: Some("application/json".into()), body: body.as_bytes().to_vec() }
    }

    struct Rig {
        _dir: TempDir,
        state: Arc<AppState>,
    }

    fn rig_with(relay_on: bool, fake: Arc<Fake>) -> Rig {
        let dir = TempDir::new();
        Household::create(&dir.0, SECRET).expect("household");
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        config.betfair_relay = relay_on;
        let state = Arc::new(test_state(config));
        state.betfair.set_sender(fake);
        Rig { _dir: dir, state }
    }

    fn rig(fake: Arc<Fake>) -> Rig {
        rig_with(true, fake)
    }

    fn login_request() -> Value {
        json!({
            "url": LOGIN,
            "headers": [["X-Application", APP_KEY], ["Content-Type", "application/x-www-form-urlencoded"], ["Accept", "application/json"]],
            "body": format!("username=relaytester&password={PASSWORD}"),
        })
    }

    impl Rig {
        async fn send(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
            let res = crate::router(self.state.clone()).oneshot(req).await.expect("infallible");
            let (status, headers) = (res.status(), res.headers().clone());
            (status, headers, to_bytes(res.into_body(), 32 << 20).await.expect("body").to_vec())
        }

        async fn relay_raw(&self, plain: &[u8]) -> (StatusCode, HeaderMap, Vec<u8>) {
            let req = Request::builder().uri("/v1/challenge").header("x-aurelis-companion", "1").body(Body::empty()).unwrap();
            let (s, _, b) = self.send(req).await;
            assert_eq!(s, StatusCode::OK);
            let nonce = serde_json::from_slice::<Value>(&b).unwrap()["nonce"].as_str().unwrap().to_string();
            let sealed = seal::seal(plain, &SECRET);
            let p = proof::proof(&SECRET, &unb64(&nonce).unwrap(), "POST", PATH, &sealed);
            let req = Request::builder()
                .method("POST")
                .uri(PATH)
                .header("x-aurelis-companion", "1")
                .header("x-aurelis-nonce", nonce)
                .header("x-aurelis-proof", b64(&p))
                .body(Body::from(sealed))
                .unwrap();
            self.send(req).await
        }

        async fn relay(&self, v: &Value) -> (StatusCode, HeaderMap, Value) {
            let (s, h, b) = self.relay_raw(v.to_string().as_bytes()).await;
            let opened = seal::open(&b, &SECRET).unwrap_or(b);
            (s, h, serde_json::from_slice(&opened).unwrap_or(Value::Null))
        }
    }

    #[tokio::test]
    async fn relays_a_login_and_returns_betfairs_answer_unchanged() {
        let fake = Fake::new(json_reply(200, LOGIN_REPLY));
        let r = rig(fake.clone());
        let (s, h, v) = r.relay(&login_request()).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert!(h.get(ERROR_HEADER).is_none());
        assert_eq!(v["status"], 200);
        assert_eq!(v["contentType"], "application/json");
        assert_eq!(v["body"].as_str().unwrap().as_bytes(), LOGIN_REPLY.as_bytes(), "byte-equal");
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].url, LOGIN);
        assert_eq!(calls[0].body, format!("username=relaytester&password={PASSWORD}"));
        let last = r.state.betfair.last_call().expect("the call is remembered");
        assert_eq!((last.host.as_str(), last.call.as_str(), last.http_code, last.fault.as_deref()), ("identitysso.betfair.com", "login", 200, None));
    }

    #[tokio::test]
    async fn betfair_403_passes_through_as_200_envelope() {
        let words = r#"{"loginStatus":"FORBIDDEN_LOCATION"}"#;
        let fake = Fake::new(BetfairOutcome::Reply { status: 403, content_type: Some("text/plain; charset=utf-8".into()), body: words.as_bytes().to_vec() });
        let r = rig(fake.clone());
        let (s, h, v) = r.relay(&login_request()).await;
        assert_eq!(s, StatusCode::OK, "a Betfair refusal is Betfair's answer, not a relay fault: {v}");
        assert!(h.get(ERROR_HEADER).is_none());
        assert_eq!((v["status"].as_u64(), v["contentType"].as_str(), v["body"].as_str()), (Some(403), Some("text/plain; charset=utf-8"), Some(words)));
        // No Content-Type from Betfair is null, not invented.
        let fake = Fake::new(BetfairOutcome::Reply { status: 204, content_type: None, body: Vec::new() });
        let r = rig(fake);
        let (s, _, v) = r.relay(&login_request()).await;
        assert_eq!((s, v["status"].as_u64(), &v["contentType"], v["body"].as_str()), (StatusCode::OK, Some(204), &Value::Null, Some("")));
    }

    #[tokio::test]
    async fn only_betfair_hosts_over_https_are_forwarded() {
        let fake = Fake::new(json_reply(200, "{}"));
        let r = rig(fake.clone());
        for url in [
            "https://evil.example/api/login",
            "http://identitysso.betfair.com/api/login",
            "http://api.betfair.com/exchange/betting/rest/v1.0/listMarketBook/",
            "https://api.betfair.com.evil.example/exchange/",
            "https://user@api.betfair.com.evil/x",
            "https://user:pw@api.betfair.com/x",
            "https://evil.example/?h=api.betfair.com",
            "https://api.betfair.com:8443/exchange/",
            "https://identitysso.betfair.com:8443/api/login",
            "https://api.betfair.com./exchange/",
            "https://betfair.com/x",
            "https://127.0.0.1/x",
            "ftp://api.betfair.com/x",
            "api.betfair.com/x",
            "not a url",
        ] {
            let (s, h, v) = r.relay(&json!({"url": url, "headers": [], "body": ""})).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{url}: {v}");
            assert!(v["reason"].as_str().is_some_and(|r| !r.is_empty()), "{url}: a printable reason: {v}");
            assert!(h.get(ERROR_HEADER).is_none(), "a refusal is not a relay fault");
        }
        // Not POST, or no URL at all.
        let (s, _, v) = r.relay(&json!({"url": LOGIN, "method": "GET"})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        let (s, _, _) = r.relay(&json!({"headers": []})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(fake.calls().is_empty(), "nothing refused was sent: {:?}", fake.calls());
        // The two hosts themselves, upper-cased or with the default port spelled out, are fine.
        for url in ["https://API.betfair.com/exchange/betting/rest/v1.0/listMarketBook/", "https://identitysso.betfair.com:443/api/login"] {
            let (s, _, v) = r.relay(&json!({"url": url, "method": "post"})).await;
            assert_eq!(s, StatusCode::OK, "{url}: {v}");
        }
        assert_eq!(fake.calls().len(), 2);
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let fake = Fake::new(BetfairOutcome::Reply { status: 302, content_type: Some("text/html".into()), body: b"moved".to_vec() });
        let r = rig(fake.clone());
        let (s, _, v) = r.relay(&login_request()).await;
        assert_eq!((s, v["status"].as_u64(), v["body"].as_str()), (StatusCode::OK, Some(302), Some("moved")), "{v}");
        assert_eq!(fake.calls().len(), 1, "no second call");
    }

    /// The production client against a local server: a 302 comes back as it is, and a gzip body is
    /// handed on still compressed (no `Accept-Encoding` was asked for and none is decoded).
    #[tokio::test]
    async fn the_real_client_neither_follows_redirects_nor_decodes() {
        use axum::http::header;
        use axum::response::IntoResponse;
        use axum::routing::post;
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_ae = Arc::new(Mutex::new(Vec::<String>::new()));
        let (h1, h2, ae) = (hits.clone(), hits.clone(), seen_ae.clone());
        let app = axum::Router::new()
            .route("/moved", post(move || async move {
                h1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (StatusCode::FOUND, [(header::LOCATION, "/target")], "moved").into_response()
            }))
            .route("/target", post(move || async move {
                h2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                "followed"
            }))
            .route("/gz", post(move |headers: HeaderMap| async move {
                ae.lock().unwrap().push(headers.get(header::ACCEPT_ENCODING).map(|v| v.to_str().unwrap_or("?").to_string()).unwrap_or_default());
                ([(header::CONTENT_ENCODING, "gzip"), (header::CONTENT_TYPE, "application/json")], vec![0x1f, 0x8b, 0x08, 0x00]).into_response()
            }))
            .route("/big", post(|| async { vec![b'x'; MAX_REPLY_BYTES + 1] }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let sender = ReqwestBetfairSender::new().unwrap();
        let out = sender.send(&format!("{base}/moved"), &[], "", Duration::from_secs(5)).await;
        assert_eq!(out, BetfairOutcome::Reply { status: 302, content_type: Some("text/plain; charset=utf-8".into()), body: b"moved".to_vec() });
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "the redirect target was never asked");
        let out = sender.send(&format!("{base}/gz"), &[], "", Duration::from_secs(5)).await;
        assert_eq!(out, BetfairOutcome::Reply { status: 200, content_type: Some("application/json".into()), body: vec![0x1f, 0x8b, 0x08, 0x00] });
        assert_eq!(seen_ae.lock().unwrap().as_slice(), [String::new()], "no Accept-Encoding was sent");
        match sender.send(&format!("{base}/big"), &[], "", Duration::from_secs(5)).await {
            BetfairOutcome::Fault(why) => assert!(why.contains("at most"), "{why}"),
            other => panic!("an oversize reply is a relay fault, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hop_headers_and_aurelis_headers_are_dropped() {
        let fake = Fake::new(json_reply(200, "[]"));
        let r = rig(fake.clone());
        let req = json!({
            "url": BOOK,
            "headers": [
                ["Host", "evil.example"], ["Connection", "close"], ["Content-Length", "99"], ["Accept-Encoding", "gzip"],
                ["X-Aurelis-Nonce", "n"], ["x-aurelis-companion", "1"], ["X-AURELIS-PROOF", "p"],
                ["Transfer-Encoding", "chunked"], ["Keep-Alive", "timeout=5"], ["Upgrade", "h2c"], ["TE", "trailers"],
                ["Proxy-Authorization", "Basic x"],
                ["X-Application", APP_KEY], ["X-Authentication", TOKEN], ["Content-Type", "application/json"],
            ],
            "body": "{\"marketIds\":[\"1.2\"]}",
        });
        let (s, _, v) = r.relay(&req).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let calls = fake.calls();
        let sent: Vec<(&str, &str)> = calls[0].headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(sent, [("X-Application", APP_KEY), ("X-Authentication", TOKEN), ("Content-Type", "application/json")]);
        // A header that could not be sent is the caller's 400, before anything goes out.
        let (s, _, v) = r.relay(&json!({"url": BOOK, "headers": [["Bad Name", "x"]]})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        let (s, _, v) = r.relay(&json!({"url": BOOK, "headers": [["X-Application", "a\r\nInjected: 1"]]})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        assert!(!v.to_string().contains("Injected"), "a value is never echoed");
        assert_eq!(fake.calls().len(), 1);
    }

    #[tokio::test]
    async fn oversize_request_is_413_and_oversize_reply_is_502_relay_fault() {
        let fake = Fake::new(json_reply(200, "{}"));
        let r = rig(fake.clone());
        let (s, h, v) = r.relay(&json!({"url": BOOK, "body": "x".repeat(MAX_REQUEST_BODY_BYTES + 1)})).await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
        assert!(v["reason"].as_str().unwrap().contains("at most"), "{v}");
        assert!(h.get(ERROR_HEADER).is_none());
        assert!(fake.calls().is_empty());
        let (s, _, _) = r.relay(&json!({"url": BOOK, "body": "x".repeat(MAX_REQUEST_BODY_BYTES)})).await;
        assert_eq!(s, StatusCode::OK, "exactly the limit is carried");

        let why = format!("Betfair's reply was more than {MAX_REPLY_BYTES} bytes; the relay carries at most {MAX_REPLY_BYTES}");
        let r = rig(Fake::new(BetfairOutcome::Fault(why.clone())));
        let (s, h, v) = r.relay(&login_request()).await;
        assert_eq!(s, StatusCode::BAD_GATEWAY);
        assert_eq!(h[ERROR_HEADER], why.as_str());
        assert_eq!(v["reason"], why.as_str());
        let last = r.state.betfair.last_call().unwrap();
        assert_eq!((last.http_code, last.fault.as_deref()), (502, Some(why.as_str())));
        // A reply that is not text cannot travel unchanged in the envelope: a relay fault, said so.
        let r = rig(Fake::new(BetfairOutcome::Reply { status: 200, content_type: None, body: vec![0xff, 0xfe, 0x00] }));
        let (s, h, _) = r.relay(&login_request()).await;
        assert_eq!(s, StatusCode::BAD_GATEWAY);
        assert!(h[ERROR_HEADER].to_str().unwrap().contains("not UTF-8"), "{h:?}");
    }

    #[tokio::test]
    async fn timeout_is_502_with_relay_error_header() {
        let fake = Fake::slow(json_reply(200, "{}"), Duration::from_secs(30));
        let r = rig(fake.clone());
        r.state.betfair.set_timeout(Duration::from_millis(50));
        let started = Instant::now();
        let (s, h, v) = r.relay(&login_request()).await;
        assert!(started.elapsed() < Duration::from_secs(10), "the relay gave up at its own limit");
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{v}");
        let header = h[ERROR_HEADER].to_str().unwrap();
        assert!(header.contains("no reply from Betfair within 50 ms"), "{header}");
        assert_eq!(v["reason"], header);
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(TIMEOUT, Duration::from_secs(14), "below the television's 15 s read timeout");
    }

    #[tokio::test]
    async fn relay_never_logs_bodies_or_credentials() {
        let fake = Fake::new(json_reply(200, LOGIN_REPLY));
        let r = rig(fake.clone());
        let (s, _, _) = r.relay(&login_request()).await;
        assert_eq!(s, StatusCode::OK);
        let book = json!({"url": BOOK, "headers": [["X-Application", APP_KEY], ["X-Authentication", TOKEN]], "body": "{\"marketIds\":[\"1.2\"]}"});
        let (s, _, _) = r.relay(&book).await;
        assert_eq!(s, StatusCode::OK);
        // Allowed host, a secret as the path's last segment: relayed, and printed only as "other".
        let pathed = json!({"url": format!("https://api.betfair.com/exchange/betting/rest/v1.0/{TOKEN}/"), "body": "{}"});
        let (s, _, _) = r.relay(&pathed).await;
        assert_eq!(s, StatusCode::OK);
        // Refused, with the secrets in the request: they must not reach the refusal's line either.
        let (s, _, v) = r.relay(&json!({"url": format!("https://{PASSWORD}@api.betfair.com/x?token={TOKEN}"), "headers": [["X-Application", APP_KEY]]})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, w) = r.relay(&json!({"url": format!("https://evil.example/?password={PASSWORD}&token={TOKEN}")})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // No scheme: the parser reads the user name as the scheme, and it must not be printed.
        let (s, _, x) = r.relay(&json!({"url": "relaytester:secretpw@api.betfair.com/x"})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(x["reason"].as_str().is_some_and(|r| !r.is_empty()), "the caller still gets a reason: {x}");
        // Nor a method the caller made up.
        let (s, _, _) = r.relay(&json!({"url": LOGIN, "method": "secretpw-method"})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // A fault, too.
        let r2 = rig(Fake::new(BetfairOutcome::Fault("no reply from Betfair within 14 s".into())));
        let (s, _, _) = r2.relay(&login_request()).await;
        assert_eq!(s, StatusCode::BAD_GATEWAY);

        let logged = LOGGED.lock().unwrap().clone();
        assert!(logged.iter().any(|l| l == &log_line("identitysso.betfair.com", "login", "HTTP 200", 0) || l.starts_with("betfair relay: identitysso.betfair.com login: HTTP 200 in ")), "{logged:?}");
        assert!(logged.iter().any(|l| l.starts_with("betfair relay: api.betfair.com listMarketBook: HTTP 200 in ")), "{logged:?}");
        assert!(logged.iter().any(|l| l.starts_with("betfair relay: api.betfair.com other: HTTP 200 in ")), "{logged:?}");
        assert!(logged.iter().any(|l| l == "betfair relay refused (400): not an allowed Betfair address"), "{logged:?}");
        assert!(logged.iter().any(|l| l == "betfair relay refused (400): not a POST"), "{logged:?}");
        let page = crate::status_page::render(&r.state.page_view(), true);
        let mut everything: Vec<String> = logged;
        everything.push(page);
        everything.push(v.to_string());
        everything.push(w.to_string());
        for text in &everything {
            for secret in [PASSWORD, APP_KEY, TOKEN, "relaytester", "secretpw", "marketIds", "username="] {
                assert!(!text.contains(secret), "'{secret}' leaked into: {text}");
            }
        }
        // Nothing of the call is written to the data folder.
        let mut found = Vec::new();
        let mut stack = vec![r._dir.0.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(bytes) = std::fs::read(&p) {
                    let text = String::from_utf8_lossy(&bytes);
                    if [PASSWORD, APP_KEY, TOKEN].iter().any(|s| text.contains(s)) {
                        found.push(p.display().to_string());
                    }
                }
            }
        }
        assert!(found.is_empty(), "written to disk: {found:?}");
    }

    #[tokio::test]
    async fn relay_requires_auth() {
        let fake = Fake::new(json_reply(200, "{}"));
        let r = rig(fake.clone());
        let plain = login_request().to_string();
        // No nonce or proof.
        let req = Request::builder().method("POST").uri(PATH).header("x-aurelis-companion", "1").body(Body::from(seal::seal(plain.as_bytes(), &SECRET))).unwrap();
        let (s, _, _) = r.send(req).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        // The right nonce, the wrong household.
        let req = Request::builder().uri("/v1/challenge").header("x-aurelis-companion", "1").body(Body::empty()).unwrap();
        let (_, _, b) = r.send(req).await;
        let nonce = serde_json::from_slice::<Value>(&b).unwrap()["nonce"].as_str().unwrap().to_string();
        let other = [8u8; 32];
        let sealed = seal::seal(plain.as_bytes(), &other);
        let p = proof::proof(&other, &unb64(&nonce).unwrap(), "POST", PATH, &sealed);
        let req = Request::builder()
            .method("POST")
            .uri(PATH)
            .header("x-aurelis-companion", "1")
            .header("x-aurelis-nonce", nonce)
            .header("x-aurelis-proof", b64(&p))
            .body(Body::from(sealed))
            .unwrap();
        let (s, _, _) = r.send(req).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        // An unpaired Companion relays nothing.
        let unpaired = Arc::new(test_state(test_config()));
        let fake2 = Fake::new(json_reply(200, "{}"));
        unpaired.betfair.set_sender(fake2.clone());
        let req = Request::builder().method("POST").uri(PATH).header("x-aurelis-companion", "1").body(Body::from(plain.clone())).unwrap();
        let res = crate::router(unpaired).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(fake.calls().is_empty() && fake2.calls().is_empty());
    }

    #[tokio::test]
    async fn relay_off_is_503_and_card_declines_it() {
        let fake = Fake::new(json_reply(200, "{}"));
        let r = rig_with(false, fake.clone());
        let (s, h, v) = r.relay(&login_request()).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
        assert_eq!(v["reason"], RELAY_OFF_REASON);
        assert!(h.get(ERROR_HEADER).is_none());
        assert!(fake.calls().is_empty());
        let card: Value = serde_json::from_str(&r.state.card_json().unwrap()).unwrap();
        assert_eq!(card["capabilities"][CAPABILITY], false);
        assert_eq!(card["declined"][CAPABILITY], RELAY_OFF_REASON);
        let page = crate::status_page::render(&r.state.page_view(), true);
        assert!(page.contains("Betfair relay: off"), "the status page says so");

        // On by default: the card offers it and declines nothing.
        let on = rig(Fake::new(json_reply(200, "{}")));
        let card: Value = serde_json::from_str(&on.state.card_json().unwrap()).unwrap();
        assert_eq!(card["capabilities"][CAPABILITY], true);
        assert!(card["declined"].get(CAPABILITY).is_none(), "{card}");
        assert!(crate::status_page::render(&on.state.page_view(), true).contains("Betfair relay: on \u{b7} no calls yet"));
        // Paused, the card reads busy, the relay is declined with the pause's reason and the route agrees.
        on.state.set_paused(true);
        let card: Value = serde_json::from_str(&on.state.card_json().unwrap()).unwrap();
        assert_eq!(card["capabilities"][CAPABILITY], false);
        assert_eq!(card["declined"][CAPABILITY], crate::state::PAUSED_RUNNER_REASON);
        let (s, _, v) = on.relay(&login_request()).await;
        assert_eq!((s, v["reason"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some(crate::state::PAUSED_RUNNER_REASON)));
    }

    #[test]
    fn call_names_are_known_betfair_calls_or_other() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(call_name(&u(BOOK)), "listMarketBook");
        assert_eq!(call_name(&u(LOGIN)), "login");
        for c in KNOWN_CALLS {
            assert_eq!(call_name(&u(&format!("https://api.betfair.com/exchange/betting/rest/v1.0/{c}/"))), c);
        }
        assert_eq!(call_name(&u("https://api.betfair.com/")), "other");
        assert_eq!(call_name(&u("https://api.betfair.com/a/b%20c<d>?x=1")), "other");
        assert_eq!(call_name(&u("https://api.betfair.com/x/listmarketbook")), "other", "exact names only");
        assert_eq!(call_name(&u(&format!("https://api.betfair.com/x/{TOKEN}"))), "other");
    }
}
