//! The AI job runner: makes the provider calls televisions handed over, by the rules
//! `AiChatClient` follows on the box (CLAUDE.md, "Free AI always, paid last resort").
//!
//! * Attempts are walked in the television's order (it already put the most generous free
//!   provider first). One HTTP call at a time, each with a 60 s limit.
//! * A busy or timed-out provider (`FailureClass::Retry`) is asked again before anything else: the
//!   pass stops for that job and it is tried again after 30 s, 60 s, 2 min … (at most 15 min), or
//!   after the provider's own `Retry-After`. A provider still busy on its second ask in a row
//!   gives way to the next attempt, so one jammed provider cannot hold a job until it expires.
//! * Out of credit (402, exhausted 429) or a refused key (401/403) parks that provider and key for
//!   six hours, for every job; the walk moves on.
//! * A paid attempt is sent only when the job allows paying **and** its free-slot wait (at least
//!   3 h, from the television's `dueAtMs`) has passed. The Companion never widens what a job allowed.
//! * Any 2xx answers the job with the provider's raw body; the television parses it.
//!
//! Keys exist only in memory, opened from the sealed job for the pass. What is recorded for an
//! attempt is the provider's words with every key of the job replaced and then scrubbed; log lines
//! name the provider, label, outcome and HTTP code only.

use crate::ai_failure::{classify, FailureClass};
use crate::ai_jobs::{AttemptOutcome, AttemptRecord, JobHeader, JobResult, JobStore, Outcome, RunnableJob};
use crate::glm_jwt::glm_token;
use crate::guide_store::{describe_err, iso_utc, sha_hex};
use crate::state::{AppState, WallClock};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// How often the runner looks for work.
pub const PASS_EVERY: Duration = Duration::from_secs(15);
/// The limit on one provider call.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a provider out of credit, or refusing its key, is left alone.
pub const PARK_MS: u64 = 6 * 3_600_000;
pub const BACKOFF_FIRST_MS: u64 = 30_000;
pub const BACKOFF_CAP_MS: u64 = 15 * 60_000;
/// The largest provider reply kept (a chat answer is kilobytes; this is a guard).
pub const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;
const KEY_REMOVED: &str = "[key removed]";

/// What one HTTP call came back with. `http_code` is `None` when no reply arrived at all.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SendOutcome {
    pub http_code: Option<u16>,
    pub body: String,
    pub timed_out: bool,
    /// The provider's `Retry-After`, in milliseconds, when it sent one in seconds.
    pub retry_after_ms: Option<u64>,
    /// The reply was larger than [`MAX_REPLY_BYTES`] and was not read; `body` says so. Asking
    /// again would bring the same reply, so the attempt fails.
    pub oversized: bool,
}

/// Makes one provider call. Production uses [`ReqwestSender`]; tests use a fake.
pub trait Sender: Send + Sync {
    fn send(&self, url: &str, headers: &[(String, String)], body: &str, timeout: Duration) -> impl Future<Output = SendOutcome> + Send;
}

/// Whether the household already holds an answer for a job's cache key: asked before any paid
/// attempt, so a question answered elsewhere in the house is never bought again.
pub trait HouseAnswers: Send + Sync {
    fn holds(&self, cache_key: &str) -> impl Future<Output = bool> + Send;
}

/// The Companion's own AI store, keyed exactly as televisions offer their answers
/// (`CompanionAiLookup.offer`: the outbound key, which is the job's `cacheKey`).
impl HouseAnswers for crate::store::BlobStore {
    async fn holds(&self, cache_key: &str) -> bool {
        match crate::ai_store::get(self, cache_key).await {
            Ok(Some((v, _))) => match v {
                Value::Null => false,
                Value::String(s) => !s.trim().is_empty(),
                _ => true,
            },
            _ => false,
        }
    }
}

/// No household store to ask (tests of the walk alone).
#[cfg(test)]
pub struct NoHouse;

#[cfg(test)]
impl HouseAnswers for NoHouse {
    async fn holds(&self, _cache_key: &str) -> bool {
        false
    }
}

/// The reason a job ends when, at its paid step, the household already holds its answer.
pub const ANSWERED_ELSEWHERE: &str = "answered elsewhere in the house: the Companion's AI store already holds this answer, so no paid provider was asked";

/// The real thing: rustls, no redirects (an auth header must never follow a provider elsewhere).
pub struct ReqwestSender {
    client: reqwest::Client,
}

impl ReqwestSender {
    pub fn new() -> Result<ReqwestSender, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("AurelisCompanion/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("the HTTP client could not be built: {e}"))?;
        Ok(ReqwestSender { client })
    }
}

impl Sender for ReqwestSender {
    async fn send(&self, url: &str, headers: &[(String, String)], body: &str, timeout: Duration) -> SendOutcome {
        let mut req = self.client.post(url).timeout(timeout).body(body.to_string());
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let timed_out = e.is_timeout();
                let body = if timed_out { format!("no reply within {} s", timeout.as_secs()) } else { format!("could not reach the provider: {}", describe_err(e)) };
                return SendOutcome { http_code: None, body, timed_out, ..Default::default() };
            }
        };
        let code = resp.status().as_u16();
        let retry_after_ms = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|secs| secs.saturating_mul(1000));
        let too_large = |n: String| SendOutcome {
            http_code: Some(code),
            body: format!("the reply was {n} bytes; the largest kept is {MAX_REPLY_BYTES}"),
            timed_out: false,
            retry_after_ms,
            oversized: true,
        };
        // Refused before reading when the provider says how big it is; otherwise read in chunks
        // and stop at the limit, so a large body is never held whole.
        if let Some(len) = resp.content_length().filter(|n| *n > MAX_REPLY_BYTES as u64) {
            return too_large(len.to_string());
        }
        let mut resp = resp;
        let mut got: Vec<u8> = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if got.len() + chunk.len() > MAX_REPLY_BYTES {
                        return too_large(format!("more than {MAX_REPLY_BYTES}"));
                    }
                    got.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    let timed_out = e.is_timeout();
                    let body = if timed_out { format!("the reply did not finish within {} s", timeout.as_secs()) } else { format!("the reply was cut off: {}", describe_err(e)) };
                    return SendOutcome { http_code: None, body, timed_out, ..Default::default() };
                }
            }
        }
        SendOutcome { http_code: Some(code), body: String::from_utf8_lossy(&got).into_owned(), timed_out: false, retry_after_ms, oversized: false }
    }
}

/// One provider attempt as the television described it. Held in memory for one pass only.
struct Attempt {
    provider: String,
    model: String,
    /// A missing `free` is read as paid: the Companion never widens what a job allowed.
    free: bool,
    kind: String,
    url: String,
    scheme: String,
    key: String,
    body: String,
}

fn parse_attempts(plain: &[u8]) -> Result<Vec<Attempt>, String> {
    let v: Value = serde_json::from_slice(plain).map_err(|e| format!("the opened job is not valid JSON: {e}"))?;
    let list = v.get("attempts").and_then(Value::as_array).filter(|a| !a.is_empty()).ok_or("the opened job holds no attempts")?;
    let text = |a: &Value, k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut out = Vec::with_capacity(list.len());
    for (i, a) in list.iter().enumerate() {
        let auth = a.get("auth").cloned().unwrap_or(Value::Null);
        let attempt = Attempt {
            provider: text(a, "provider"),
            model: text(a, "model"),
            free: a.get("free").and_then(Value::as_bool).unwrap_or(false),
            kind: text(a, "kind"),
            url: text(a, "url"),
            scheme: text(&auth, "scheme"),
            key: text(&auth, "key"),
            body: text(a, "body"),
        };
        if attempt.provider.is_empty() || attempt.url.is_empty() || attempt.key.is_empty() {
            return Err(format!("attempt {i} of the opened job is incomplete"));
        }
        out.push(attempt);
    }
    Ok(out)
}

/// A park is per provider and key, so a rotated key is tried again at once. The key itself is
/// never held here: only its SHA-256.
fn park_key(a: &Attempt) -> String {
    format!("{}:{}", a.provider, sha_hex(&a.key))
}

/// The headers for one call; a GLM token is made now, for the clock at send time.
fn headers_for(a: &Attempt, now_ms: u64) -> Vec<(String, String)> {
    let mut h = vec![("Content-Type".to_string(), "application/json".to_string()), ("Accept-Language".to_string(), "en-US,en".to_string())];
    match a.scheme.as_str() {
        "glm-jwt" => h.push(("Authorization".to_string(), format!("Bearer {}", glm_token(&a.key, now_ms)))),
        "goog-api-key" => h.push(("x-goog-api-key".to_string(), a.key.clone())),
        _ => h.push(("Authorization".to_string(), format!("Bearer {}", a.key))),
    }
    h
}

/// Replaces every key of the job (and each half of an `id.secret` key, and any token made from
/// one) wherever a provider echoed it. The store scrubs again on top of this.
fn without_keys(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    let mut all: Vec<&str> = secrets.iter().map(String::as_str).filter(|s| s.len() >= 4).collect();
    // Longest first, so a whole key goes before a half of it.
    all.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for s in all {
        if out.contains(s) {
            out = out.replace(s, KEY_REMOVED);
        }
    }
    out
}

fn secrets_of(attempts: &[Attempt]) -> Vec<String> {
    let mut out = Vec::new();
    for a in attempts {
        out.push(a.key.clone());
        if let Some((id, secret)) = a.key.split_once('.') {
            for half in [id, secret] {
                if half.len() >= 8 {
                    out.push(half.to_string());
                }
            }
        }
    }
    out
}

/// `09:30 UTC`.
fn hh_mm(ms: u64) -> String {
    format!("{} UTC", &iso_utc(ms as i64)[11..16])
}

#[derive(Clone, Copy)]
struct Park {
    until_ms: u64,
    why: &'static str,
}

#[derive(Default)]
struct Wait {
    next_try_ms: u64,
    /// Passes ended by a busy provider, for the back-off ladder.
    retries: u32,
    /// Attempts (by index) already asked again once after being busy.
    reasked: HashSet<usize>,
    /// Attempts (by index) the provider refused as a request (a plain failure, e.g. a 400 for an
    /// unknown model), with what it said: sending them again would bring the same answer.
    failed: HashMap<usize, String>,
}

/// What the runner remembers between passes, in memory only: parked provider keys and each job's
/// next-try time. A restart forgets both, so a job is simply tried again at once.
#[derive(Default)]
pub struct RunnerMemory {
    parks: Mutex<HashMap<String, Park>>,
    waits: Mutex<HashMap<String, Wait>>,
}

impl RunnerMemory {
    fn parks(&self) -> MutexGuard<'_, HashMap<String, Park>> {
        self.parks.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn waits(&self) -> MutexGuard<'_, HashMap<String, Wait>> {
        self.waits.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ready(&self, id: &str, now_ms: u64) -> bool {
        self.waits().get(id).map_or(true, |w| now_ms >= w.next_try_ms)
    }

    fn parked(&self, key: &str, now_ms: u64) -> Option<Park> {
        let mut parks = self.parks();
        match parks.get(key).copied() {
            Some(p) if now_ms < p.until_ms => Some(p),
            Some(_) => {
                parks.remove(key);
                None
            }
            None => None,
        }
    }

    fn park(&self, key: String, until_ms: u64, why: &'static str) {
        self.parks().insert(key, Park { until_ms, why });
    }

    /// Asks the job's memory whether attempt `index` has had its one re-ask; marks it if not.
    fn take_reask(&self, id: &str, index: usize) -> bool {
        self.waits().entry(id.to_string()).or_default().reasked.insert(index)
    }

    fn clear_reask(&self, id: &str, index: usize) {
        if let Some(w) = self.waits().get_mut(id) {
            w.reasked.remove(&index);
        }
    }

    fn failed_earlier(&self, id: &str, index: usize) -> Option<String> {
        self.waits().get(id).and_then(|w| w.failed.get(&index).cloned())
    }

    fn remember_failed(&self, id: &str, index: usize, said: &str) {
        self.waits().entry(id.to_string()).or_default().failed.insert(index, said.to_string());
    }

    /// The next back-off step for a job: when it may be tried again.
    fn back_off(&self, id: &str, now_ms: u64, retry_after_ms: Option<u64>) -> u64 {
        let mut waits = self.waits();
        let w = waits.entry(id.to_string()).or_default();
        w.retries += 1;
        let ladder = BACKOFF_FIRST_MS.saturating_mul(1u64 << (w.retries - 1).min(20)).min(BACKOFF_CAP_MS);
        let delay = retry_after_ms.map(|r| r.min(PARK_MS)).unwrap_or(ladder);
        w.next_try_ms = now_ms + delay;
        w.next_try_ms
    }

    fn wait_until(&self, id: &str, at_ms: u64) {
        self.waits().entry(id.to_string()).or_default().next_try_ms = at_ms;
    }

    fn forget(&self, id: &str) {
        self.waits().remove(id);
    }

    /// Unpair or a new household: nothing remembered about the old one applies.
    pub fn clear(&self) {
        self.parks().clear();
        self.waits().clear();
    }
}

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RunReport {
    pub taken: usize,
    pub sent: usize,
    pub answered: usize,
    pub failed: usize,
    pub expired: usize,
    pub waiting: usize,
    activity: Vec<(i64, String)>,
}

impl RunReport {
    fn note(&mut self, clock: &WallClock, line: String) {
        log(line.clone());
        self.activity.push((clock(), line));
    }
}

fn now_of(clock: &WallClock) -> u64 {
    clock().max(0) as u64
}

fn log_line(provider: &str, label: &str, outcome: &str, code: Option<u16>) -> String {
    let code = code.map(|c| c.to_string()).unwrap_or_else(|| "none".to_string());
    format!("ai runner: {provider} for '{label}': {outcome} (HTTP {code})")
}

/// Every line the runner prints goes through here: provider, label, outcome and HTTP code, or a
/// fixed sentence of the runner's own — never a provider's words. Tests read what was printed.
fn log(line: String) {
    #[cfg(test)]
    tests::LOGGED.lock().unwrap_or_else(|e| e.into_inner()).push(line.clone());
    println!("{line}");
}

/// One pass over the runnable jobs: each job at most once, one call at a time. (Production
/// passes go through [`run_once_while`], which also stops for a pause.)
#[cfg(test)]
pub async fn run_once<S: Sender>(memory: &RunnerMemory, store: &JobStore, secret: &[u8; 32], sender: &S, clock: &WallClock) -> RunReport {
    run_once_while(memory, store, secret, sender, &NoHouse, clock, || true).await
}

/// As [`run_once`], asking `go` before each job: a pause (or the runner switched off) stops the
/// pass between jobs rather than at the next one.
pub async fn run_once_while<S: Sender, H: HouseAnswers>(memory: &RunnerMemory, store: &JobStore, secret: &[u8; 32], sender: &S, house: &H, clock: &WallClock, go: impl Fn() -> bool) -> RunReport {
    let mut report = RunReport::default();
    let mut taken: HashSet<String> = HashSet::new();
    while go() {
        let now = now_of(clock);
        let Some(job) = store.next_runnable_where(now, |id| !taken.contains(id) && memory.ready(id, now)) else { break };
        taken.insert(job.id.clone());
        report.taken += 1;
        run_job(memory, store, secret, sender, house, clock, job, &mut report).await;
    }
    report
}

fn earliest(slot: &mut Option<(u64, String)>, at: u64, why: String) {
    if slot.as_ref().map_or(true, |(t, _)| at < *t) {
        *slot = Some((at, why));
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_job<S: Sender, H: HouseAnswers>(memory: &RunnerMemory, store: &JobStore, secret: &[u8; 32], sender: &S, house: &H, clock: &WallClock, job: RunnableJob, report: &mut RunReport) {
    let id = job.id.clone();
    let header: JobHeader = job.header.clone();
    let label = header.label.clone();
    // `logged` is what may be printed: the provider, outcome and code, or a fixed sentence of our
    // own. `why` (the provider's words) goes only to the store, which scrubs it.
    let fail = |report: &mut RunReport, why: String, logged: String| {
        report.note(clock, logged);
        store.finish(&id, Outcome::Failed(why));
        memory.forget(&id);
        report.failed += 1;
    };
    let Some(plain) = job.open(secret) else {
        let why = "the sealed job could not be opened with this household's secret".to_string();
        return fail(report, why.clone(), format!("ai runner: job '{label}' failed: {why}"));
    };
    let attempts = match parse_attempts(&plain) {
        Ok(a) => a,
        Err(why) => return fail(report, why, format!("ai runner: job '{label}' failed: the opened job could not be read")),
    };
    drop(plain);
    let secrets = secrets_of(&attempts);
    let paid_from = header.due_at_ms.saturating_add(header.free_slot_wait_ms);
    let before_expiry = |t: u64| header.expires_at_ms == 0 || t < header.expires_at_ms;
    // What the last provider asked said, and who it was (for the log).
    let mut last_reason: Option<String> = None;
    let mut last_heard: (String, Option<u16>) = (String::new(), None);
    // The earliest time a skipped attempt could be sent (a park ends, the paid wait is over), and why.
    let mut wake: Option<(u64, String)> = None;
    let mut paid_not_allowed = false;
    let mut paid_wait_past_expiry = false;
    let mut retry_pending: Option<Option<u64>> = None;

    for (index, a) in attempts.iter().enumerate() {
        let now = now_of(clock);
        if header.expires_at_ms > 0 && now >= header.expires_at_ms {
            let why = match &last_reason {
                Some(r) => format!("expired before it could be answered (last: {r})"),
                None => "expired before it could be answered".to_string(),
            };
            report.note(clock, log_line(&a.provider, &label, "expired", None));
            store.finish(&id, Outcome::Expired(why));
            memory.forget(&id);
            report.expired += 1;
            return;
        }
        let record = |outcome: AttemptOutcome, http_code: Option<u16>, detail: Option<String>| {
            let detail = detail.map(|d| without_keys(&d, &secrets));
            store.record_attempt(&id, AttemptRecord { provider: a.provider.clone(), free: a.free, at_ms: now, outcome, http_code, detail });
        };
        let pk = park_key(a);
        if let Some(p) = memory.parked(&pk, now) {
            let why = format!("parked until {} ({})", hh_mm(p.until_ms), p.why);
            record(AttemptOutcome::Skipped, None, Some(why.clone()));
            last_reason = Some(format!("{}: {why}", a.provider));
            if before_expiry(p.until_ms) {
                earliest(&mut wake, p.until_ms, format!("waiting for {}: {why}", a.provider));
            }
            continue;
        }
        if let Some(said) = memory.failed_earlier(&id, index) {
            record(AttemptOutcome::Skipped, None, Some(format!("failed earlier: {said}")));
            last_reason = Some(format!("{}: {said}", a.provider));
            continue;
        }
        if !a.free {
            if !header.paid_allowed {
                record(AttemptOutcome::Skipped, None, Some("this job does not allow a paid provider".to_string()));
                paid_not_allowed = true;
                continue;
            }
            if now < paid_from {
                let why = format!("waiting for a free provider until {}", hh_mm(paid_from));
                record(AttemptOutcome::Skipped, None, Some(why.clone()));
                if before_expiry(paid_from) {
                    earliest(&mut wake, paid_from, why);
                } else {
                    paid_wait_past_expiry = true;
                }
                continue;
            }
            // Never buy what the house already holds: another television (or an earlier answer)
            // may have put it in the AI store since this job was handed over.
            if house.holds(&header.cache_key).await {
                record(AttemptOutcome::Skipped, None, Some(ANSWERED_ELSEWHERE.to_string()));
                report.note(clock, format!("ai runner: job '{label}' answered elsewhere in the house; no paid call"));
                store.finish(&id, Outcome::Failed(ANSWERED_ELSEWHERE.to_string()));
                memory.forget(&id);
                report.failed += 1;
                return;
            }
        }

        let headers = headers_for(a, now);
        let mut call_secrets = secrets.clone();
        call_secrets.extend(headers.iter().filter(|(k, _)| k == "Authorization").map(|(_, v)| v.trim_start_matches("Bearer ").to_string()));
        let out = sender.send(&a.url, &headers, &a.body, CALL_TIMEOUT).await;
        drop(headers);
        report.sent += 1;
        let at = now_of(clock);
        last_heard = (a.provider.clone(), out.http_code);

        if let (Some(code @ 200..=299), false) = (out.http_code, out.oversized) {
            report.note(clock, log_line(&a.provider, &label, "answered", Some(code)));
            record(AttemptOutcome::Answered, Some(code), None);
            store.finish(&id, Outcome::Done(JobResult { provider: a.provider.clone(), model: a.model.clone(), kind: a.kind.clone(), http_code: code, body: out.body, answered_at_ms: at }));
            memory.forget(&id);
            report.answered += 1;
            return;
        }

        let words = without_keys(&out.body, &call_secrets);
        let said = match out.http_code {
            Some(code) => format!("HTTP {code}: {words}"),
            None => words,
        };
        // Ruling R6: no reply at all (DNS, connect, reset, timeout) is asked again with back-off;
        // a reply too large to keep fails, since asking again brings the same reply.
        let class = if out.oversized {
            FailureClass::Failed
        } else if out.http_code.is_none() {
            FailureClass::Retry
        } else {
            classify(out.http_code, &out.body, out.timed_out)
        };
        match class {
            FailureClass::Retry => {
                report.note(clock, log_line(&a.provider, &label, "busy", out.http_code));
                record(AttemptOutcome::Retry, out.http_code, Some(said.clone()));
                last_reason = Some(format!("{}: {said}", a.provider));
                if memory.take_reask(&id, index) {
                    // Asked again before anything else: this pass ends for the job.
                    retry_pending = Some(out.retry_after_ms);
                    break;
                }
                // Busy on its second ask in a row: the next attempt is tried, and this one is
                // asked again on the next pass.
                memory.clear_reask(&id, index);
                retry_pending = Some(retry_pending.flatten().or(out.retry_after_ms));
            }
            class @ (FailureClass::Park | FailureClass::AuthPark) => {
                let why = if class == FailureClass::Park { "out of credit" } else { "the key was refused" };
                let until = at + PARK_MS;
                memory.park(pk, until, why);
                memory.clear_reask(&id, index);
                report.note(clock, log_line(&a.provider, &label, "parked", out.http_code));
                record(AttemptOutcome::Parked, out.http_code, Some(format!("{said} — parked until {}", hh_mm(until))));
                last_reason = Some(format!("{}: {said}", a.provider));
            }
            FailureClass::Failed => {
                memory.clear_reask(&id, index);
                memory.remember_failed(&id, index, &said);
                report.note(clock, log_line(&a.provider, &label, "failed", out.http_code));
                record(AttemptOutcome::Failed, out.http_code, Some(said.clone()));
                last_reason = Some(format!("{}: {said}", a.provider));
            }
        }
    }

    let now = now_of(clock);
    if let Some(retry_after) = retry_pending {
        let next = memory.back_off(&id, now, retry_after);
        let heard = last_reason.clone().unwrap_or_else(|| "a busy reply".to_string());
        store.requeue(&id, &format!("waiting to ask again at {} after {heard}", hh_mm(next)));
        report.waiting += 1;
        return;
    }
    if let Some((t, why)) = wake {
        memory.wait_until(&id, t);
        store.requeue(&id, &why);
        report.waiting += 1;
        return;
    }
    let why = last_reason
        .or_else(|| paid_wait_past_expiry.then(|| "still waiting for the three-hour free-slot wait at expiry".to_string()))
        .or_else(|| paid_not_allowed.then(|| "paid providers were not allowed for this job".to_string()))
        .unwrap_or_else(|| "every attempt was skipped".to_string());
    let logged = if last_heard.0.is_empty() { format!("ai runner: job '{label}' failed: no provider could be asked") } else { log_line(&last_heard.0, &label, "failed", last_heard.1) };
    fail(report, why, logged);
}

/// Starts the runner: a pass every 15 s while the card offers the runner, the household is paired
/// and the operator has not paused the Companion (asked again between jobs).
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let sender = match ReqwestSender::new() {
            Ok(s) => s,
            Err(why) => {
                eprintln!("AI runner not started: {why}");
                state.push_job(&format!("AI runner not started: {why}"));
                return;
            }
        };
        let memory = RunnerMemory::default();
        let clock: WallClock = {
            let s = state.clone();
            Arc::new(move || s.wall_ms())
        };
        let allowed = |s: &AppState| s.card().capabilities.ai_runner && !s.is_paused();
        let mut household: Option<String> = None;
        let mut tick = tokio::time::interval(PASS_EVERY);
        // A pass that ran long (a minute per slow provider) is followed by a full interval, not a burst.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if !allowed(&state) {
                continue;
            }
            let Some(secret) = state.auth.household_secret() else {
                continue;
            };
            let fingerprint = crate::household::fingerprint(&secret);
            if household.as_deref() != Some(fingerprint.as_str()) {
                memory.clear();
                household = Some(fingerprint);
            }
            let report = run_once_while(&memory, &state.ai_jobs, &secret, &sender, &state.stores.ai, &clock, || allowed(&state)).await;
            for (at, line) in report.activity {
                state.push_job_at(at, &line);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_jobs::{parse_job, JobState};
    use crate::household::{Household, TempDir};
    use crate::state::{test_config, test_state};
    use companion_protocol::seal;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicI64, Ordering};

    const SECRET: [u8; 32] = [7u8; 32];
    const T0: i64 = 1_800_000_000_000;
    const FREE_KEY: &str = "gsk-free-key-that-must-never-be-printed";
    const OTHER_FREE_KEY: &str = "sk-or-second-free-key-never-printed";
    const PAID_KEY: &str = "sk-paid-key-that-must-never-be-printed";
    const GLM_KEY: &str = "glmid1234.glmsecretabcdefgh";
    const GROQ: &str = "https://api.groq.example/openai/v1/chat/completions";
    const ROUTER: &str = "https://openrouter.example/api/v1/chat/completions";
    const PAID: &str = "https://api.paid.example/v1/chat/completions";
    const GLM: &str = "https://open.bigmodel.example/api/paas/v4/chat/completions";
    const HOUR: i64 = 3_600_000;

    /// Every line the runner printed, from every test (they run in parallel, so a test looks for
    /// its own lines and asserts what must be true of all of them).
    pub static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    #[derive(Debug, Clone)]
    struct Call {
        url: String,
        headers: Vec<(String, String)>,
        body: String,
        at_ms: i64,
    }

    /// Replies from a script, per URL; an unscripted URL gets a 400 so a test notices.
    struct FakeSender {
        now: Arc<AtomicI64>,
        replies: Mutex<HashMap<String, VecDeque<SendOutcome>>>,
        calls: Mutex<Vec<Call>>,
        /// Moves the clock on by this much during every call (a slow provider).
        each_call_takes_ms: AtomicI64,
    }

    impl FakeSender {
        fn new(now: Arc<AtomicI64>) -> FakeSender {
            FakeSender { now, replies: Mutex::new(HashMap::new()), calls: Mutex::new(Vec::new()), each_call_takes_ms: AtomicI64::new(0) }
        }
        fn reply(&self, url: &str, code: u16, body: &str) {
            self.replies.lock().unwrap().entry(url.to_string()).or_default().push_back(SendOutcome { http_code: Some(code), body: body.to_string(), ..Default::default() });
        }
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
        fn calls_to(&self, url: &str) -> usize {
            self.calls().iter().filter(|c| c.url == url).count()
        }
    }

    impl Sender for FakeSender {
        async fn send(&self, url: &str, headers: &[(String, String)], body: &str, timeout: Duration) -> SendOutcome {
            assert_eq!(timeout, CALL_TIMEOUT);
            self.calls.lock().unwrap().push(Call { url: url.into(), headers: headers.to_vec(), body: body.into(), at_ms: self.now.load(Ordering::SeqCst) });
            self.now.fetch_add(self.each_call_takes_ms.load(Ordering::SeqCst), Ordering::SeqCst);
            self.replies
                .lock()
                .unwrap()
                .get_mut(url)
                .and_then(VecDeque::pop_front)
                .unwrap_or(SendOutcome { http_code: Some(400), body: format!("unscripted call to {url}"), ..Default::default() })
        }
    }

    struct Rig {
        _dir: TempDir,
        store: JobStore,
        now: Arc<AtomicI64>,
        clock: WallClock,
        sender: FakeSender,
        memory: RunnerMemory,
    }

    fn rig() -> Rig {
        let dir = TempDir::new();
        let now = Arc::new(AtomicI64::new(T0));
        let clock: WallClock = {
            let n = now.clone();
            Arc::new(move || n.load(Ordering::SeqCst))
        };
        let store = JobStore::open(&dir.0, clock.clone());
        Rig { _dir: dir, store, sender: FakeSender::new(now.clone()), now, clock, memory: RunnerMemory::default() }
    }

    fn attempt(provider: &str, url: &str, free: bool, scheme: &str, key: &str) -> Value {
        json!({"provider":provider,"model":"m-1","free":free,"kind":"openai","url":url,
               "auth":{"scheme":scheme,"key":key},"body":format!("{{\"provider\":\"{provider}\"}}")})
    }

    fn job(paid_allowed: bool, attempts: Vec<Value>) -> Value {
        json!({"cacheKey":"out:v2:week-ahead:2026-11-18","label":"week-ahead","dueAtMs":T0,"expiresAtMs":T0 + 24 * HOUR,
               "paidAllowed":paid_allowed,"freeSlotWaitMs":10_800_000,"attempts":attempts})
    }

    impl Rig {
        fn submit(&self, job: &Value) -> String {
            submit_to(&self.store, job)
        }
        fn advance(&self, ms: i64) {
            self.now.fetch_add(ms, Ordering::SeqCst);
        }
        async fn pass(&self) -> RunReport {
            run_once(&self.memory, &self.store, &SECRET, &self.sender, &self.clock).await
        }
    }

    fn submit_to(store: &JobStore, job: &Value) -> String {
        let plain = job.to_string().into_bytes();
        let header = parse_job(&plain).expect("valid job");
        store.submit(seal::seal(&plain, &SECRET), &header).expect("kept").id
    }

    fn outcomes(store: &JobStore, id: &str) -> Vec<(String, AttemptOutcome, Option<u16>)> {
        store.get(id).unwrap().attempts.iter().map(|a| (a.provider.clone(), a.outcome, a.http_code)).collect()
    }

    fn header<'a>(call: &'a Call, name: &str) -> Option<&'a str> {
        call.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    #[tokio::test]
    async fn free_attempt_answers_and_job_is_done() {
        let r = rig();
        let id = r.submit(&job(true, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY), attempt("paid", PAID, false, "bearer", PAID_KEY)]));
        let raw = r#"{"choices":[{"message":{"content":"Hello"}}],"usage":{"total_tokens":9}}"#;
        r.sender.reply(GROQ, 200, raw);
        let report = r.pass().await;
        assert_eq!((report.taken, report.sent, report.answered), (1, 1, 1));
        let s = r.store.get(&id).unwrap();
        assert_eq!(s.state, JobState::Done);
        let result = s.result.expect("result");
        assert_eq!((result.provider.as_str(), result.model.as_str(), result.kind.as_str(), result.http_code), ("groq", "m-1", "openai", 200));
        assert_eq!(result.body, raw, "the raw body is kept as the provider sent it");
        assert_eq!(outcomes(&r.store, &id), vec![("groq".into(), AttemptOutcome::Answered, Some(200))]);
        let calls = r.sender.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].body, "{\"provider\":\"groq\"}");
        assert_eq!(header(&calls[0], "Authorization"), Some(format!("Bearer {FREE_KEY}").as_str()));
        assert_eq!(header(&calls[0], "Content-Type"), Some("application/json"));
        assert_eq!(header(&calls[0], "Accept-Language"), Some("en-US,en"));
        // A finished job is not taken again.
        assert_eq!(r.pass().await.taken, 0);
    }

    #[tokio::test]
    async fn busy_free_provider_is_asked_again_before_paid() {
        let r = rig();
        let id = r.submit(&job(true, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY), attempt("paid", PAID, false, "bearer", PAID_KEY)]));
        r.sender.reply(GROQ, 503, "overloaded");
        r.sender.reply(GROQ, 200, "{\"ok\":1}");
        let first = r.pass().await;
        assert_eq!((first.sent, first.waiting), (1, 1));
        assert_eq!(r.store.get(&id).unwrap().state, JobState::Queued);
        // Not before its back-off is up.
        r.advance(29_000);
        assert_eq!(r.pass().await.taken, 0);
        r.advance(1_000);
        let second = r.pass().await;
        assert_eq!(second.answered, 1);
        assert_eq!(r.store.get(&id).unwrap().state, JobState::Done);
        assert_eq!(r.sender.calls_to(GROQ), 2);
        assert_eq!(r.sender.calls_to(PAID), 0, "paid is never sent while a free provider may still answer");
        assert_eq!(outcomes(&r.store, &id), vec![("groq".into(), AttemptOutcome::Retry, Some(503)), ("groq".into(), AttemptOutcome::Answered, Some(200))]);
    }

    #[tokio::test]
    async fn back_off_climbs_and_honours_retry_after() {
        let r = rig();
        let id = r.submit(&job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY)]));
        r.sender.replies.lock().unwrap().entry(GROQ.into()).or_default().push_back(SendOutcome { http_code: Some(429), body: "slow down".into(), retry_after_ms: Some(120_000), ..Default::default() });
        r.pass().await;
        r.advance(119_000);
        assert_eq!(r.pass().await.taken, 0, "Retry-After is honoured");
        r.advance(1_000);
        r.sender.replies.lock().unwrap().entry(GROQ.into()).or_default().push_back(SendOutcome { http_code: None, body: "no reply".into(), timed_out: true, ..Default::default() });
        r.pass().await;
        // Second pass ended by a busy provider: 60 s.
        r.advance(59_000);
        assert_eq!(r.pass().await.taken, 0);
        r.advance(1_000);
        r.sender.reply(GROQ, 200, "{}");
        assert_eq!(r.pass().await.answered, 1);
        assert_eq!(r.store.get(&id).unwrap().state, JobState::Done);
    }

    #[tokio::test]
    async fn exhausted_429_parks_six_hours_and_moves_to_next_free() {
        let r = rig();
        let attempts = vec![attempt("glm", GROQ, true, "bearer", FREE_KEY), attempt("openrouter", ROUTER, true, "bearer", OTHER_FREE_KEY)];
        let id = r.submit(&job(false, attempts.clone()));
        r.sender.reply(GROQ, 429, r#"{"error":{"code":"1113","message":"insufficient balance"}}"#);
        r.sender.reply(ROUTER, 200, "{\"a\":1}");
        let report = r.pass().await;
        assert_eq!((report.sent, report.answered), (2, 1));
        assert_eq!(outcomes(&r.store, &id), vec![("glm".into(), AttemptOutcome::Parked, Some(429)), ("openrouter".into(), AttemptOutcome::Answered, Some(200))]);
        assert_eq!(r.store.get(&id).unwrap().result.unwrap().provider, "openrouter");

        // A second job inside the six hours skips the parked provider without asking it.
        let mut second = job(false, attempts.clone());
        second["cacheKey"] = json!("out:v2:other");
        r.advance(6 * HOUR - 60_000);
        second["dueAtMs"] = json!(r.now.load(Ordering::SeqCst));
        let id2 = r.submit(&second);
        r.sender.reply(ROUTER, 200, "{\"b\":2}");
        r.pass().await;
        assert_eq!(r.sender.calls_to(GROQ), 1, "parked provider not asked within six hours");
        let skipped = &r.store.get(&id2).unwrap().attempts[0];
        assert_eq!(skipped.outcome, AttemptOutcome::Skipped);
        let until = hh_mm((T0 + 6 * HOUR) as u64);
        assert!(skipped.detail.as_deref().unwrap().contains(&until), "skip names the park's end: {:?}", skipped.detail);

        // After six hours it is asked again.
        r.advance(60_000);
        let mut third = job(false, attempts);
        third["cacheKey"] = json!("out:v2:third");
        third["dueAtMs"] = json!(r.now.load(Ordering::SeqCst));
        r.submit(&third);
        r.sender.reply(GROQ, 200, "{}");
        r.pass().await;
        assert_eq!(r.sender.calls_to(GROQ), 2);
    }

    #[tokio::test]
    async fn auth_failure_parks_and_moves_on() {
        let r = rig();
        let id = r.submit(&job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY), attempt("openrouter", ROUTER, true, "bearer", OTHER_FREE_KEY)]));
        r.sender.reply(GROQ, 401, "invalid api key");
        r.sender.reply(ROUTER, 403, "forbidden");
        let report = r.pass().await;
        assert_eq!((report.sent, report.failed), (2, 1), "both keys refused: the job fails, it does not loop");
        let s = r.store.get(&id).unwrap();
        assert_eq!(s.state, JobState::Failed);
        assert_eq!(outcomes(&r.store, &id), vec![("groq".into(), AttemptOutcome::Parked, Some(401)), ("openrouter".into(), AttemptOutcome::Parked, Some(403))]);
        assert!(s.reason.as_deref().unwrap().contains("openrouter"), "reason names the last refusal: {:?}", s.reason);
        assert!(s.attempts[0].detail.as_deref().unwrap().contains("401"));
        // Nothing more is sent for it.
        r.advance(HOUR);
        r.pass().await;
        assert_eq!(r.sender.calls().len(), 2);
    }

    #[tokio::test]
    async fn paid_waits_three_hours_from_due() {
        let r = rig();
        let id = r.submit(&job(true, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY), attempt("paid", PAID, false, "bearer", PAID_KEY)]));
        r.sender.reply(GROQ, 400, "model not found");
        r.pass().await;
        assert_eq!(r.sender.calls_to(PAID), 0);
        let s = r.store.get(&id).unwrap();
        assert_eq!(s.state, JobState::Queued, "waiting for the paid slot, not failed");
        let skip = s.attempts.last().unwrap();
        assert_eq!((skip.provider.as_str(), skip.outcome), ("paid", AttemptOutcome::Skipped));
        let due_plus_3h = hh_mm((T0 + 3 * HOUR) as u64);
        assert_eq!(skip.detail.as_deref(), Some(format!("waiting for a free provider until {due_plus_3h}").as_str()));

        assert_eq!(s.reason.as_deref(), Some(format!("waiting for a free provider until {due_plus_3h}").as_str()), "a waiting job says why");

        // At due + 2 h 59 min, with the runner's memory gone (as after a restart), the job is
        // taken and the paid gate itself refuses.
        r.advance(3 * HOUR - 60_000);
        r.memory.clear();
        r.sender.reply(GROQ, 400, "model not found");
        assert_eq!(r.pass().await.taken, 1, "the job is looked at, not held by a wake time");
        assert_eq!(r.sender.calls_to(PAID), 0, "paid not sent at due + 2h59m");
        assert_eq!(r.store.get(&id).unwrap().attempts.last().unwrap().outcome, AttemptOutcome::Skipped);
        // At due + 3 h the free provider (which failed as a request) is passed over, then paid is sent.
        r.advance(60_000);
        r.sender.reply(PAID, 200, "{\"paid\":true}");
        r.pass().await;
        assert_eq!(r.sender.calls_to(PAID), 1);
        assert_eq!(r.sender.calls_to(GROQ), 2, "a request that failed is not sent again at every wake");
        assert_eq!(r.sender.calls().last().unwrap().at_ms, T0 + 3 * HOUR);
        let s = r.store.get(&id).unwrap();
        let n = s.attempts.len();
        assert_eq!((s.attempts[n - 2].provider.as_str(), s.attempts[n - 2].outcome), ("groq", AttemptOutcome::Skipped));
        assert!(s.attempts[n - 2].detail.as_deref().unwrap().starts_with("failed earlier: HTTP 400"), "{:?}", s.attempts[n - 2].detail);
        assert_eq!((s.state, s.result.unwrap().provider.as_str()), (JobState::Done, "paid"));
    }

    #[tokio::test]
    async fn paid_never_sent_when_not_allowed() {
        let r = rig();
        let id = r.submit(&job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY), attempt("paid", PAID, false, "bearer", PAID_KEY)]));
        r.sender.reply(GROQ, 400, "bad request");
        r.advance(4 * HOUR);
        r.pass().await;
        assert_eq!(r.sender.calls_to(PAID), 0);
        let s = r.store.get(&id).unwrap();
        assert_eq!(s.state, JobState::Failed);
        assert_eq!(s.attempts.last().unwrap().detail.as_deref(), Some("this job does not allow a paid provider"));
        assert!(s.reason.as_deref().unwrap().contains("groq"), "{:?}", s.reason);
    }

    #[tokio::test]
    async fn an_attempt_without_free_is_treated_as_paid() {
        let r = rig();
        let mut a = attempt("mystery", PAID, true, "bearer", PAID_KEY);
        a.as_object_mut().unwrap().remove("free");
        r.submit(&job(false, vec![a]));
        r.pass().await;
        assert_eq!(r.sender.calls().len(), 0);
    }

    #[tokio::test]
    async fn glm_attempt_gets_a_fresh_token_at_send_time() {
        let r = rig();
        r.submit(&job(false, vec![attempt("glm", GLM, true, "glm-jwt", GLM_KEY)]));
        r.advance(5 * 60_000);
        r.sender.reply(GLM, 200, "{}");
        r.pass().await;
        let call = &r.sender.calls()[0];
        let sent = header(call, "Authorization").unwrap();
        assert_eq!(sent, format!("Bearer {}", glm_token(GLM_KEY, call.at_ms as u64)));
        assert_ne!(sent, format!("Bearer {}", glm_token(GLM_KEY, T0 as u64)), "made at send time, not at hand-off");
        assert!(!sent.contains("glmsecret"));
    }

    #[tokio::test]
    async fn gemini_key_goes_in_its_own_header() {
        let r = rig();
        r.submit(&job(false, vec![attempt("gemini", GROQ, true, "goog-api-key", FREE_KEY)]));
        r.sender.reply(GROQ, 200, "{}");
        r.pass().await;
        let call = &r.sender.calls()[0];
        assert_eq!(header(call, "x-goog-api-key"), Some(FREE_KEY));
        assert_eq!(header(call, "Authorization"), None);
    }

    #[tokio::test]
    async fn job_past_expiry_is_expired_with_reason() {
        let r = rig();
        let id = r.submit(&job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY)]));
        r.advance(24 * HOUR);
        r.pass().await;
        assert!(r.sender.calls().is_empty(), "an expired job is never sent");
        let s = r.store.get(&id).unwrap();
        assert_eq!(s.state, JobState::Expired);
        assert!(s.reason.as_deref().unwrap_or("").contains("expired"), "{:?}", s.reason);
    }

    #[tokio::test]
    async fn a_window_that_closes_mid_run_expires_the_job() {
        let r = rig();
        let id = r.submit(&job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY), attempt("openrouter", ROUTER, true, "bearer", OTHER_FREE_KEY)]));
        r.advance(24 * HOUR - 30_000);
        r.sender.each_call_takes_ms.store(60_000, Ordering::SeqCst);
        r.sender.reply(GROQ, 400, "bad request");
        let report = r.pass().await;
        assert_eq!((report.sent, report.expired), (1, 1));
        assert_eq!(r.sender.calls_to(ROUTER), 0, "nothing is sent after the window closes");
        let s = r.store.get(&id).unwrap();
        assert_eq!(s.state, JobState::Expired);
        assert!(s.reason.as_deref().unwrap().contains("groq"), "the reason says what was last heard: {:?}", s.reason);
    }

    #[tokio::test]
    async fn a_provider_busy_twice_in_a_row_gives_way_to_the_next() {
        let r = rig();
        let id = r.submit(&job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY), attempt("openrouter", ROUTER, true, "bearer", OTHER_FREE_KEY)]));
        r.sender.reply(GROQ, 503, "busy");
        r.pass().await;
        assert_eq!(r.sender.calls_to(ROUTER), 0, "the busy provider is asked again first");
        r.advance(30_000);
        r.sender.reply(GROQ, 503, "still busy");
        r.sender.reply(ROUTER, 200, "{}");
        r.pass().await;
        let s = r.store.get(&id).unwrap();
        assert_eq!((s.state, s.result.unwrap().provider.as_str()), (JobState::Done, "openrouter"));
    }

    #[tokio::test]
    async fn a_job_whose_only_provider_is_parked_waits_for_the_park_to_end() {
        let r = rig();
        r.memory.park(format!("groq:{}", sha_hex(FREE_KEY)), (T0 + HOUR) as u64, "out of credit");
        let id = r.submit(&job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY)]));
        r.pass().await;
        assert_eq!(r.store.get(&id).unwrap().state, JobState::Queued);
        r.advance(HOUR - 1);
        assert_eq!(r.pass().await.taken, 0);
        r.advance(1);
        r.sender.reply(GROQ, 200, "{}");
        r.pass().await;
        assert_eq!(r.store.get(&id).unwrap().state, JobState::Done);
    }

    #[tokio::test]
    async fn network_error_is_retried_not_failed() {
        let r = rig();
        let id = r.submit(&job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY), attempt("openrouter", ROUTER, true, "bearer", OTHER_FREE_KEY)]));
        let unreachable = || SendOutcome { http_code: None, body: "could not reach the provider: dns error".into(), ..Default::default() };
        r.sender.replies.lock().unwrap().entry(GROQ.into()).or_default().push_back(unreachable());
        r.pass().await;
        let s = r.store.get(&id).unwrap();
        assert_eq!(s.state, JobState::Queued, "no reply is asked again, not failed");
        assert_eq!(s.attempts[0].outcome, AttemptOutcome::Retry);
        assert!(s.reason.as_deref().unwrap().contains("waiting to ask again"), "{:?}", s.reason);
        assert_eq!(r.sender.calls_to(ROUTER), 0);
        r.advance(30_000);
        r.sender.reply(GROQ, 200, "{}");
        r.pass().await;
        assert_eq!(r.store.get(&id).unwrap().state, JobState::Done);

        // A provider that never answers: the job ends at expiry with the last thing heard.
        let mut second = job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY)]);
        second["cacheKey"] = json!("out:v2:unreachable");
        second["dueAtMs"] = json!(r.now.load(Ordering::SeqCst));
        second["expiresAtMs"] = json!(r.now.load(Ordering::SeqCst) + 10 * 60_000);
        let id2 = r.submit(&second);
        for _ in 0..20 {
            r.sender.replies.lock().unwrap().entry(GROQ.into()).or_default().push_back(unreachable());
            r.pass().await;
            r.advance(60_000);
        }
        r.memory.clear();
        r.pass().await;
        let s = r.store.get(&id2).unwrap();
        assert_eq!(s.state, JobState::Expired);
        assert!(s.attempts.iter().all(|a| a.outcome == AttemptOutcome::Retry), "{:?}", s.attempts);
    }

    #[tokio::test]
    async fn a_job_ended_by_paid_rules_alone_says_why() {
        let r = rig();
        let id = r.submit(&job(false, vec![attempt("paid", PAID, false, "bearer", PAID_KEY)]));
        r.pass().await;
        let s = r.store.get(&id).unwrap();
        assert_eq!((s.state, s.reason.as_deref()), (JobState::Failed, Some("paid providers were not allowed for this job")));

        let mut short = job(true, vec![attempt("paid", PAID, false, "bearer", PAID_KEY)]);
        short["cacheKey"] = json!("out:v2:short");
        short["expiresAtMs"] = json!(T0 + 2 * HOUR);
        let id2 = r.submit(&short);
        r.pass().await;
        let s = r.store.get(&id2).unwrap();
        assert_eq!((s.state, s.reason.as_deref()), (JobState::Failed, Some("still waiting for the three-hour free-slot wait at expiry")));
        assert!(r.sender.calls().is_empty());
    }

    #[tokio::test]
    async fn a_pause_stops_the_pass_between_jobs() {
        let r = rig();
        for k in ["a", "b", "c"] {
            let mut j = job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY)]);
            j["cacheKey"] = json!(format!("out:v2:{k}"));
            r.submit(&j);
        }
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let report = run_once_while(&r.memory, &r.store, &SECRET, &r.sender, &NoHouse, &r.clock, || asked.fetch_add(1, Ordering::SeqCst) < 1).await;
        assert_eq!(report.taken, 1, "paused after the first job");
    }

    #[tokio::test]
    async fn queued_job_shows_what_it_waits_for_on_the_status_page() {
        let dir = TempDir::new();
        Household::create(&dir.0, SECRET).expect("household");
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let now = Arc::new(AtomicI64::new(T0));
        let clock: WallClock = {
            let n = now.clone();
            Arc::new(move || n.load(Ordering::SeqCst))
        };
        let state = test_state(config).with_wall_clock(clock.clone());
        let sender = FakeSender::new(now.clone());
        submit_to(&state.ai_jobs, &job(true, vec![attempt("paid", PAID, false, "bearer", PAID_KEY)]));
        run_once(&RunnerMemory::default(), &state.ai_jobs, &SECRET, &sender, &clock).await;
        let page = crate::status_page::render(&state.page_view(), true);
        let want = format!("waiting for a free provider until {}", hh_mm((T0 + 3 * HOUR) as u64));
        assert!(page.contains("queued") && page.contains(&want), "{page}");
    }

    #[tokio::test]
    async fn a_job_that_will_not_open_fails_with_a_reason() {
        let r = rig();
        let plain = job(false, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY)]).to_string().into_bytes();
        let id = r.store.submit(seal::seal(&plain, &[1u8; 32]), &parse_job(&plain).unwrap()).unwrap().id;
        r.pass().await;
        let s = r.store.get(&id).unwrap();
        assert_eq!(s.state, JobState::Failed);
        assert!(s.reason.unwrap().contains("could not be opened"));
    }

    #[tokio::test]
    async fn logs_and_status_never_carry_the_key() {
        let dir = TempDir::new();
        Household::create(&dir.0, SECRET).expect("household");
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let now = Arc::new(AtomicI64::new(T0));
        let clock: WallClock = {
            let n = now.clone();
            Arc::new(move || n.load(Ordering::SeqCst))
        };
        let state = test_state(config).with_wall_clock(clock.clone());
        let sender = FakeSender::new(now.clone());
        let id = submit_to(&state.ai_jobs, &job(true, vec![
            attempt("groq", GROQ, true, "bearer", FREE_KEY),
            attempt("glm", GLM, true, "glm-jwt", GLM_KEY),
            attempt("openrouter", ROUTER, true, "bearer", OTHER_FREE_KEY),
        ]));
        // Providers echo what they were sent, in every way they do.
        sender.reply(GROQ, 401, &format!("Incorrect API key provided: {FREE_KEY}. You can find your key at …"));
        let token = glm_token(GLM_KEY, T0 as u64);
        sender.reply(GLM, 400, &format!("bad token {token} for glmid1234 with glmsecretabcdefgh"));
        sender.reply(ROUTER, 400, &format!("{{\"error\":\"no such model\",\"echo\":\"{OTHER_FREE_KEY}\"}}"));
        let memory = RunnerMemory::default();
        let secret = state.auth.household_secret().expect("paired");
        run_once(&memory, &state.ai_jobs, &secret, &sender, &clock).await;

        let status = serde_json::to_string(&state.ai_jobs.get(&id).unwrap()).unwrap();
        let page = crate::status_page::render(&state.page_view(), true);
        let mut on_disk = String::new();
        for e in std::fs::read_dir(dir.0.join(crate::ai_jobs::DIR_NAME)).unwrap() {
            let p = e.unwrap().path();
            on_disk.push_str(&String::from_utf8_lossy(&std::fs::read(&p).unwrap()));
        }
        for text in [&status, &page, &on_disk] {
            for secret in [FREE_KEY, OTHER_FREE_KEY, GLM_KEY, "glmsecretabcdefgh", token.as_str()] {
                assert!(!text.contains(secret), "a key leaked: {secret} in {text}");
            }
        }
        assert!(status.contains("401"), "the provider's refusal is still recorded: {status}");
        assert!(page.contains("Recent AI jobs") && page.contains("week-ahead") && page.contains("failed"), "{page}");
        let logged = LOGGED.lock().unwrap().clone();
        for line in &logged {
            for secret in [FREE_KEY, OTHER_FREE_KEY, GLM_KEY, "glmsecretabcdefgh", token.as_str(), "no such model", "Incorrect API key"] {
                assert!(!line.contains(secret), "a log line carries a key or a provider's words: {line}");
            }
        }
        for want in [
            "ai runner: groq for 'week-ahead': parked (HTTP 401)",
            "ai runner: glm for 'week-ahead': failed (HTTP 400)",
        ] {
            assert!(logged.iter().any(|l| l == want), "missing log line {want}: {logged:?}");
        }
        // The job's end is logged by provider and code only, not with its reason.
        assert!(logged.iter().filter(|l| l.as_str() == "ai runner: openrouter for 'week-ahead': failed (HTTP 400)").count() >= 2, "{logged:?}");
    }

    #[test]
    fn without_keys_replaces_whole_keys_before_halves() {
        let s = without_keys("a glmid1234.glmsecretabcdefgh b glmsecretabcdefgh", &secrets_of(&[Attempt {
            provider: "glm".into(),
            model: String::new(),
            free: true,
            kind: "openai".into(),
            url: GLM.into(),
            scheme: "glm-jwt".into(),
            key: GLM_KEY.into(),
            body: String::new(),
        }]));
        assert_eq!(s, "a [key removed] b [key removed]");
    }
    /// A household store for the walk: holds the answers whose keys it was given.
    struct HeldKeys(std::collections::HashSet<String>);

    impl HouseAnswers for HeldKeys {
        async fn holds(&self, cache_key: &str) -> bool {
            self.0.contains(cache_key)
        }
    }

    #[tokio::test]
    async fn paid_step_finds_the_answer_held_in_the_house_and_buys_nothing() {
        let r = rig();
        let id = r.submit(&job(true, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY), attempt("paid", PAID, false, "bearer", PAID_KEY)]));
        r.advance(3 * HOUR);
        r.sender.reply(GROQ, 400, "model not found");
        let house = HeldKeys(["out:v2:week-ahead:2026-11-18".to_string()].into_iter().collect());
        let report = run_once_while(&r.memory, &r.store, &SECRET, &r.sender, &house, &r.clock, || true).await;
        assert_eq!(r.sender.calls_to(PAID), 0, "the house holds the answer: nothing is bought");
        assert_eq!(r.sender.calls_to(GROQ), 1, "free providers are still asked first");
        assert_eq!(report.failed, 1);
        let s = r.store.get(&id).unwrap();
        assert_eq!(s.state, JobState::Failed);
        assert_eq!(s.reason.as_deref(), Some(ANSWERED_ELSEWHERE));
        let last = s.attempts.last().unwrap();
        assert_eq!((last.provider.as_str(), last.outcome, last.detail.as_deref()), ("paid", AttemptOutcome::Skipped, Some(ANSWERED_ELSEWHERE)));
    }

    #[tokio::test]
    async fn paid_step_still_buys_when_the_house_holds_another_key() {
        let r = rig();
        let id = r.submit(&job(true, vec![attempt("groq", GROQ, true, "bearer", FREE_KEY), attempt("paid", PAID, false, "bearer", PAID_KEY)]));
        r.advance(3 * HOUR);
        r.sender.reply(GROQ, 400, "model not found");
        r.sender.reply(PAID, 200, "{\"paid\":true}");
        let house = HeldKeys(["out:v2:week-ahead:2026-11-17".to_string()].into_iter().collect());
        run_once_while(&r.memory, &r.store, &SECRET, &r.sender, &house, &r.clock, || true).await;
        assert_eq!(r.sender.calls_to(PAID), 1);
        assert_eq!(r.store.get(&id).unwrap().state, JobState::Done);
    }

    #[tokio::test]
    async fn the_companions_ai_store_is_looked_up_under_the_jobs_cache_key() {
        let state = test_state(test_config());
        let key = "out:v2:week-ahead:2026-11-18";
        assert!(!state.stores.ai.holds(key).await, "an empty store holds nothing");
        // As a television offers it (CompanionAiLookup.offer → /v1/ai/put): the outbound key, a text value.
        crate::ai_store::put(&state.stores.ai, key, &Value::String("The week ahead.".into()), T0).await.expect("stored");
        assert!(state.stores.ai.holds(key).await);
        assert!(!state.stores.ai.holds("out:v2:week-ahead:2026-11-19").await);
        crate::ai_store::put(&state.stores.ai, "out:v2:blank", &Value::String("  ".into()), T0).await.expect("stored");
        assert!(!state.stores.ai.holds("out:v2:blank").await, "a blank answer is no answer");
    }
}
