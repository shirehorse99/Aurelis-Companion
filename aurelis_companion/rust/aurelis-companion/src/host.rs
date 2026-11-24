//! What the host is (measured) and what that means (decided).
//! `gather_facts` is the only function here that touches the real machine; everything else
//! is pure over `HostFacts` and `LoadHistory`.

use companion_protocol::card::Capabilities;
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

pub const MIN_RAM_MB: u64 = 900;
pub const MIN_FREE_DISK_MB: u64 = 2048;
pub const MIN_FREE_RAM_MB: u64 = 300;
pub const BUSY_LOAD_FRACTION: f64 = 0.70;
pub const BUDGET_CAP_MB: u64 = 20 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct HostFacts {
    pub arch: String,
    pub ram_mb: u64,
    pub free_ram_mb: u64,
    pub disk_free_mb: u64,
    pub on_sd_card: bool,
    /// Whether the configured picture folder sits on an SD card; `None` when none is configured.
    pub picture_dir_on_sd: Option<bool>,
    pub bench_score: u64,
    pub cores: u32,
    pub load_1m: f64,
    pub label: String,
}

/// What the operator chose for the picture store.
#[derive(Debug, Clone, PartialEq)]
pub struct PicturePolicy {
    pub enabled: bool,
    /// The configured picture folder, if any.
    pub picture_dir: Option<String>,
}

/// The last few load readings, newest last, as (load, cores).
#[derive(Debug, Clone, Default)]
pub struct LoadHistory {
    samples: VecDeque<(f64, u32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub state: String,
    pub busy_reason: Option<String>,
    pub capabilities: Capabilities,
    pub declined: BTreeMap<String, String>,
}

impl LoadHistory {
    pub fn push(&mut self, load_1m: f64, cores: u32) {
        self.samples.push_back((load_1m, cores.max(1)));
        while self.samples.len() > 2 {
            self.samples.pop_front();
        }
    }

    /// `Some(reason)` when the last two samples were both above the busy line.
    pub fn sustained_high(&self) -> Option<String> {
        if self.samples.len() < 2 {
            return None;
        }
        let high = |(load, cores): &(f64, u32)| *load > BUSY_LOAD_FRACTION * f64::from(*cores);
        if !self.samples.iter().all(high) {
            return None;
        }
        let (load, cores) = self.samples.back()?;
        Some(format!("load {load:.1} on {cores} cores"))
    }
}

pub fn check_floor(facts: &HostFacts) -> Result<(), String> {
    if facts.arch != "amd64" && facts.arch != "arm64" {
        return Err(format!("this machine is {} and the Companion needs a 64-bit amd64 or arm64 machine", facts.arch));
    }
    if facts.ram_mb < MIN_RAM_MB {
        return Err(format!("this machine has {} MB of RAM and the Companion needs at least {MIN_RAM_MB} MB (a 1 GB board reports about 900)", facts.ram_mb));
    }
    if facts.disk_free_mb < MIN_FREE_DISK_MB {
        return Err(format!(
            "the data folder has {} MB free and the Companion needs at least {MIN_FREE_DISK_MB} MB (2 GB)",
            facts.disk_free_mb
        ));
    }
    Ok(())
}

pub fn assess(facts: &HostFacts, history: &LoadHistory, paused: bool, pictures: &PicturePolicy) -> Assessment {
    let busy_reason = if paused {
        Some("paused on the status page".to_string())
    } else if facts.free_ram_mb < MIN_FREE_RAM_MB {
        Some(format!("free memory {} MB (needs {MIN_FREE_RAM_MB} MB)", facts.free_ram_mb))
    } else {
        history.sustained_high()
    };

    let mut declined = BTreeMap::new();
    let floor = check_floor(facts);
    let stores = floor.is_ok();
    if let Err(why) = &floor {
        declined.insert("aiStore".to_string(), why.clone());
        declined.insert("guideStore".to_string(), why.clone());
    }

    let picture_dir_ok: Result<(), String> = match (&pictures.picture_dir, facts.picture_dir_on_sd) {
        (Some(_), Some(false)) => Ok(()),
        (Some(dir), Some(true)) => Err(format!("the picture folder {dir} is on an SD card or eMMC; choose other storage")),
        (Some(dir), None) => Err(format!("cannot tell what storage {dir} is on")),
        (None, _) if facts.on_sd_card => Err("the data folder is on an SD card or eMMC; set a picture folder on other storage".to_string()),
        (None, _) => Ok(()),
    };
    let picture_store = if !pictures.enabled {
        declined.insert("pictureStore".to_string(), "switched off in the Companion's options (pictures)".to_string());
        false
    } else if !stores {
        declined.insert("pictureStore".to_string(), "this machine is below the Companion's minimum".to_string());
        false
    } else if let Err(why) = picture_dir_ok {
        declined.insert("pictureStore".to_string(), why);
        false
    } else {
        true
    };
    declined.insert("aiRunner".to_string(), "stage 2".to_string());
    let heavy_pass = if !stores {
        declined.insert("heavyPass".to_string(), "this machine is below the Companion's minimum".to_string());
        false
    } else if facts.free_ram_mb < crate::guide_parse::MIN_PARSE_FREE_MB {
        declined.insert(
            "heavyPass".to_string(),
            format!("{} MB free; parsing a guide needs at least {} MB free", facts.free_ram_mb, crate::guide_parse::MIN_PARSE_FREE_MB),
        );
        false
    } else {
        true
    };

    Assessment {
        state: if busy_reason.is_some() { "busy" } else { "available" }.to_string(),
        busy_reason,
        capabilities: Capabilities { ai_store: stores, guide_store: stores, picture_store, ai_runner: false, heavy_pass, dvr: false, stream_relay: true, timeshift: true, transcode: true, stage: false, tuner: true, homeassistant: true, notifications: true, lineup: true },
        declined,
    }
}

/// 10% of the free space at first start, capped at 20 GB.
pub fn default_budget_mb(disk_free_mb: u64) -> u64 {
    (disk_free_mb / 10).min(BUDGET_CAP_MB)
}

// ---- the one part that touches the real host -------------------------------------------------

fn normalise_arch(arch: &str) -> String {
    match arch {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        other => other.to_string(),
    }
}

fn read_trim(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\0').to_string();
    if text.is_empty() || text.eq_ignore_ascii_case("to be filled by o.e.m.") || text.eq_ignore_ascii_case("default string") {
        None
    } else {
        Some(text)
    }
}

pub fn host_label() -> String {
    let base = match (read_trim("/sys/class/dmi/id/sys_vendor"), read_trim("/sys/class/dmi/id/product_name")) {
        (Some(v), Some(p)) => format!("{v} {p}"),
        (None, Some(p)) => p,
        (Some(v), None) => v,
        (None, None) => read_trim("/proc/device-tree/model")
            .unwrap_or_else(|| sysinfo::System::name().unwrap_or_else(|| std::env::consts::OS.to_string())),
    };
    let base: String = base.chars().take(48).collect();
    if std::env::var_os("SUPERVISOR_TOKEN").is_some() {
        format!("{base} (HA OS)")
    } else if Path::new("/.dockerenv").exists() {
        format!("{base} (Docker)")
    } else {
        base
    }
}

fn disk_entry<'a>(disks: &'a sysinfo::Disks, path: &Path) -> Result<&'a sysinfo::Disk, String> {
    let path = std::fs::canonicalize(path).map_err(|e| format!("{} cannot be resolved: {e}", path.display()))?;
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .ok_or_else(|| format!("no mounted disk holds {}", path.display()))
}

/// The disk holding `path`: (free MB, mounted from an `mmcblk*` device), or why it cannot be told.
fn disk_of(disks: &sysinfo::Disks, path: &Path) -> Result<(u64, bool), String> {
    let best = disk_entry(disks, path)?;
    let on_sd = best.name().to_string_lossy().contains("mmcblk");
    Ok((best.available_space() / (1024 * 1024), on_sd))
}

/// Free bytes on the disk holding `path`, measured now (a syscall plus a mount listing: call it
/// from a blocking thread, once per write, never per read).
pub fn free_disk_bytes(path: &Path) -> Result<u64, String> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    Ok(disk_entry(&disks, path)?.available_space())
}

/// Memory a new allocation could use. Linux: the kernel's MemAvailable, which sysinfo reports as
/// `available_memory`. Elsewhere (a native run on a Mac) sysinfo's figure subtracts the compressor
/// and reads 0 on a machine that is merely using it, which would leave the card "busy" for ever; so
/// there the larger of sysinfo's available and free figures stands, and on macOS the free, inactive
/// and purgeable pages `vm_stat` reports count too.
fn available_bytes(sys: &sysinfo::System) -> u64 {
    let available = sys.available_memory();
    if cfg!(target_os = "linux") {
        return available;
    }
    let mut best = available.max(sys.free_memory());
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("vm_stat").output() {
            if let Some(b) = parse_vm_stat_reclaimable(&String::from_utf8_lossy(&out.stdout)) {
                best = best.max(b);
            }
        }
    }
    best
}

/// Free + inactive + purgeable pages, in bytes, from `vm_stat` text.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn parse_vm_stat_reclaimable(text: &str) -> Option<u64> {
    let mut lines = text.lines();
    let page: u64 = lines.next()?.split("page size of ").nth(1)?.split_whitespace().next()?.parse().ok()?;
    let mut pages = 0u64;
    let mut seen = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        if matches!(name.trim(), "Pages free" | "Pages inactive" | "Pages purgeable") {
            if let Ok(n) = value.trim().trim_end_matches('.').parse::<u64>() {
                pages += n;
                seen = true;
            }
        }
    }
    seen.then(|| pages * page)
}

/// Measures the machine. `bench_score` is carried over from the last benchmark, because the
/// benchmark is slow and runs on its own schedule.
pub fn gather_facts(data_dir: &Path, picture_dir: Option<&Path>, bench_score: u64) -> Result<HostFacts, String> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let mut ram = sys.total_memory();
    let mut free = available_bytes(&sys);
    if let Some(limits) = sys.cgroup_limits() {
        // Inside a container the machine's figures are not the Companion's.
        if limits.total_memory > 0 && limits.total_memory < ram {
            ram = limits.total_memory;
            free = free.min(limits.free_memory);
        }
    }
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let (disk_free_mb, on_sd_card) =
        disk_of(&disks, data_dir).map_err(|why| format!("cannot read free space for {}: {why}", data_dir.display()))?;
    // Unknown storage stays `None`, and `assess` declines pictures with that reason.
    let picture_dir_on_sd = picture_dir.and_then(|p| disk_of(&disks, p).ok()).map(|(_, sd)| sd);
    let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    Ok(HostFacts {
        arch: normalise_arch(std::env::consts::ARCH),
        ram_mb: ram / (1024 * 1024),
        free_ram_mb: free / (1024 * 1024),
        disk_free_mb,
        on_sd_card,
        picture_dir_on_sd,
        bench_score,
        cores,
        load_1m: sysinfo::System::load_average().one,
        label: host_label(),
    })
}

/// SHA-256 over 64 MB; score is MB/s.
pub fn run_benchmark() -> u64 {
    use sha2::{Digest, Sha256};
    // On the heap: a 1 MB array on the stack overflows a 2 MB worker thread in debug builds.
    let block = vec![0xA5u8; 1024 * 1024];
    let mut hasher = Sha256::new();
    let started = std::time::Instant::now();
    for _ in 0..64 {
        hasher.update(&block);
    }
    let digest = hasher.finalize();
    std::hint::black_box(digest);
    let secs = started.elapsed().as_secs_f64().max(0.001);
    (64.0 / secs) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> HostFacts {
        HostFacts {
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
        }
    }

    fn off() -> PicturePolicy {
        PicturePolicy { enabled: false, picture_dir: None }
    }

    fn on() -> PicturePolicy {
        PicturePolicy { enabled: true, picture_dir: None }
    }

    #[test]
    fn vm_stat_free_inactive_and_purgeable_pages_count_as_available() {
        let text = "Mach Virtual Memory Statistics: (page size of 4096 bytes)\nPages free:                              118548.\nPages active:                            314173.\nPages inactive:                          291808.\nPages speculative:                        23105.\nPages purgeable:                           1451.\n";
        assert_eq!(parse_vm_stat_reclaimable(text), Some((118_548 + 291_808 + 1_451) * 4096));
        assert_eq!(parse_vm_stat_reclaimable("garbage"), None);
    }

    #[test]
    fn a_host_that_is_not_linux_never_reads_zero_free_memory_while_it_has_some() {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        if !cfg!(target_os = "linux") {
            assert!(available_bytes(&sys) >= sys.free_memory());
        }
    }

    #[test]
    fn floor_passes_a_good_host_and_arm64() {
        assert!(check_floor(&good()).is_ok());
        assert!(check_floor(&HostFacts { arch: "arm64".into(), ..good() }).is_ok());
    }

    #[test]
    fn floor_refuses_32_bit() {
        let e = check_floor(&HostFacts { arch: "armv7".into(), ..good() }).unwrap_err();
        assert!(e.contains("armv7"), "{e}");
        assert!(e.contains("amd64") && e.contains("arm64"), "{e}");
    }

    #[test]
    fn floor_refuses_under_1gb_ram() {
        let e = check_floor(&HostFacts { ram_mb: 512, ..good() }).unwrap_err();
        assert!(e.contains("512 MB") && e.contains("900 MB"), "{e}");
    }

    #[test]
    fn floor_refuses_under_2gb_disk() {
        let e = check_floor(&HostFacts { disk_free_mb: 1500, ..good() }).unwrap_err();
        assert!(e.contains("1500 MB") && e.contains("2048 MB"), "{e}");
    }

    #[test]
    fn busy_after_two_high_load_samples_not_one() {
        let mut h = LoadHistory::default();
        h.push(3.4, 4);
        assert_eq!(assess(&good(), &h, false, &off()).state, "available");
        h.push(3.4, 4);
        let a = assess(&good(), &h, false, &off());
        assert_eq!(a.state, "busy");
        assert_eq!(a.busy_reason.as_deref(), Some("load 3.4 on 4 cores"));
        h.push(0.2, 4);
        assert_eq!(assess(&good(), &h, false, &off()).state, "available");
    }

    #[test]
    fn busy_under_300mb_free() {
        let a = assess(&HostFacts { free_ram_mb: 280, ..good() }, &LoadHistory::default(), false, &off());
        assert_eq!(a.state, "busy");
        assert_eq!(a.busy_reason.as_deref(), Some("free memory 280 MB (needs 300 MB)"));
    }

    #[test]
    fn busy_when_paused_and_capabilities_stay_as_assessed() {
        let a = assess(&good(), &LoadHistory::default(), true, &off());
        assert_eq!(a.state, "busy");
        assert_eq!(a.busy_reason.as_deref(), Some("paused on the status page"));
        assert!(a.capabilities.ai_store && a.capabilities.guide_store);
    }

    #[test]
    fn stores_follow_the_floor_and_later_stages_stay_declined() {
        let a = assess(&good(), &LoadHistory::default(), false, &off());
        assert!(a.capabilities.ai_store && a.capabilities.guide_store);
        assert!(!a.capabilities.ai_runner);
        assert_eq!(a.declined["aiRunner"], "stage 2");
        let below = assess(&HostFacts { disk_free_mb: 100, ..good() }, &LoadHistory::default(), false, &off());
        assert!(!below.capabilities.ai_store && !below.capabilities.guide_store);
        assert!(below.declined.contains_key("aiStore") && below.declined.contains_key("guideStore"));
    }

    #[test]
    fn pictures_declined_on_sd_card_without_picture_dir() {
        let a = assess(&HostFacts { on_sd_card: true, ..good() }, &LoadHistory::default(), false, &on());
        assert!(!a.capabilities.picture_store);
        assert!(a.declined["pictureStore"].contains("SD card"));
        // a picture folder on other storage lifts the refusal
        let p = PicturePolicy { enabled: true, picture_dir: Some("/media/pics".into()) };
        let b = assess(&HostFacts { on_sd_card: true, picture_dir_on_sd: Some(false), ..good() }, &LoadHistory::default(), false, &p);
        assert!(b.capabilities.picture_store);
        // but not one that is itself on an SD card
        let c = assess(&HostFacts { on_sd_card: true, picture_dir_on_sd: Some(true), ..good() }, &LoadHistory::default(), false, &p);
        assert!(!c.capabilities.picture_store);
        assert!(c.declined["pictureStore"].contains("/media/pics"));
        // and one whose storage cannot be told is declined with that reason, not enabled
        let d = assess(&HostFacts { picture_dir_on_sd: None, ..good() }, &LoadHistory::default(), false, &p);
        assert!(!d.capabilities.picture_store);
        assert!(d.declined["pictureStore"].contains("cannot tell what storage /media/pics is on"));
    }

    #[test]
    fn pictures_declined_until_enabled() {
        let a = assess(&good(), &LoadHistory::default(), false, &off());
        assert!(!a.capabilities.picture_store);
        assert_eq!(a.declined["pictureStore"], "switched off in the Companion's options (pictures)");
        assert!(assess(&good(), &LoadHistory::default(), false, &on()).capabilities.picture_store);
    }

    #[test]
    fn budget_is_10pc_of_free_capped_at_20gb() {
        assert_eq!(default_budget_mb(10_000), 1_000);
        assert_eq!(default_budget_mb(402_000), 20_480);
        assert_eq!(default_budget_mb(0), 0);
    }

    #[test]
    fn card_offers_heavy_pass_with_the_memory() {
        let roomy = assess(&HostFacts { free_ram_mb: 2048, ..good() }, &LoadHistory::default(), false, &off());
        assert!(roomy.capabilities.heavy_pass);
        assert!(!roomy.declined.contains_key("heavyPass"));
        let tight = assess(&HostFacts { free_ram_mb: 800, ..good() }, &LoadHistory::default(), false, &off());
        assert!(!tight.capabilities.heavy_pass);
        let why = &tight.declined["heavyPass"];
        assert!(why.contains("1024 MB") && why.contains("800 MB"), "{why}");
    }
}
