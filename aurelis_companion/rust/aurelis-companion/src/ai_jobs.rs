//! The AI job store: provider calls a television hands to the Companion to make for it.
//!
//! A job is the television's description of what it would send (addresses, keys, bodies), so the
//! whole job is kept sealed under the household secret and only opened in memory, for the runner
//! (Task 3). The `.json` beside it holds only what is safe to print: the header, the state, what
//! each attempt did (scrubbed) and, once answered, the provider's raw reply. No key, auth field
//! or address is ever written there, logged or returned. A job that can never run again (answered,
//! failed, expired) has its sealed file deleted at once; its record is kept 48 hours for the
//! television to collect.

use crate::auth::Session;
use crate::bundle_store::{write_atomic, MAX_CLOCK_LEAD_MS};
use crate::guide_store::one_line;
use crate::state::{AppState, WallClock};
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use companion_protocol::seal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

pub const DIR_NAME: &str = "ai_jobs";
pub const PATH: &str = "/v1/ai/jobs";
/// The largest sealed job kept. A real one is a few kilobytes of prompt; this is a guard.
pub const MAX_SEALED_BYTES: usize = 256 * 1024;
/// Jobs waiting or running at once.
pub const MAX_OPEN_JOBS: usize = 200;
/// Every record held, finished ones included, so a flood of fast jobs cannot fill the disk.
pub const MAX_KEPT_JOBS: usize = 1000;
/// How long a finished job waits for its television to collect it.
pub const RESULT_KEPT_MS: u64 = 48 * 3_600_000;
/// The Companion never shortens the free-slot wait below this (the owner's rule, `FREE_SLOT_WAIT_MS`).
pub const MIN_FREE_SLOT_WAIT_MS: u64 = 3 * 3_600_000;
pub const DEFAULT_LIFE_MS: u64 = 24 * 3_600_000;
const TOMBSTONES_KEPT: usize = 50;
pub const DETAIL_MAX_CHARS: usize = 500;
pub const RUNNER_OFF_REASON: &str = "switched off in the Companion's options (ai_runner)";
const AUTH_SCHEMES: [&str; 3] = ["bearer", "glm-jwt", "goog-api-key"];
const KINDS: [&str; 2] = ["openai", "gemini"];

#[derive(Debug, PartialEq)]
pub struct Refusal {
    pub status: StatusCode,
    pub reason: String,
}

fn refuse<T>(status: StatusCode, reason: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal { status, reason: reason.into() })
}

/// What a job says about itself, with none of what it carries (no key, address or body).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobHeader {
    pub cache_key: String,
    pub label: String,
    pub due_at_ms: u64,
    /// 0 when the television named none; the store then uses due + 24 h.
    pub expires_at_ms: u64,
    pub paid_allowed: bool,
    pub free_slot_wait_ms: u64,
    pub attempt_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Expired,
}

impl JobState {
    fn terminal(self) -> bool {
        matches!(self, JobState::Done | JobState::Failed | JobState::Expired)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttemptOutcome {
    Answered,
    Retry,
    Parked,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptRecord {
    pub provider: String,
    pub free: bool,
    pub at_ms: u64,
    pub outcome: AttemptOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    pub provider: String,
    pub model: String,
    pub kind: String,
    pub http_code: u16,
    pub body: String,
    pub answered_at_ms: u64,
}

pub enum Outcome {
    Done(JobResult),
    Failed(String),
    /// A job's window closed mid-run.
    Expired(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    id: String,
    state: JobState,
    header: JobHeader,
    queued_at_ms: u64,
    finished_at_ms: Option<u64>,
    attempts: Vec<AttemptRecord>,
    result: Option<JobResult>,
    reason: Option<String>,
}

impl Record {
    fn provider(&self) -> Option<String> {
        self.result.as_ref().map(|r| r.provider.clone()).or_else(|| self.attempts.last().map(|a| a.provider.clone()))
    }
}

/// What `GET /v1/ai/jobs/{id}` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub id: String,
    pub state: JobState,
    pub label: String,
    pub cache_key: String,
    pub queued_at_ms: u64,
    pub due_at_ms: u64,
    pub expires_at_ms: u64,
    pub attempts: Vec<AttemptRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JobResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobSummary {
    pub id: String,
    pub state: JobState,
    pub label: String,
    pub cache_key: String,
    pub queued_at_ms: u64,
    /// The provider that answered, or else the last one asked; none before the first attempt.
    pub provider: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct JobCounts {
    pub queued: usize,
    pub running: usize,
    pub done: usize,
    /// Failed and expired: a job that will not produce an answer.
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Submitted {
    pub id: String,
    pub queued_at_ms: u64,
    pub existing: bool,
    /// Whether the job now held for the key may pay: for an existing job, ITS rule, not the
    /// newcomer's (a television holds its own paid providers back only for a job that may pay).
    pub paid_allowed: bool,
}

/// A job ready to run. The keys stay sealed until the runner calls [`RunnableJob::open`].
pub struct RunnableJob {
    pub id: String,
    pub header: JobHeader,
    sealed: Vec<u8>,
}

impl RunnableJob {
    /// The job's plain JSON (the television's attempts, with keys), for use in memory only.
    pub fn open(&self, secret: &[u8; 32]) -> Option<Vec<u8>> {
        seal::open(&self.sealed, secret)
    }
}

struct Inner {
    jobs: HashMap<String, Record>,
    /// Jobs removed unread or on schedule, newest last, so the status page can say what became of them.
    tombstones: VecDeque<JobSummary>,
}

pub struct JobStore {
    dir: PathBuf,
    clock: Mutex<WallClock>,
    inner: Mutex<Inner>,
}

fn sealed_name(id: &str) -> String {
    format!("{id}.sealed")
}

fn json_name(id: &str) -> String {
    format!("{id}.json")
}

fn new_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A provider's words made safe to keep and print: cut at the first thing that could be a
/// credential (a key or token parameter, an Authorization header, a bearer token, a login), one
/// printable line, at most 500 characters.
pub fn scrub_detail(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let markers = ["key=", "token=", "authorization", "bearer ", "username=", "password=", "x-goog-api-key", "api_key", "apikey"];
    let cut = markers.iter().filter_map(|m| lower.find(m)).min();
    let kept = match cut {
        Some(0) => "[secret removed]".to_string(),
        Some(i) => format!("{} [secret removed]", s[..i].trim_end()),
        None => s.to_string(),
    };
    one_line(&kept, DETAIL_MAX_CHARS)
}

/// Checks a job's JSON and returns its header. The refusal names the field that is wrong.
pub fn parse_job(plain: &[u8]) -> Result<JobHeader, Refusal> {
    let bad = |why: String| -> Result<JobHeader, Refusal> { refuse(StatusCode::BAD_REQUEST, why) };
    let v: Value = match serde_json::from_slice(plain) {
        Ok(v) => v,
        Err(e) => return bad(format!("the job is not valid JSON: {e}")),
    };
    let Some(obj) = v.as_object() else { return bad("the job must be a JSON object".into()) };
    let text = |k: &str| obj.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let Some(cache_key) = text("cacheKey") else { return bad("'cacheKey' is required (the answer's identity)".into()) };
    let number = |k: &str| -> Result<u64, Refusal> {
        match obj.get(k) {
            None | Some(Value::Null) => Ok(0),
            Some(n) => n.as_u64().ok_or(Refusal { status: StatusCode::BAD_REQUEST, reason: format!("'{k}' must be a whole number of milliseconds") }),
        }
    };
    let due_at_ms = number("dueAtMs")?;
    let expires_at_ms = number("expiresAtMs")?;
    // The wait before a paid provider may answer is the household's rule: never assumed, so a job
    // that forgot it is refused rather than allowed to pay at once.
    if !matches!(obj.get("freeSlotWaitMs"), Some(Value::Number(_))) {
        return bad("'freeSlotWaitMs' is required (how long a free slot is waited for before a paid provider may answer, in milliseconds)".into());
    }
    let free_slot_wait_ms = number("freeSlotWaitMs")?;
    let paid_allowed = match obj.get("paidAllowed") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return bad("'paidAllowed' must be true or false".into()),
    };
    let attempts = match obj.get("attempts") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        Some(Value::Array(_)) | None | Some(Value::Null) => return bad("'attempts' is required and must hold at least one provider attempt".into()),
        Some(_) => return bad("'attempts' must be a list".into()),
    };
    for (i, a) in attempts.iter().enumerate() {
        let field = |k: &str| a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
        for k in ["provider", "url", "body"] {
            if field(k).is_none() {
                return bad(format!("'attempts[{i}].{k}' is required"));
            }
        }
        match field("kind") {
            Some(k) if KINDS.contains(&k) => {}
            Some(k) => return bad(format!("'attempts[{i}].kind' '{}' is not one of {}", one_line(k, 30), KINDS.join(", "))),
            None => return bad(format!("'attempts[{i}].kind' is required ({})", KINDS.join(" or "))),
        }
        let auth = a.get("auth");
        match auth.and_then(|x| x.get("scheme")).and_then(Value::as_str) {
            Some(s) if AUTH_SCHEMES.contains(&s) => {}
            Some(s) => return bad(format!("'attempts[{i}].auth.scheme' '{}' is not one of {}", one_line(s, 30), AUTH_SCHEMES.join(", "))),
            None => return bad(format!("'attempts[{i}].auth.scheme' is required ({})", AUTH_SCHEMES.join(", "))),
        }
        if auth.and_then(|x| x.get("key")).and_then(Value::as_str).filter(|s| !s.is_empty()).is_none() {
            return bad(format!("'attempts[{i}].auth.key' is required"));
        }
    }
    Ok(JobHeader {
        cache_key: cache_key.to_string(),
        label: one_line(text("label").unwrap_or(""), 80),
        due_at_ms,
        expires_at_ms,
        paid_allowed,
        free_slot_wait_ms,
        attempt_count: attempts.len(),
    })
}

fn remove_file(dir: &Path, name: &str) {
    match std::fs::remove_file(dir.join(name)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("ai jobs: cannot delete {}: {e}", dir.join(name).display()),
    }
}

fn write_record(dir: &Path, rec: &Record) -> Result<(), String> {
    let body = serde_json::to_vec(rec).map_err(|e| format!("cannot encode the job record: {e}"))?;
    write_atomic(dir, &json_name(&rec.id), &body)
}

impl JobStore {
    /// Reads what is held. A job that was running when the process died is queued again: it is
    /// retried, never lost and never counted twice. A record whose sealed file is gone cannot run
    /// and is failed with that reason; leftovers with no record are deleted.
    pub fn open(data_dir: &Path, clock: WallClock) -> JobStore {
        let dir = data_dir.join(DIR_NAME);
        let mut jobs = HashMap::new();
        let mut names: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            names = rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
        }
        for name in &names {
            if name.ends_with(".tmp") {
                remove_file(&dir, name);
                continue;
            }
            let Some(id) = name.strip_suffix(".json") else { continue };
            let read = std::fs::read(dir.join(name)).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Record>(&b).map_err(|e| e.to_string()));
            let rec = match read {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("ai jobs: {name} is unreadable ({e}); dropping it");
                    remove_file(&dir, name);
                    remove_file(&dir, &sealed_name(id));
                    continue;
                }
            };
            jobs.insert(rec.id.clone(), rec);
        }
        let now = clock().max(0) as u64;
        for rec in jobs.values_mut() {
            let sealed_present = dir.join(sealed_name(&rec.id)).is_file();
            let mut changed = false;
            if rec.state == JobState::Running {
                rec.state = JobState::Queued;
                changed = true;
            }
            if rec.state == JobState::Queued && !sealed_present {
                rec.state = JobState::Failed;
                rec.reason = Some("the sealed job file is missing, so it cannot run".into());
                rec.finished_at_ms = Some(now);
                changed = true;
            }
            if rec.state.terminal() && sealed_present {
                remove_file(&dir, &sealed_name(&rec.id));
            }
            if changed {
                if let Err(e) = write_record(&dir, rec) {
                    eprintln!("ai jobs: {e}");
                }
            }
        }
        for name in &names {
            if let Some(id) = name.strip_suffix(".sealed") {
                if !jobs.contains_key(id) {
                    remove_file(&dir, name);
                }
            }
        }
        JobStore { dir, clock: Mutex::new(clock), inner: Mutex::new(Inner { jobs, tombstones: VecDeque::new() }) }
    }

    pub fn set_clock(&self, clock: WallClock) {
        *self.clock.lock().unwrap_or_else(|e| e.into_inner()) = clock;
    }

    fn now(&self) -> u64 {
        let clock = self.clock.lock().unwrap_or_else(|e| e.into_inner()).clone();
        clock().max(0) as u64
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Applies time: a queued job past its expiry is expired (its keys are deleted), and a
    /// finished one nobody collected is deleted 48 hours after it finished.
    fn sweep(&self, inner: &mut Inner, now: u64) {
        let ids: Vec<String> = inner.jobs.keys().cloned().collect();
        for id in ids {
            let Some(rec) = inner.jobs.get_mut(&id) else { continue };
            if rec.state == JobState::Queued && rec.header.expires_at_ms > 0 && now >= rec.header.expires_at_ms {
                rec.state = JobState::Expired;
                rec.reason = Some("expired before it could run".into());
                rec.finished_at_ms = Some(now);
                remove_file(&self.dir, &sealed_name(&id));
                if let Err(e) = write_record(&self.dir, rec) {
                    eprintln!("ai jobs: {e}");
                }
                continue;
            }
            if rec.state.terminal() && now >= rec.finished_at_ms.unwrap_or(rec.queued_at_ms).saturating_add(RESULT_KEPT_MS) {
                let reason = if rec.state == JobState::Done {
                    "not collected within 48 hours".to_string()
                } else {
                    format!("{} (cleared after 48 hours)", rec.reason.clone().unwrap_or_else(|| "finished without an answer".into()))
                };
                let summary = JobSummary {
                    id: id.clone(),
                    state: JobState::Expired,
                    label: rec.header.label.clone(),
                    cache_key: rec.header.cache_key.clone(),
                    queued_at_ms: rec.queued_at_ms,
                    provider: rec.provider(),
                    reason: Some(scrub_detail(&reason)),
                };
                remove_file(&self.dir, &sealed_name(&id));
                remove_file(&self.dir, &json_name(&id));
                inner.jobs.remove(&id);
                inner.tombstones.push_back(summary);
                while inner.tombstones.len() > TOMBSTONES_KEPT {
                    inner.tombstones.pop_front();
                }
            }
        }
    }

    pub fn submit(&self, sealed: Vec<u8>, job: &JobHeader) -> Result<Submitted, Refusal> {
        self.submit_if(sealed, job, || true)
    }

    /// As [`submit`](Self::submit), but `still_paired` is asked under the store lock, so a submit
    /// cannot write after an unpair has taken the household (reset takes the same lock).
    pub fn submit_if(&self, sealed: Vec<u8>, job: &JobHeader, still_paired: impl Fn() -> bool) -> Result<Submitted, Refusal> {
        if sealed.len() > MAX_SEALED_BYTES {
            return refuse(StatusCode::PAYLOAD_TOO_LARGE, format!("the job is {} bytes sealed and the largest kept is {MAX_SEALED_BYTES} (256 KiB)", sealed.len()));
        }
        let now = self.now();
        let mut header = job.clone();
        if header.due_at_ms == 0 {
            header.due_at_ms = now;
        }
        header.free_slot_wait_ms = header.free_slot_wait_ms.max(MIN_FREE_SLOT_WAIT_MS);
        if header.expires_at_ms == 0 {
            header.expires_at_ms = header.due_at_ms + DEFAULT_LIFE_MS;
        } else if header.expires_at_ms <= now {
            return refuse(StatusCode::BAD_REQUEST, "'expiresAtMs' is not in the future, so the job could never run — check this television's clock");
        } else if header.expires_at_ms < header.due_at_ms {
            return refuse(StatusCode::BAD_REQUEST, "'expiresAtMs' is before 'dueAtMs', so the job could never run");
        }
        let ahead = header.due_at_ms.saturating_sub(now);
        if ahead > MAX_CLOCK_LEAD_MS {
            return refuse(StatusCode::BAD_REQUEST, format!("'dueAtMs' is {} min in the future — check this television's clock", ahead / 60_000));
        }
        let mut inner = self.lock();
        if !still_paired() {
            return refuse(StatusCode::CONFLICT, "this Companion is no longer paired; the job was not kept");
        }
        self.sweep(&mut inner, now);
        // A job already waiting, running or answered for this key is the answer: one call serves every television.
        if let Some(existing) = inner.jobs.values().find(|r| r.header.cache_key == job.cache_key && matches!(r.state, JobState::Queued | JobState::Running | JobState::Done)) {
            return Ok(Submitted { id: existing.id.clone(), queued_at_ms: existing.queued_at_ms, existing: true, paid_allowed: existing.header.paid_allowed });
        }
        let open = inner.jobs.values().filter(|r| matches!(r.state, JobState::Queued | JobState::Running)).count();
        if open >= MAX_OPEN_JOBS {
            return refuse(StatusCode::SERVICE_UNAVAILABLE, format!("the AI queue is full ({MAX_OPEN_JOBS} jobs waiting or running); try again once some have run"));
        }
        // Refuse before deleting anything: a 503 must leave what is held as it was.
        if inner.jobs.len() >= MAX_KEPT_JOBS {
            return refuse(StatusCode::SERVICE_UNAVAILABLE, format!("the AI queue holds {MAX_KEPT_JOBS} jobs including finished ones nobody has collected; try again later"));
        }
        // A failed or expired job for the same key makes way for the new ask.
        let stale: Vec<String> = inner.jobs.values().filter(|r| r.header.cache_key == job.cache_key).map(|r| r.id.clone()).collect();
        for id in stale {
            inner.jobs.remove(&id);
            remove_file(&self.dir, &sealed_name(&id));
            remove_file(&self.dir, &json_name(&id));
        }
        let id = new_id();
        let paid_allowed = header.paid_allowed;
        let rec = Record { id: id.clone(), state: JobState::Queued, header, queued_at_ms: now, finished_at_ms: None, attempts: Vec::new(), result: None, reason: None };
        let written = std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))
            .and_then(|_| write_atomic(&self.dir, &sealed_name(&id), &sealed))
            .and_then(|_| write_record(&self.dir, &rec));
        if let Err(why) = written {
            eprintln!("ai jobs: {why}");
            remove_file(&self.dir, &sealed_name(&id));
            remove_file(&self.dir, &json_name(&id));
            return refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("the job could not be kept: {why}"));
        }
        inner.jobs.insert(id.clone(), rec);
        Ok(Submitted { id, queued_at_ms: now, existing: false, paid_allowed })
    }

    pub fn get(&self, id: &str) -> Option<JobStatus> {
        let now = self.now();
        let mut inner = self.lock();
        self.sweep(&mut inner, now);
        inner.jobs.get(id).map(|r| JobStatus {
            id: r.id.clone(),
            state: r.state,
            label: r.header.label.clone(),
            cache_key: r.header.cache_key.clone(),
            queued_at_ms: r.queued_at_ms,
            due_at_ms: r.header.due_at_ms,
            expires_at_ms: r.header.expires_at_ms,
            attempts: r.attempts.clone(),
            result: r.result.clone(),
            reason: r.reason.clone(),
        })
    }

    pub fn header(&self, id: &str) -> Option<JobHeader> {
        self.lock().jobs.get(id).map(|r| r.header.clone())
    }

    pub fn delete(&self, id: &str) -> bool {
        let mut inner = self.lock();
        let existed = inner.jobs.remove(id).is_some();
        if existed {
            remove_file(&self.dir, &sealed_name(id));
            remove_file(&self.dir, &json_name(id));
        }
        existed
    }

    /// The earliest-due queued job whose time has come, marked running and handed over with its
    /// sealed file read (not opened).
    #[cfg(test)]
    pub fn next_runnable(&self, now_ms: u64) -> Option<RunnableJob> {
        self.next_runnable_where(now_ms, |_| true)
    }

    /// As [`next_runnable`](Self::next_runnable), among the queued jobs `wanted` accepts by id (the
    /// runner passes over a job waiting out its back-off, or one it already took this pass).
    pub fn next_runnable_where(&self, now_ms: u64, wanted: impl Fn(&str) -> bool) -> Option<RunnableJob> {
        let mut inner = self.lock();
        self.sweep(&mut inner, now_ms);
        let id = inner
            .jobs
            .values()
            .filter(|r| r.state == JobState::Queued && r.header.due_at_ms <= now_ms && wanted(&r.id))
            .min_by_key(|r| (r.header.due_at_ms, r.queued_at_ms, r.id.clone()))
            .map(|r| r.id.clone())?;
        let sealed = match std::fs::read(self.dir.join(sealed_name(&id))) {
            Ok(b) => b,
            Err(e) => {
                if let Some(rec) = inner.jobs.get_mut(&id) {
                    rec.state = JobState::Failed;
                    rec.reason = Some(format!("the sealed job file could not be read: {e}"));
                    rec.finished_at_ms = Some(now_ms);
                    if let Err(e) = write_record(&self.dir, rec) {
                        eprintln!("ai jobs: {e}");
                    }
                }
                eprintln!("ai jobs: job {id}: the sealed file could not be read: {e}");
                return None;
            }
        };
        let rec = inner.jobs.get_mut(&id)?;
        rec.state = JobState::Running;
        if let Err(e) = write_record(&self.dir, rec) {
            eprintln!("ai jobs: {e}");
        }
        Some(RunnableJob { id, header: rec.header.clone(), sealed })
    }

    /// Notes what one attempt did. Words are scrubbed here, whatever the caller sent.
    pub fn record_attempt(&self, id: &str, mut attempt: AttemptRecord) -> bool {
        attempt.detail = attempt.detail.map(|d| scrub_detail(&d));
        let mut inner = self.lock();
        let Some(rec) = inner.jobs.get_mut(id) else { return false };
        rec.attempts.push(attempt);
        if let Err(e) = write_record(&self.dir, rec) {
            eprintln!("ai jobs: {e}");
        }
        true
    }

    /// Puts a running job back in the queue (it is waiting for a busy provider, a park to end or
    /// the paid wait); the runner keeps its next-try time. Its sealed file stays.
    /// `waiting_for` becomes the job's reason while it waits (scrubbed), so its status and the
    /// status page say what it is waiting on.
    pub fn requeue(&self, id: &str, waiting_for: &str) -> bool {
        let mut inner = self.lock();
        let Some(rec) = inner.jobs.get_mut(id) else { return false };
        if rec.state != JobState::Running {
            return false;
        }
        rec.state = JobState::Queued;
        rec.reason = Some(scrub_detail(waiting_for));
        if let Err(e) = write_record(&self.dir, rec) {
            eprintln!("ai jobs: {e}");
        }
        true
    }

    /// Ends a job. Its sealed file (the keys) goes at once; the record waits for collection.
    pub fn finish(&self, id: &str, outcome: Outcome) -> bool {
        let now = self.now();
        let mut inner = self.lock();
        let Some(rec) = inner.jobs.get_mut(id) else { return false };
        match outcome {
            Outcome::Done(result) => {
                rec.finished_at_ms = Some(result.answered_at_ms);
                rec.state = JobState::Done;
                rec.result = Some(result);
                rec.reason = None;
            }
            Outcome::Failed(why) => {
                rec.state = JobState::Failed;
                rec.reason = Some(scrub_detail(&why));
                rec.finished_at_ms = Some(now);
            }
            Outcome::Expired(why) => {
                rec.state = JobState::Expired;
                rec.reason = Some(scrub_detail(&why));
                rec.finished_at_ms = Some(now);
            }
        }
        remove_file(&self.dir, &sealed_name(id));
        if let Err(e) = write_record(&self.dir, rec) {
            eprintln!("ai jobs: {e}");
        }
        true
    }

    pub fn counts(&self) -> JobCounts {
        let now = self.now();
        let mut inner = self.lock();
        self.sweep(&mut inner, now);
        let mut c = JobCounts::default();
        for r in inner.jobs.values() {
            match r.state {
                JobState::Queued => c.queued += 1,
                JobState::Running => c.running += 1,
                JobState::Done => c.done += 1,
                JobState::Failed | JobState::Expired => c.failed += 1,
            }
        }
        c
    }

    /// The newest `n` jobs, live ones and those removed unread, newest first.
    pub fn recent(&self, n: usize) -> Vec<JobSummary> {
        let now = self.now();
        let mut inner = self.lock();
        self.sweep(&mut inner, now);
        let mut all: Vec<JobSummary> = inner
            .jobs
            .values()
            .map(|r| JobSummary {
                id: r.id.clone(),
                state: r.state,
                label: r.header.label.clone(),
                cache_key: r.header.cache_key.clone(),
                queued_at_ms: r.queued_at_ms,
                provider: r.provider(),
                reason: r.reason.clone(),
            })
            .chain(inner.tombstones.iter().cloned())
            .collect();
        all.sort_by(|a, b| b.queued_at_ms.cmp(&a.queued_at_ms).then_with(|| a.id.cmp(&b.id)));
        all.truncate(n);
        all
    }

    /// Unpair: forget everything and delete the folder.
    pub fn reset(&self) {
        let mut inner = self.lock();
        inner.jobs.clear();
        inner.tombstones.clear();
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("unpair: cannot delete {}: {e}", self.dir.display()),
        }
    }
}

fn refusal_response(session: &Session, r: Refusal) -> Response {
    session.sealed_reason(r.status, &r.reason)
}

/// `POST /v1/ai/jobs`. The middleware has peeled the transport seal; the job is sealed again here
/// under the household secret before it touches the disk.
pub async fn submit_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>, body: Bytes) -> Response {
    // The card's own word: off in the options, or the stores below the floor.
    let card = state.card();
    if !card.capabilities.ai_runner {
        let why = card.declined.get("aiRunner").cloned().unwrap_or_else(|| RUNNER_OFF_REASON.to_string());
        return session.sealed_reason(StatusCode::SERVICE_UNAVAILABLE, &why);
    }
    let header = match parse_job(&body) {
        Ok(h) => h,
        Err(r) => {
            eprintln!("ai job refused: {}", r.reason);
            return refusal_response(&session, r);
        }
    };
    let sealed = seal::seal(&body, &session.secret);
    if sealed.len() <= MAX_SEALED_BYTES {
        if let Err(r) = state.stores.budget.check_floor(sealed.len() as u64).await {
            return session.sealed_reason(StatusCode::INSUFFICIENT_STORAGE, &r.reason);
        }
    }
    let paired_state = state.clone();
    match state.ai_jobs.submit_if(sealed, &header, move || paired_state.auth.snapshot().paired) {
        Ok(s) => {
            if !s.existing {
                state.push_job(&format!("AI job queued: {}", if header.label.is_empty() { "(no label)" } else { &header.label }));
            }
            let status = if s.existing { StatusCode::OK } else { StatusCode::ACCEPTED };
            let mut v = serde_json::json!({ "id": s.id, "queuedAtMs": s.queued_at_ms, "paidAllowed": s.paid_allowed });
            if s.existing {
                v["existing"] = Value::Bool(true);
            }
            session.sealed_json(status, &v)
        }
        Err(r) => {
            eprintln!("ai job refused: {}", r.reason);
            refusal_response(&session, r)
        }
    }
}

const NO_SUCH_JOB: &str = "no such job (it may have been collected, or not collected within 48 hours)";

pub async fn get_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>, UrlPath(id): UrlPath<String>) -> Response {
    match state.ai_jobs.get(&id) {
        Some(status) => match serde_json::to_value(&status) {
            Ok(v) => session.sealed_json(StatusCode::OK, &v),
            Err(e) => session.sealed_reason(StatusCode::INTERNAL_SERVER_ERROR, &format!("the job status could not be written: {e}")),
        },
        None => session.sealed_reason(StatusCode::NOT_FOUND, NO_SUCH_JOB),
    }
}

pub async fn delete_route(State(state): State<Arc<AppState>>, Extension(session): Extension<Session>, UrlPath(id): UrlPath<String>) -> Response {
    if state.ai_jobs.delete(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        session.sealed_reason(StatusCode::NOT_FOUND, NO_SUCH_JOB)
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
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicI64, Ordering};
    use tower::ServiceExt;

    const SECRET: [u8; 32] = [9u8; 32];
    const T0: i64 = 1_800_000_000_000;
    const KEY: &str = "sk-must-never-leave-the-sealed-file";
    const URL: &str = "https://api.groq.example/openai/v1/chat/completions";

    struct Rig {
        dir: TempDir,
        state: Arc<AppState>,
        now: Arc<AtomicI64>,
    }

    fn rig_with(ai_runner: bool) -> Rig {
        let dir = TempDir::new();
        Household::create(&dir.0, SECRET).expect("household");
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        config.ai_runner = ai_runner;
        let now = Arc::new(AtomicI64::new(T0));
        let wall = {
            let n = now.clone();
            Arc::new(move || n.load(Ordering::SeqCst)) as crate::state::WallClock
        };
        Rig { state: Arc::new(test_state(config).with_wall_clock(wall)), dir, now }
    }

    fn rig() -> Rig {
        rig_with(true)
    }

    fn attempt(scheme: &str) -> Value {
        json!({"provider":"groq","model":"llama","free":true,"kind":"openai","url":URL,
               "auth":{"scheme":scheme,"key":KEY},"body":"{\"messages\":[]}"})
    }

    fn job(cache_key: &str) -> Value {
        json!({"cacheKey":cache_key,"label":"week-ahead","dueAtMs":T0,"expiresAtMs":T0 + 86_400_000,
               "paidAllowed":true,"freeSlotWaitMs":10_800_000,"attempts":[attempt("bearer")]})
    }

    impl Rig {
        fn advance(&self, ms: i64) {
            self.now.fetch_add(ms, Ordering::SeqCst);
        }

        async fn send(&self, req: Request<Body>) -> (StatusCode, Vec<u8>) {
            let res = crate::router(self.state.clone()).oneshot(req).await.expect("infallible");
            (res.status(), to_bytes(res.into_body(), 8 << 20).await.expect("body").to_vec())
        }

        async fn authed(&self, method: &str, path: &str, plain: &[u8]) -> (StatusCode, Vec<u8>) {
            let req = Request::builder().uri("/v1/challenge").header("x-aurelis-companion", "1").body(Body::empty()).unwrap();
            let (s, b) = self.send(req).await;
            assert_eq!(s, StatusCode::OK);
            let nonce = serde_json::from_slice::<Value>(&b).unwrap()["nonce"].as_str().unwrap().to_string();
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

        async fn call(&self, method: &str, path: &str, plain: &[u8]) -> (StatusCode, Value) {
            let (s, b) = self.authed(method, path, plain).await;
            let opened = seal::open(&b, &SECRET).unwrap_or(b);
            (s, serde_json::from_slice(&opened).unwrap_or(Value::Null))
        }

        async fn submit(&self, v: &Value) -> (StatusCode, Value) {
            self.call("POST", PATH, v.to_string().as_bytes()).await
        }
    }

    fn header_of(v: &Value) -> JobHeader {
        parse_job(v.to_string().as_bytes()).expect("valid job")
    }

    #[tokio::test]
    async fn submit_get_delete_round_trip() {
        let r = rig();
        let (s, v) = r.submit(&job("out:v2:week-ahead:2026-11-18")).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(id.len(), 32);
        assert_eq!(v["queuedAtMs"], T0);
        let (s, got) = r.call("GET", &format!("{PATH}/{id}"), b"").await;
        assert_eq!(s, StatusCode::OK, "{got}");
        assert_eq!((got["state"].as_str(), got["label"].as_str()), (Some("queued"), Some("week-ahead")));
        assert_eq!(got["cacheKey"], "out:v2:week-ahead:2026-11-18");
        assert_eq!(got["attempts"], json!([]));
        let text = got.to_string();
        assert!(!text.contains(KEY) && !text.contains("groq.example") && !text.contains("auth"), "{text}");
        // At rest: the job file is sealed and the side file holds neither key nor address.
        let sealed = std::fs::read(r.dir.0.join(format!("ai_jobs/{id}.sealed"))).unwrap();
        assert!(!String::from_utf8_lossy(&sealed).contains(KEY));
        assert!(seal::open(&sealed, &SECRET).is_some(), "sealed under the household secret");
        let side = std::fs::read_to_string(r.dir.0.join(format!("ai_jobs/{id}.json"))).unwrap();
        assert!(!side.contains(KEY) && !side.contains("groq.example"), "{side}");
        // The header the store keeps echoes what was sent.
        let h = r.state.ai_jobs.header(&id).unwrap();
        assert_eq!(
            (h.cache_key.as_str(), h.label.as_str(), h.due_at_ms, h.expires_at_ms, h.paid_allowed, h.free_slot_wait_ms, h.attempt_count),
            ("out:v2:week-ahead:2026-11-18", "week-ahead", T0 as u64, T0 as u64 + 86_400_000, true, 10_800_000, 1)
        );
        let (s, _) = r.authed("DELETE", &format!("{PATH}/{id}"), b"").await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert!(!r.dir.0.join(format!("ai_jobs/{id}.sealed")).exists() && !r.dir.0.join(format!("ai_jobs/{id}.json")).exists());
        let (s, v) = r.call("GET", &format!("{PATH}/{id}"), b"").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(v["reason"].as_str().unwrap().contains("no such job"), "{v}");
        assert_eq!(r.authed("DELETE", &format!("{PATH}/{id}"), b"").await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn job_without_cache_key_or_attempts_is_400() {
        let r = rig();
        let mut no_key = job("k");
        no_key.as_object_mut().unwrap().remove("cacheKey");
        let (s, v) = r.submit(&no_key).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["reason"].as_str().unwrap().contains("cacheKey"), "{v}");
        let mut blank_key = job("k");
        blank_key["cacheKey"] = json!("  ");
        assert!(r.submit(&blank_key).await.1["reason"].as_str().unwrap().contains("cacheKey"));
        let mut none = job("k");
        none.as_object_mut().unwrap().remove("attempts");
        let (s, v) = r.submit(&none).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["reason"].as_str().unwrap().contains("attempts"), "{v}");
        let mut empty = job("k");
        empty["attempts"] = json!([]);
        assert!(r.submit(&empty).await.1["reason"].as_str().unwrap().contains("attempts"));
        let mut no_url = job("k");
        no_url["attempts"][0].as_object_mut().unwrap().remove("url");
        assert!(r.submit(&no_url).await.1["reason"].as_str().unwrap().contains("attempts[0].url"));
        let (s, v) = r.call("POST", PATH, b"not json").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["reason"].as_str().unwrap().contains("JSON"), "{v}");
        assert_eq!(r.state.ai_jobs.counts().queued, 0);
    }

    #[tokio::test]
    async fn unknown_auth_scheme_is_400() {
        let r = rig();
        let mut j = job("k");
        j["attempts"][0]["auth"]["scheme"] = json!("basic");
        let (s, v) = r.submit(&j).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let reason = v["reason"].as_str().unwrap();
        assert!(reason.contains("basic") && reason.contains("bearer"), "{reason}");
        assert!(!reason.contains(KEY));
        for ok in ["bearer", "glm-jwt", "goog-api-key"] {
            let mut j = job(&format!("k-{ok}"));
            j["attempts"][0]["auth"]["scheme"] = json!(ok);
            assert_eq!(r.submit(&j).await.0, StatusCode::ACCEPTED, "{ok}");
        }
        let mut kind = job("k-kind");
        kind["attempts"][0]["kind"] = json!("anthropic");
        assert_eq!(r.submit(&kind).await.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn oversize_job_is_413() {
        let r = rig();
        let mut big = job("big");
        big["attempts"][0]["body"] = json!("x".repeat(MAX_SEALED_BYTES + 1));
        let (s, v) = r.submit(&big).await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
        assert!(v["reason"].as_str().unwrap().contains("256 KiB"), "{v}");
        assert_eq!(r.state.ai_jobs.counts().queued, 0);
        let mut ok = job("ok");
        ok["attempts"][0]["body"] = json!("x".repeat(200 * 1024));
        assert_eq!(r.submit(&ok).await.0, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn queue_full_is_503_with_reason() {
        let r = rig();
        let sealed = seal::seal(b"{}", &SECRET);
        for i in 0..MAX_OPEN_JOBS {
            let h = header_of(&job(&format!("k{i}")));
            r.state.ai_jobs.submit(sealed.clone(), &h).unwrap_or_else(|e| panic!("{i}: {}", e.reason));
        }
        assert_eq!(r.state.ai_jobs.counts().queued, 200);
        let (s, v) = r.submit(&job("one-too-many")).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert!(v["reason"].as_str().unwrap().contains("200"), "{v}");
        // A repeat of a job already waiting is still answered, not refused.
        let (s, v) = r.submit(&job("k0")).await;
        assert_eq!((s, &v["existing"]), (StatusCode::OK, &json!(true)));
    }

    #[tokio::test]
    async fn duplicate_cache_key_returns_existing_job() {
        let r = rig();
        let (s, first) = r.submit(&job("same")).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let (s, again) = r.submit(&job("same")).await;
        assert_eq!(s, StatusCode::OK, "{again}");
        assert_eq!((&again["id"], &again["existing"]), (&first["id"], &json!(true)));
        assert_eq!(again["queuedAtMs"], first["queuedAtMs"]);
        assert_eq!(r.state.ai_jobs.counts().queued, 1);
        // A job that has failed does not block a fresh ask for the same answer.
        let id = first["id"].as_str().unwrap();
        r.state.ai_jobs.finish(id, Outcome::Failed("provider said no".into()));
        let (s, fresh) = r.submit(&job("same")).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{fresh}");
        assert_ne!(fresh["id"], first["id"]);
    }

    #[tokio::test]
    async fn existing_job_reply_says_whether_that_job_may_pay() {
        let r = rig();
        let mut free_only = job("free-only");
        free_only["paidAllowed"] = json!(false);
        let (s, first) = r.submit(&free_only).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        assert_eq!(first["paidAllowed"], json!(false), "{first}");
        // A second television asks the same question allowing paid: it is told the job it joins never pays.
        let (s, again) = r.submit(&job("free-only")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!((&again["existing"], &again["paidAllowed"]), (&json!(true), &json!(false)), "{again}");
        let (_, paid) = r.submit(&job("may-pay")).await;
        assert_eq!(paid["paidAllowed"], json!(true));
        let (_, paid_again) = r.submit(&job("may-pay")).await;
        assert_eq!((&paid_again["existing"], &paid_again["paidAllowed"]), (&json!(true), &json!(true)));
    }

    #[tokio::test]
    async fn future_due_is_refused() {
        let r = rig();
        let mut j = job("future");
        j["dueAtMs"] = json!(T0 + 60 * 60_000);
        let (s, v) = r.submit(&j).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let reason = v["reason"].as_str().unwrap();
        assert!(reason.contains("dueAtMs") && reason.contains("clock"), "{reason}");
        j["dueAtMs"] = json!(T0 + 5 * 60_000);
        assert_eq!(r.submit(&j).await.0, StatusCode::ACCEPTED, "a few minutes of drift is ordinary");
    }

    #[tokio::test]
    async fn jobs_survive_restart_and_running_is_requeued() {
        let r = rig();
        let (_, a) = r.submit(&job("a")).await;
        let (_, b) = r.submit(&job("b")).await;
        let running = r.state.ai_jobs.next_runnable(T0 as u64).expect("one is due");
        assert_eq!(r.state.ai_jobs.counts().running, 1);
        let opened = running.open(&SECRET).expect("opens in memory");
        assert!(String::from_utf8_lossy(&opened).contains(KEY));
        // The process dies with that job running; the next start finds both on disk.
        let reopened = JobStore::open(&r.dir.0, Arc::new(|| T0));
        let counts = reopened.counts();
        assert_eq!((counts.queued, counts.running), (2, 0), "running is requeued, never lost or doubled");
        let again = reopened.next_runnable(T0 as u64).expect("due");
        assert_eq!(again.id, running.id, "the earliest-queued job runs again");
        assert!(a["id"] != b["id"]);
    }

    #[tokio::test]
    async fn uncollected_result_expires_after_48h() {
        let r = rig();
        let (_, v) = r.submit(&job("done-one")).await;
        let id = v["id"].as_str().unwrap().to_string();
        r.state.ai_jobs.next_runnable(T0 as u64).unwrap();
        r.state.ai_jobs.record_attempt(
            &id,
            AttemptRecord { provider: "groq".into(), free: true, at_ms: T0 as u64, outcome: AttemptOutcome::Answered, http_code: Some(200), detail: None },
        );
        r.state.ai_jobs.finish(
            &id,
            Outcome::Done(JobResult { provider: "groq".into(), model: "llama".into(), kind: "openai".into(), http_code: 200, body: "{\"ok\":true}".into(), answered_at_ms: T0 as u64 }),
        );
        let (s, got) = r.call("GET", &format!("{PATH}/{id}"), b"").await;
        assert_eq!((s, got["state"].as_str()), (StatusCode::OK, Some("done")), "{got}");
        assert_eq!(got["result"]["body"], "{\"ok\":true}");
        assert_eq!(got["attempts"][0]["outcome"], "answered");
        // The keys are not needed once it has an answer.
        assert!(!r.dir.0.join(format!("ai_jobs/{id}.sealed")).exists());
        r.advance(48 * 3_600_000 - 1);
        assert_eq!(r.call("GET", &format!("{PATH}/{id}"), b"").await.0, StatusCode::OK, "still there a moment before");
        r.advance(1);
        assert_eq!(r.call("GET", &format!("{PATH}/{id}"), b"").await.0, StatusCode::NOT_FOUND);
        assert!(!r.dir.0.join(format!("ai_jobs/{id}.json")).exists());
        let recent = r.state.ai_jobs.recent(10);
        assert!(recent.iter().any(|j| j.id == id && j.reason.as_deref() == Some("not collected within 48 hours")), "{recent:?}");
        assert_eq!(r.state.ai_jobs.counts().done, 0);
    }

    #[tokio::test]
    async fn a_queued_job_past_its_expiry_is_expired_with_a_reason() {
        let r = rig();
        let mut j = job("stale");
        j["dueAtMs"] = json!(T0 + 5 * 60_000);
        j["expiresAtMs"] = json!(T0 + 30 * 60_000);
        let id = r.submit(&j).await.1["id"].as_str().unwrap().to_string();
        r.advance(60 * 60_000);
        assert!(r.state.ai_jobs.next_runnable(r.state.wall_ms() as u64).is_none());
        let got = r.call("GET", &format!("{PATH}/{id}"), b"").await.1;
        assert_eq!(got["state"], "expired", "{got}");
        assert!(got["reason"].as_str().unwrap().contains("expired"), "{got}");
        assert!(!r.dir.0.join(format!("ai_jobs/{id}.sealed")).exists(), "no keys kept for a job that will never run");
    }

    #[tokio::test]
    async fn detail_and_reason_are_scrubbed_and_short() {
        let r = rig();
        let id = r.submit(&job("scrub")).await.1["id"].as_str().unwrap().to_string();
        r.state.ai_jobs.record_attempt(
            &id,
            AttemptRecord {
                provider: "groq".into(),
                free: true,
                at_ms: 1,
                outcome: AttemptOutcome::Failed,
                http_code: Some(401),
                detail: Some(format!("bad request: Authorization: Bearer {KEY} {}", "y".repeat(900))),
            },
        );
        r.state.ai_jobs.finish(&id, Outcome::Failed(format!("refused at https://x.example/v1?key={KEY}&more=1")));
        let got = r.call("GET", &format!("{PATH}/{id}"), b"").await.1;
        let text = got.to_string();
        assert!(!text.contains(KEY), "{text}");
        assert!(got["attempts"][0]["detail"].as_str().unwrap().chars().count() <= 500);
        assert!(got["reason"].as_str().unwrap().chars().count() <= 500);
        assert_eq!(got["state"], "failed");
    }

    #[tokio::test]
    async fn unpair_wipes_ai_jobs() {
        let r = rig();
        let id = r.submit(&job("x")).await.1["id"].as_str().unwrap().to_string();
        assert!(r.dir.0.join(format!("ai_jobs/{id}.sealed")).is_file());
        crate::state::unpair(&r.state).await.expect("unpaired");
        assert!(!r.dir.0.join("ai_jobs").exists(), "folder gone");
        assert_eq!(r.state.ai_jobs.counts(), JobCounts::default());
        assert!(r.state.ai_jobs.get(&id).is_none());
        assert!(crate::household::STORE_DIRS.contains(&DIR_NAME), "on the household wipe list too");
    }

    #[tokio::test]
    async fn card_reports_ai_runner_and_counts() {
        let r = rig();
        r.submit(&job("one")).await;
        r.submit(&job("two")).await;
        let running = r.state.ai_jobs.next_runnable(T0 as u64).unwrap();
        r.state.ai_jobs.finish(&running.id, Outcome::Failed("nope".into()));
        r.state.ai_jobs.next_runnable(T0 as u64).unwrap();
        let req = Request::builder().uri("/v1/card").body(Body::empty()).unwrap();
        let (s, b) = r.send(req).await;
        assert_eq!(s, StatusCode::OK);
        let card: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(card["capabilities"]["aiRunner"], true, "{card}");
        assert!(card["declined"].get("aiRunner").is_none(), "{card}");
        assert_eq!(card["aiJobs"], json!({"queued":0,"running":1,"done":0,"failed":1}));

        let off = rig_with(false);
        let req = Request::builder().uri("/v1/card").body(Body::empty()).unwrap();
        let card: Value = serde_json::from_slice(&off.send(req).await.1).unwrap();
        assert_eq!(card["capabilities"]["aiRunner"], false);
        assert_eq!(card["declined"]["aiRunner"], "switched off in the Companion's options (ai_runner)");
        let (s, v) = off.submit(&job("x")).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(v["reason"], "switched off in the Companion's options (ai_runner)");
        assert_eq!(off.state.ai_jobs.counts().queued, 0);
    }

    #[tokio::test]
    async fn ai_job_routes_require_auth() {
        let r = rig();
        for (method, path) in [("POST", PATH.to_string()), ("GET", format!("{PATH}/abc")), ("DELETE", format!("{PATH}/abc"))] {
            let req = Request::builder().method(method).uri(&path).header("x-aurelis-companion", "1").body(Body::from("x")).unwrap();
            assert_eq!(r.send(req).await.0, StatusCode::UNAUTHORIZED, "{method}");
        }
        assert_eq!(r.state.ai_jobs.counts().queued, 0);
    }

    #[tokio::test]
    async fn missing_due_is_stamped_now() {
        let r = rig();
        for (i, v) in [None, Some(json!(null)), Some(json!(0))].into_iter().enumerate() {
            let mut j = job(&format!("nodue{i}"));
            match v {
                None => {
                    j.as_object_mut().unwrap().remove("dueAtMs");
                }
                Some(x) => j["dueAtMs"] = x,
            }
            j.as_object_mut().unwrap().remove("expiresAtMs");
            let (s, v) = r.submit(&j).await;
            assert_eq!(s, StatusCode::ACCEPTED, "{v}");
            let h = r.state.ai_jobs.header(v["id"].as_str().unwrap()).unwrap();
            assert_eq!(h.due_at_ms, T0 as u64, "stamped now, not 0");
            assert_eq!(h.expires_at_ms, T0 as u64 + 86_400_000);
        }
    }

    #[tokio::test]
    async fn missing_free_slot_wait_is_400() {
        let r = rig();
        let mut j = job("nowait");
        j.as_object_mut().unwrap().remove("freeSlotWaitMs");
        let (s, v) = r.submit(&j).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["reason"].as_str().unwrap().contains("freeSlotWaitMs"), "{v}");
        j["freeSlotWaitMs"] = json!(null);
        assert_eq!(r.submit(&j).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(r.state.ai_jobs.counts().queued, 0);
    }

    #[tokio::test]
    async fn short_free_slot_wait_is_raised_to_three_hours() {
        let r = rig();
        for (i, w) in [0u64, 1, 10_799_999].into_iter().enumerate() {
            let mut j = job(&format!("short{i}"));
            j["freeSlotWaitMs"] = json!(w);
            let (s, v) = r.submit(&j).await;
            assert_eq!(s, StatusCode::ACCEPTED, "{v}");
            assert_eq!(r.state.ai_jobs.header(v["id"].as_str().unwrap()).unwrap().free_slot_wait_ms, 10_800_000);
        }
        let mut long = job("long");
        long["freeSlotWaitMs"] = json!(20_000_000u64);
        let v = r.submit(&long).await.1;
        assert_eq!(r.state.ai_jobs.header(v["id"].as_str().unwrap()).unwrap().free_slot_wait_ms, 20_000_000);
    }

    #[tokio::test]
    async fn route_refuses_with_the_cards_reason_when_the_stores_are_declined() {
        let r = rig();
        let mut f = r.state.facts();
        f.ram_mb = 100;
        r.state.update_facts(f);
        let card = r.state.card();
        assert!(!card.capabilities.ai_runner);
        let want = card.declined["aiRunner"].clone();
        let (s, v) = r.submit(&job("below")).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
        assert_eq!(v["reason"], want.as_str());
        assert_eq!(r.state.ai_jobs.counts().queued, 0);
    }

    #[tokio::test]
    async fn submit_after_unpair_is_refused_and_leaves_no_file() {
        let r = rig();
        let h = header_of(&job("late"));
        let sealed = seal::seal(b"{}", &SECRET);
        let why = r.state.ai_jobs.submit_if(sealed.clone(), &h, || false).unwrap_err();
        assert_eq!(why.status, StatusCode::CONFLICT);
        assert!(!r.dir.0.join("ai_jobs").exists());
        // Through the real unpair: reset has run, a later store submit is refused by the pairing check.
        crate::state::unpair(&r.state).await.expect("unpaired");
        let state = r.state.clone();
        let why = r.state.ai_jobs.submit_if(sealed, &h, move || state.auth.snapshot().paired).unwrap_err();
        assert_eq!(why.status, StatusCode::CONFLICT);
        assert!(!r.dir.0.join("ai_jobs").exists(), "nothing written after the wipe");
    }

    #[tokio::test]
    async fn expiry_before_now_or_before_due_is_400() {
        let r = rig();
        for (i, (due, exp)) in [(T0, T0 - 1), (T0, T0), (T0 + 120_000, T0 + 60_000)].into_iter().enumerate() {
            let mut j = job(&format!("exp{i}"));
            j["dueAtMs"] = json!(due);
            j["expiresAtMs"] = json!(exp);
            let (s, v) = r.submit(&j).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{i}: {v}");
            assert!(v["reason"].as_str().unwrap().contains("expiresAtMs"), "{v}");
        }
        assert_eq!(r.state.ai_jobs.counts().queued, 0);
    }

    #[tokio::test]
    async fn a_503_for_too_many_records_deletes_nothing() {
        let r = rig();
        let sealed = seal::seal(b"{}", &SECRET);
        let mut first = String::new();
        for batch in 0..5 {
            let mut ids = Vec::new();
            for i in 0..200 {
                let h = header_of(&job(&format!("k{}", batch * 200 + i)));
                let sub = r.state.ai_jobs.submit(sealed.clone(), &h).unwrap_or_else(|e| panic!("{}", e.reason));
                ids.push(sub.id);
            }
            if batch == 0 {
                first = ids[0].clone();
            }
            for id in ids {
                r.state.ai_jobs.finish(&id, Outcome::Failed("no".into()));
            }
        }
        assert_eq!(r.state.ai_jobs.counts().failed, MAX_KEPT_JOBS);
        let (s, v) = r.submit(&job("k0")).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
        assert!(r.state.ai_jobs.get(&first).is_some(), "the older failed record for the same key is still there");
        assert!(r.dir.0.join(format!("ai_jobs/{first}.json")).is_file());
        assert_eq!(r.state.ai_jobs.counts().failed, MAX_KEPT_JOBS);
    }
}
