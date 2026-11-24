//! Shared state behind an `Arc`. Later tasks add their stores and the pairing registry here.

use crate::auth::{system_clock, Auth, Clock};
use crate::config::Config;
use crate::host::{assess, Assessment, HostFacts, LoadHistory, PicturePolicy};
use crate::store::Stores;
use companion_protocol::card::{Card, Disk, Host, UsedMb};
use std::collections::{BTreeMap, VecDeque};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};

pub const PROTOCOL_VERSION: u32 = 1;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceReachability {
    Unknown,
    Online,
    NoReply,
}

#[derive(Clone)]
pub struct DeviceStatus {
    pub name: String,
    pub last_seen_ms: i64,
    address: Option<Ipv4Addr>,
    pub reachability: DeviceReachability,
}
pub const JOB_LINES_KEPT: usize = 50;
/// AI jobs listed on the status page.
pub const AI_JOBS_SHOWN: usize = 20;
/// What the card says of the runner while the operator has paused the Companion: televisions then
/// neither hand calls over nor hold their own paid providers back for a job that is not being run.
pub const PAUSED_RUNNER_REASON: &str = "paused on the Companion's status page";

/// How much of the disk budget each store holds. The stores (Task 6 onward) implement this.
///
/// Called on every card and status-page request, from async handlers. It must be cheap and
/// must not block: return a figure the store keeps up to date (a cached counter), never walk
/// the disk or take a slow lock here.
pub trait DiskUsage: Send + Sync {
    fn used_mb(&self) -> UsedMb;
}

struct Inner {
    facts: HostFacts,
    history: LoadHistory,
    paused: bool,
    assessment: Assessment,
    assessed_at_ms: i64,
    jobs: VecDeque<(i64, String)>,
    devices: BTreeMap<String, DeviceStatus>,
    paired: bool,
    household_fingerprint: Option<String>,
    pairing_code: Option<String>,
}

pub struct AppState {
    pub config: Config,
    pub budget_mb: u64,
    disk: Box<dyn DiskUsage>,
    pub stores: Arc<Stores>,
    pub guides: crate::guide_store::GuideSources,
    /// The master's sealed household bundle, held opaque for followers (Task 9b).
    pub bundle: crate::bundle_store::BundleStore,
    /// Provider calls a television handed over, sealed at rest (stage 2).
    pub ai_jobs: crate::ai_jobs::JobStore,
    /// Forwards televisions' Betfair calls; keeps only the last call's name, code and time.
    pub betfair: crate::betfair_relay::Relay,
    /// Headless household DVR captures and schedules.
    pub dvr: Arc<crate::dvr::DvrStore>,
    /// Catch-up and series episode staging store.
    pub stage: Arc<crate::stage::StageStore>,
    /// Household IPTV tuner arbiter & connection lease pool.
    pub tuner: Arc<crate::tuner::TunerStore>,
    /// Live stream relay & concurrent fan-out manager.
    pub stream_relay: Arc<crate::stream_relay::StreamRelayManager>,
    /// Home Assistant media_player bridge and TV notification hub.
    pub ha: Arc<crate::homeassistant::HaManager>,
    /// Channel lineup, custom overrides, and numbering manager.
    pub lineup: Arc<crate::lineup::LineupStore>,
    pub auth: Arc<Auth>,
    inner: Mutex<Inner>,
    /// Wall time in Unix milliseconds, for comparing with stamps televisions send. Injected so a
    /// test can pin it; production reads the system clock ([`now_ms`]).
    wall: WallClock,
}

/// Unix milliseconds. Unlike [`Clock`] (monotonic, arbitrary start) this can be compared with a
/// television's stamp.
pub type WallClock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// What the status page prints beside the card.
pub struct PageView {
    pub card: Card,
    pub paused: bool,
    pub pairing_code: Option<String>,
    pub jobs: Vec<(i64, String)>,
    pub devices: Vec<DeviceStatus>,
    /// The newest AI jobs (the runner's), newest first.
    pub ai_jobs: Vec<crate::ai_jobs::JobSummary>,
    /// Wall time when the view was taken, for the jobs' ages.
    pub now_ms: i64,
    /// The Betfair relay's last call (never its headers or body), if any since start.
    pub betfair_last: Option<crate::betfair_relay::LastCall>,
    /// Each registered guide and what became of its parse.
    pub guides: Vec<(String, String, String)>,
    /// Household DVR recordings.
    pub dvr_recordings: Vec<crate::dvr::RecordingEntry>,
    /// Active live stream relays.
    pub active_relays: Vec<crate::stream_relay::RelaySummary>,
    pub notifications_enabled: bool,
    pub suppress_cctv_duplicates: bool,
    pub catchup_ad_skip_enabled: bool,
    pub catchup_live_handoff_enabled: bool,
    pub lineup_overrides_count: usize,
    pub lineup_channels_count: usize,
    pub lineup_channels: Vec<crate::lineup::ChannelSummary>,
}

impl AppState {
    pub fn new(config: Config, facts: HostFacts, stores: Arc<Stores>) -> AppState {
        AppState::with_clock(config, facts, stores, system_clock())
    }

    pub fn with_clock(config: Config, facts: HostFacts, stores: Arc<Stores>, clock: Clock) -> AppState {
        let budget_mb = stores.budget.limit_mb();
        let mut history = LoadHistory::default();
        history.push(facts.load_1m, facts.cores);
        let policy = picture_policy(&config);
        let assessment = assess(&facts, &history, false, &policy);
        let auth = Arc::new(Auth::new(config.data_dir.clone(), clock));
        let view = auth.snapshot();
        let wall: WallClock = Arc::new(now_ms);
        AppState {
            budget_mb,
            disk: Box::new(stores.budget.clone()),
            guides: crate::guide_store::GuideSources::load(&config.data_dir),
            bundle: crate::bundle_store::BundleStore::open(&config.data_dir),
            ai_jobs: crate::ai_jobs::JobStore::open(&config.data_dir, wall.clone()),
            betfair: crate::betfair_relay::Relay::new(),
            dvr: crate::dvr::DvrStore::open(&config.data_dir, wall.clone()),
            stage: crate::stage::StageStore::open(&config.data_dir, wall.clone()),
            tuner: crate::tuner::TunerStore::new(wall.clone()),
            ha: crate::homeassistant::HaManager::with_data_dir(
                config.homeassistant_enabled,
                config.hass_url.clone(),
                config.hass_token.clone(),
                Some(config.data_dir.clone()),
                wall.clone(),
            ),
            lineup: Arc::new(crate::lineup::LineupStore::new(config.data_dir.clone(), wall.clone())),
            stream_relay: crate::stream_relay::StreamRelayManager::new(
                wall.clone(),
                Some(config.data_dir.clone()),
                config.timeshift_enabled,
                config.timeshift_minutes,
            ),
            stores,
            auth,
            inner: Mutex::new(Inner {
                facts,
                history,
                paused: false,
                assessment,
                assessed_at_ms: now_ms(),
                jobs: VecDeque::new(),
                devices: BTreeMap::new(),
                paired: view.paired,
                household_fingerprint: view.fingerprint,
                pairing_code: view.code,
            }),
            config,
            wall,
        }
    }

    /// Wall time now, from the injected [`WallClock`].
    pub fn wall_ms(&self) -> i64 {
        (self.wall)()
    }

    /// The same state on a pinned wall clock.
    #[cfg(test)]
    pub fn with_wall_clock(mut self, wall: WallClock) -> AppState {
        self.ai_jobs.set_clock(wall.clone());
        if let Some(dvr) = Arc::get_mut(&mut self.dvr) {
            dvr.set_clock(wall.clone());
        }
        if let Some(stage) = Arc::get_mut(&mut self.stage) {
            stage.set_clock(wall.clone());
        }
        if let Some(tuner) = Arc::get_mut(&mut self.tuner) {
            tuner.set_clock(wall.clone());
        }
        self.stream_relay = crate::stream_relay::StreamRelayManager::new(
            wall.clone(),
            Some(self.config.data_dir.clone()),
            self.config.timeshift_enabled,
            self.config.timeshift_minutes,
        );
        self.wall = wall;
        self
    }

    /// A poisoned lock means another thread panicked mid-update; the data is still readable.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn card(&self) -> Card {
        let mut used = self.disk.used_mb();
        used.dvr = self.dvr.used_mb();
        used.stage = self.stage.used_mb();
        let inner = self.lock();
        let mut card = build_card(&inner, self.budget_mb, used, self.bundle.captured_at_ms());
        apply_ai_runner(&mut card, self.config.ai_runner, inner.paused);
        apply_betfair_relay(&mut card, self.config.betfair_relay, inner.paused);
        apply_dvr(&mut card, self.config.dvr_enabled, inner.paused, &inner.facts);
        apply_stage(&mut card, self.config.stage_enabled, inner.paused, &inner.facts);
        apply_stream_relay(&mut card, self.config.stream_relay, inner.paused);
        apply_timeshift(&mut card, self.config.timeshift_enabled, inner.paused);
        apply_transcode(&mut card, self.config.transcode_enabled, inner.paused);
        apply_tuner(&mut card, self.config.tuner_enabled, inner.paused);
        apply_homeassistant(&mut card, self.config.homeassistant_enabled, inner.paused, self.ha.notifications_enabled());
        card
    }

    /// The card as the wire carries it: the protocol's card plus `aiJobs`, the counts of jobs the
    /// runner holds, and `capabilities.betfairRelay`. (The extra keys are read by televisions that
    /// know them and ignored by the rest; the protocol crate, which ships inside every television's
    /// native library, is left alone.)
    pub fn card_json(&self) -> serde_json::Result<String> {
        let card = self.card();
        let relay_on = !card.declined.contains_key(crate::betfair_relay::CAPABILITY);
        let mut v = serde_json::to_value(card)?;
        v["aiJobs"] = serde_json::to_value(self.ai_jobs.counts())?;
        v["capabilities"][crate::betfair_relay::CAPABILITY] = serde_json::Value::Bool(relay_on);
        serde_json::to_string(&v)
    }

    /// Why the Betfair relay is not serving right now, or `None` when it is. The card says the same.
    pub fn betfair_relay_declined(&self) -> Option<String> {
        betfair_relay_refusal(self.config.betfair_relay, self.lock().paused).map(str::to_string)
    }

    pub fn page_view(&self) -> PageView {
        let mut used = self.disk.used_mb();
        used.dvr = self.dvr.used_mb();
        used.stage = self.stage.used_mb();
        let ai_jobs = self.ai_jobs.recent(AI_JOBS_SHOWN);
        let now_ms = self.wall_ms();
        let inner = self.lock();
        PageView {
            card: {
                let mut card = build_card(&inner, self.budget_mb, used, self.bundle.captured_at_ms());
                apply_ai_runner(&mut card, self.config.ai_runner, inner.paused);
                apply_betfair_relay(&mut card, self.config.betfair_relay, inner.paused);
                apply_dvr(&mut card, self.config.dvr_enabled, inner.paused, &inner.facts);
                apply_stage(&mut card, self.config.stage_enabled, inner.paused, &inner.facts);
                apply_stream_relay(&mut card, self.config.stream_relay, inner.paused);
                apply_timeshift(&mut card, self.config.timeshift_enabled, inner.paused);
                apply_transcode(&mut card, self.config.transcode_enabled, inner.paused);
                apply_tuner(&mut card, self.config.tuner_enabled, inner.paused);
                apply_homeassistant(&mut card, self.config.homeassistant_enabled, inner.paused, self.ha.notifications_enabled());
                card
            },
            paused: inner.paused,
            pairing_code: if inner.paired { None } else { inner.pairing_code.clone() },
            jobs: inner.jobs.iter().cloned().collect(),
            devices: inner.devices.values().cloned().collect(),
            ai_jobs,
            now_ms,
            betfair_last: self.betfair.last_call(),
            guides: self.guides.parse_rows(&self.stores.guide),
            dvr_recordings: self.dvr.list(),
            active_relays: self.stream_relay.active_summaries(),
            notifications_enabled: self.ha.notifications_enabled(),
            suppress_cctv_duplicates: self.ha.suppress_cctv_duplicates(),
            catchup_ad_skip_enabled: self.ha.catchup_ad_skip_enabled(),
            catchup_live_handoff_enabled: self.ha.catchup_live_handoff_enabled(),
            lineup_overrides_count: self.lineup.overrides_count(),
            lineup_channels_count: self.lineup.channels_count(),
            lineup_channels: self.lineup.get_channels(),
        }
    }

    pub fn facts(&self) -> HostFacts {
        self.lock().facts.clone()
    }

    pub fn is_paused(&self) -> bool {
        self.lock().paused
    }

    pub fn set_paused(&self, paused: bool) {
        {
            let mut inner = self.lock();
            inner.paused = paused;
        }
        self.push_job(if paused { "paused from the status page" } else { "resumed from the status page" });
        self.reassess();
    }

    /// A fresh measurement: records a load sample, then decides again.
    pub fn update_facts(&self, facts: HostFacts) {
        {
            let mut inner = self.lock();
            inner.history.push(facts.load_1m, facts.cores);
            inner.facts = facts;
        }
        self.reassess();
    }

    pub fn set_bench_score(&self, score: u64) {
        self.lock().facts.bench_score = score;
    }

    fn reassess(&self) {
        let policy = picture_policy(&self.config);
        let mut inner = self.lock();
        inner.assessment = assess(&inner.facts, &inner.history, inner.paused, &policy);
        inner.assessed_at_ms = now_ms();
    }

    /// Newest last; only the last 50 are kept.
    pub fn push_job(&self, line: &str) {
        self.push_job_at(self.wall_ms(), line);
    }

    pub fn push_job_at(&self, at: i64, line: &str) {
        let mut inner = self.lock();
        inner.jobs.push_back((at, line.chars().take(300).collect()));
        while inner.jobs.len() > JOB_LINES_KEPT {
            inner.jobs.pop_front();
        }
    }

    pub fn see_device(&self, id: String, name: String, address: Option<Ipv4Addr>) {
        let mut inner = self.lock();
        let now = self.wall_ms();
        if inner.devices.len() >= 32 && !inner.devices.contains_key(&id) {
            if let Some(oldest) = inner.devices.iter().min_by_key(|(_, device)| device.last_seen_ms).map(|(id, _)| id.clone()) {
                inner.devices.remove(&oldest);
            }
        }
        inner.devices.insert(id, DeviceStatus {
            name, last_seen_ms: now, address,
            reachability: if address.is_some() { DeviceReachability::Online } else { DeviceReachability::Unknown },
        });
    }

    pub fn device_probe_targets(&self) -> Vec<(String, Ipv4Addr, i64)> {
        self.lock().devices.iter().filter_map(|(id, device)| {
            device.address.map(|address| (id.clone(), address, device.last_seen_ms))
        }).collect()
    }

    pub fn record_device_probe(&self, id: &str, address: Ipv4Addr, last_seen_ms: i64, result: DeviceReachability) {
        if let Some(device) = self.lock().devices.get_mut(id) {
            if device.address == Some(address) && device.last_seen_ms == last_seen_ms {
                device.reachability = result;
            }
        }
    }

    /// Sets the card's pairing fields directly (the status-page tests need states the flow never rests in).
    #[cfg(test)]
    pub fn set_pairing(&self, paired: bool, fingerprint: Option<String>, code: Option<String>) {
        let mut inner = self.lock();
        inner.paired = paired;
        inner.household_fingerprint = fingerprint;
        inner.pairing_code = code;
    }

    /// Copies the pairing state (paired, fingerprint, code) onto the card and status page.
    pub fn sync_pairing(&self) {
        let view = self.auth.snapshot();
        let mut inner = self.lock();
        inner.paired = view.paired;
        inner.household_fingerprint = view.fingerprint;
        inner.pairing_code = view.code;
    }
}

/// Unpairs, and on success empties the stores' memory and counters too (the folders were deleted
/// with the secret; this makes the running Companion agree with them). It runs in its own task so
/// a client that hangs up cannot leave the stores half-reset.
pub async fn unpair(state: &Arc<AppState>) -> Result<(), crate::auth::Refusal> {
    let s = state.clone();
    let result = tokio::spawn(async move {
        let result = s.auth.unpair().await;
        if result.is_ok() {
            s.stores.reset().await;
            s.bundle.reset().await;
            s.ai_jobs.reset();
            let _ = s.dvr.reset().await;
            s.stage.reset().await;
            s.tuner.reset().await;
            let _ = s.lineup.clear();
            s.lock().devices.clear();
            // What the stores' own reset does not reach: the registrations (they hold provider
            // logins) and a picture folder that was never opened because the store was off.
            if let Err(why) = s.guides.clear().await {
                eprintln!("unpair: {why}");
            }
            // Only the Companion's own subfolder: the rest of the operator's folder is theirs.
            if let Some(dir) = s.config.picture_dir.as_ref().map(|d| crate::store::picture_root(Some(d), &s.config.data_dir)) {
                match tokio::fs::remove_dir_all(&dir).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => eprintln!("unpair: cannot delete the picture folder {}: {e}", dir.display()),
                }
            }
        }
        s.sync_pairing();
        result
    })
    .await
    .unwrap_or_else(|e| Err(crate::auth::Refusal::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("unpair failed: {e}"))));
    state.sync_pairing();
    result
}

/// The runner is on when the operator has not switched it off and the stores themselves are
/// allowed to run (the same floor) and the Companion is not paused; otherwise the card says why not.
fn apply_ai_runner(card: &mut Card, enabled: bool, paused: bool) {
    if !enabled {
        card.capabilities.ai_runner = false;
        card.declined.insert("aiRunner".to_string(), crate::ai_jobs::RUNNER_OFF_REASON.to_string());
    } else if paused {
        card.capabilities.ai_runner = false;
        card.declined.insert("aiRunner".to_string(), PAUSED_RUNNER_REASON.to_string());
    } else if !card.capabilities.ai_store {
        card.capabilities.ai_runner = false;
        let why = card.declined.get("aiStore").cloned().unwrap_or_else(|| "this machine is below the Companion's minimum".to_string());
        card.declined.insert("aiRunner".to_string(), why);
    } else {
        card.capabilities.ai_runner = true;
        card.declined.remove("aiRunner");
    }
}

/// The relay needs no disk, so only the operator's option and the pause decide it. Paused, the card
/// already reads busy and televisions call Betfair themselves; the route agrees with the card.
fn betfair_relay_refusal(enabled: bool, paused: bool) -> Option<&'static str> {
    if !enabled {
        Some(crate::betfair_relay::RELAY_OFF_REASON)
    } else if paused {
        Some(PAUSED_RUNNER_REASON)
    } else {
        None
    }
}

fn apply_betfair_relay(card: &mut Card, enabled: bool, paused: bool) {
    match betfair_relay_refusal(enabled, paused) {
        Some(why) => {
            card.declined.insert(crate::betfair_relay::CAPABILITY.to_string(), why.to_string());
        }
        None => {
            card.declined.remove(crate::betfair_relay::CAPABILITY);
        }
    }
}

pub const DVR_OFF_REASON: &str = "switched off in the Companion's options (dvr)";

fn apply_dvr(card: &mut Card, enabled: bool, paused: bool, facts: &HostFacts) {
    if !enabled {
        card.capabilities.dvr = false;
        card.declined.insert("dvr".to_string(), DVR_OFF_REASON.to_string());
    } else if paused {
        card.capabilities.dvr = false;
        card.declined.insert("dvr".to_string(), PAUSED_RUNNER_REASON.to_string());
    } else if facts.disk_free_mb < crate::host::MIN_FREE_DISK_MB {
        card.capabilities.dvr = false;
        card.declined.insert(
            "dvr".to_string(),
            format!("the data folder has {} MB free; DVR needs at least {} MB (2 GB)", facts.disk_free_mb, crate::host::MIN_FREE_DISK_MB),
        );
    } else if facts.free_ram_mb < crate::host::MIN_FREE_RAM_MB {
        card.capabilities.dvr = false;
        card.declined.insert(
            "dvr".to_string(),
            format!("free memory {} MB; DVR needs at least {} MB free", facts.free_ram_mb, crate::host::MIN_FREE_RAM_MB),
        );
    } else {
        card.capabilities.dvr = true;
        card.declined.remove("dvr");
    }
}

pub const STAGE_OFF_REASON: &str = "switched off in the Companion's options (stage)";

fn apply_stage(card: &mut Card, enabled: bool, paused: bool, facts: &HostFacts) {
    if !enabled {
        card.capabilities.stage = false;
        card.declined.insert("stage".to_string(), STAGE_OFF_REASON.to_string());
    } else if paused {
        card.capabilities.stage = false;
        card.declined.insert("stage".to_string(), PAUSED_RUNNER_REASON.to_string());
    } else if facts.disk_free_mb < crate::host::MIN_FREE_DISK_MB {
        card.capabilities.stage = false;
        card.declined.insert(
            "stage".to_string(),
            format!("the data folder has {} MB free; staging needs at least {} MB (2 GB)", facts.disk_free_mb, crate::host::MIN_FREE_DISK_MB),
        );
    } else {
        card.capabilities.stage = true;
        card.declined.remove("stage");
    }
}

pub const STREAM_RELAY_OFF_REASON: &str = "switched off in the Companion's options (stream_relay)";

fn apply_stream_relay(card: &mut Card, enabled: bool, paused: bool) {
    if !enabled {
        card.capabilities.stream_relay = false;
        card.declined.insert("streamRelay".to_string(), STREAM_RELAY_OFF_REASON.to_string());
    } else if paused {
        card.capabilities.stream_relay = false;
        card.declined.insert("streamRelay".to_string(), PAUSED_RUNNER_REASON.to_string());
    } else {
        card.capabilities.stream_relay = true;
        card.declined.remove("streamRelay");
    }
}

pub const TIMESHIFT_OFF_REASON: &str = "switched off in the Companion's options (timeshift)";

fn apply_timeshift(card: &mut Card, enabled: bool, paused: bool) {
    if !enabled {
        card.capabilities.timeshift = false;
        card.declined.insert("timeshift".to_string(), TIMESHIFT_OFF_REASON.to_string());
    } else if paused {
        card.capabilities.timeshift = false;
        card.declined.insert("timeshift".to_string(), PAUSED_RUNNER_REASON.to_string());
    } else {
        card.capabilities.timeshift = true;
        card.declined.remove("timeshift");
    }
}

pub const TRANSCODE_OFF_REASON: &str = "switched off in the Companion's options (transcode)";

fn apply_transcode(card: &mut Card, enabled: bool, paused: bool) {
    if !enabled {
        card.capabilities.transcode = false;
        card.declined.insert("transcode".to_string(), TRANSCODE_OFF_REASON.to_string());
    } else if paused {
        card.capabilities.transcode = false;
        card.declined.insert("transcode".to_string(), PAUSED_RUNNER_REASON.to_string());
    } else {
        card.capabilities.transcode = true;
        card.declined.remove("transcode");
    }
}

pub const TUNER_OFF_REASON: &str = "switched off in the Companion's options (tuner)";

fn apply_tuner(card: &mut Card, enabled: bool, paused: bool) {
    if !enabled {
        card.capabilities.tuner = false;
        card.declined.insert("tuner".to_string(), TUNER_OFF_REASON.to_string());
    } else if paused {
        card.capabilities.tuner = false;
        card.declined.insert("tuner".to_string(), PAUSED_RUNNER_REASON.to_string());
    } else {
        card.capabilities.tuner = true;
        card.declined.remove("tuner");
    }
}

pub const HOMEASSISTANT_OFF_REASON: &str = "switched off in the Companion's options (homeassistant)";

fn apply_homeassistant(card: &mut Card, enabled: bool, paused: bool, notifications_enabled: bool) {
    if !enabled {
        card.capabilities.homeassistant = false;
        card.capabilities.notifications = false;
        card.declined.insert("homeassistant".to_string(), HOMEASSISTANT_OFF_REASON.to_string());
    } else if paused {
        card.capabilities.homeassistant = false;
        card.capabilities.notifications = false;
        card.declined.insert("homeassistant".to_string(), PAUSED_RUNNER_REASON.to_string());
    } else {
        card.capabilities.homeassistant = true;
        card.capabilities.notifications = notifications_enabled;
        card.declined.remove("homeassistant");
    }
}

fn picture_policy(config: &Config) -> PicturePolicy {
    PicturePolicy { enabled: config.picture_store_enabled, picture_dir: config.picture_dir.as_ref().map(|p| p.display().to_string()) }
}

fn build_card(inner: &Inner, budget_mb: u64, used: UsedMb, household_bundle_at_ms: Option<u64>) -> Card {
    Card {
        protocol: vec![PROTOCOL_VERSION],
        version: env!("CARGO_PKG_VERSION").to_string(),
        host: Host {
            arch: inner.facts.arch.clone(),
            ram_mb: inner.facts.ram_mb,
            free_ram_mb: inner.facts.free_ram_mb,
            disk_free_mb: inner.facts.disk_free_mb,
            on_sd_card: inner.facts.on_sd_card,
            bench_score: inner.facts.bench_score,
            label: inner.facts.label.clone(),
        },
        state: inner.assessment.state.clone(),
        busy_reason: inner.assessment.busy_reason.clone(),
        paired: inner.paired,
        household_fingerprint: inner.household_fingerprint.clone(),
        capabilities: inner.assessment.capabilities.clone(),
        declined: inner.assessment.declined.clone(),
        disk: Disk { budget_mb, used_mb: used },
        assessed_at_ms: inner.assessed_at_ms,
        household_bundle_at_ms,
    }
}

/// A test Companion on a clock that stands still. Nonces, codes and pairings are judged on this
/// clock, so a test that takes longer than a nonce's ten seconds on a loaded machine (sealing a
/// 2 MiB body under a load average of 200) still passes or fails on its own merits. Tests that are
/// about time passing use [`test_state_with`] and move the clock themselves.
#[cfg(test)]
pub fn test_state(config: Config) -> AppState {
    test_state_with(config, frozen_clock())
}

#[cfg(test)]
pub fn frozen_clock() -> Clock {
    Arc::new(|| 1_000_000)
}

#[cfg(test)]
pub fn test_state_with(mut config: Config, clock: Clock) -> AppState {
    let facts = HostFacts {
        arch: "amd64".into(),
        ram_mb: 3870,
        free_ram_mb: 1410,
        disk_free_mb: 402_000,
        on_sd_card: false,
        picture_dir_on_sd: None,
        bench_score: 800,
        cores: 4,
        load_1m: 0.5,
        label: "Test host".into(),
    };
    // The stores live in the data folder; a test that names none gets a private one.
    if !config.data_dir.is_dir() {
        let t = crate::household::TempDir::new();
        config.data_dir = t.0.clone();
        std::mem::forget(t);
    }
    let free = Arc::new(crate::store::FixedFree(std::sync::atomic::AtomicU64::new(500 * 1024 * 1024 * 1024)));
    let stores = Arc::new(Stores::open_with(&config, facts.disk_free_mb, free).expect("test stores"));
    AppState::with_clock(config, facts, stores, clock)
}

#[cfg(test)]
pub fn test_config() -> Config {
    Config { data_dir: "/data".into(), picture_dir: None, port: 8765, disk_budget_mb: None, picture_store_enabled: false, ai_runner: true, betfair_relay: true, dvr_enabled: true, stream_relay: true, timeshift_enabled: true, timeshift_minutes: 60, transcode_enabled: true, stage_enabled: true, tuner_enabled: true, homeassistant_enabled: true, hass_url: None, hass_token: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_carries_the_assessment_and_the_default_budget() {
        let c = test_state(test_config()).card();
        assert_eq!(c.protocol, vec![1]);
        assert_eq!(c.state, "available");
        assert!(!c.paired && c.household_fingerprint.is_none());
        assert_eq!(c.disk.budget_mb, 20_480);
        assert_eq!(c.disk.used_mb, UsedMb { ai: 0, guide: 0, picture: 0, dvr: 0, stage: 0 });
        assert!(c.capabilities.ai_store && !c.capabilities.picture_store && c.capabilities.dvr && c.capabilities.stage && c.capabilities.tuner);
    }

    #[test]
    fn pausing_shows_on_the_card_and_resuming_clears_it() {
        let s = test_state(test_config());
        s.set_paused(true);
        let c = s.card();
        assert_eq!((c.state.as_str(), c.busy_reason.as_deref()), ("busy", Some("paused on the status page")));
        s.set_paused(false);
        assert_eq!(s.card().state, "available");
    }

    #[test]
    fn paused_runner_is_declined_on_the_card_so_televisions_neither_hand_off_nor_hold() {
        let s = test_state(test_config());
        assert!(s.card().capabilities.ai_runner);
        s.set_paused(true);
        let c = s.card();
        assert!(!c.capabilities.ai_runner);
        assert_eq!(c.declined.get("aiRunner").map(String::as_str), Some(PAUSED_RUNNER_REASON));
        assert_eq!(PAUSED_RUNNER_REASON, "paused on the Companion's status page");
        assert!(!s.page_view().card.capabilities.ai_runner, "the status page shows what the card says");
        s.set_paused(false);
        let c = s.card();
        assert!(c.capabilities.ai_runner && !c.declined.contains_key("aiRunner"));
        // Switched off in the options still says so, paused or not.
        let mut off = test_config();
        off.ai_runner = false;
        let s = test_state(off);
        s.set_paused(true);
        assert_eq!(s.card().declined["aiRunner"], crate::ai_jobs::RUNNER_OFF_REASON);
    }

    #[test]
    fn timeshift_capability_follows_config_and_pause() {
        let mut cfg = test_config();
        cfg.timeshift_enabled = false;
        let s = test_state(cfg);
        let c = s.card();
        assert!(!c.capabilities.timeshift);
        assert_eq!(c.declined.get("timeshift").map(String::as_str), Some(TIMESHIFT_OFF_REASON));

        let s2 = test_state(test_config());
        assert!(s2.card().capabilities.timeshift);
        s2.set_paused(true);
        assert!(!s2.card().capabilities.timeshift);
        assert_eq!(s2.card().declined.get("timeshift").map(String::as_str), Some(PAUSED_RUNNER_REASON));
    }

    #[test]
    fn transcode_capability_follows_config_and_pause() {
        let mut cfg = test_config();
        cfg.transcode_enabled = false;
        let s = test_state(cfg);
        let c = s.card();
        assert!(!c.capabilities.transcode);
        assert_eq!(c.declined.get("transcode").map(String::as_str), Some(TRANSCODE_OFF_REASON));

        let s2 = test_state(test_config());
        assert!(s2.card().capabilities.transcode);
        s2.set_paused(true);
        assert!(!s2.card().capabilities.transcode);
        assert_eq!(s2.card().declined.get("transcode").map(String::as_str), Some(PAUSED_RUNNER_REASON));
    }

    #[test]
    fn stage_capability_follows_config_and_pause() {
        let mut cfg = test_config();
        cfg.stage_enabled = false;
        let s = test_state(cfg);
        let c = s.card();
        assert!(!c.capabilities.stage);
        assert_eq!(c.declined.get("stage").map(String::as_str), Some(STAGE_OFF_REASON));

        let s2 = test_state(test_config());
        assert!(s2.card().capabilities.stage);
        s2.set_paused(true);
        assert!(!s2.card().capabilities.stage);
        assert_eq!(s2.card().declined.get("stage").map(String::as_str), Some(PAUSED_RUNNER_REASON));
    }

    #[test]
    fn tuner_capability_follows_config_and_pause() {
        let mut cfg = test_config();
        cfg.tuner_enabled = false;
        let s = test_state(cfg);
        let c = s.card();
        assert!(!c.capabilities.tuner);
        assert_eq!(c.declined.get("tuner").map(String::as_str), Some(TUNER_OFF_REASON));

        let s2 = test_state(test_config());
        assert!(s2.card().capabilities.tuner);
        s2.set_paused(true);
        assert!(!s2.card().capabilities.tuner);
        assert_eq!(s2.card().declined.get("tuner").map(String::as_str), Some(PAUSED_RUNNER_REASON));
    }

    #[test]
    fn dvr_capability_follows_config_pause_and_disk_free() {
        let mut cfg = test_config();
        cfg.dvr_enabled = false;
        let s = test_state(cfg);
        let c = s.card();
        assert!(!c.capabilities.dvr);
        assert_eq!(c.declined.get("dvr").map(String::as_str), Some(DVR_OFF_REASON));

        let s2 = test_state(test_config());
        assert!(s2.card().capabilities.dvr);
        s2.set_paused(true);
        assert!(!s2.card().capabilities.dvr);
        assert_eq!(s2.card().declined.get("dvr").map(String::as_str), Some(PAUSED_RUNNER_REASON));
    }

    #[test]
    fn job_log_keeps_the_last_fifty() {
        let s = test_state(test_config());
        for i in 0..80 {
            s.push_job(&format!("job {i}"));
        }
        let jobs = s.page_view().jobs;
        assert_eq!(jobs.len(), 50);
        assert_eq!(jobs.last().map(|(_, line)| line.as_str()), Some("job 79"));
    }
}
