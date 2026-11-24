//! The budgeted blob store behind the AI answers (now), guides and pictures (Task 7).
//!
//! One [`Budget`] is shared by every kind: a single byte limit, a single least-recently-read order
//! across all of them, and the host's 2 GB free-disk floor. Each kind keeps its blobs under
//! `<dir>/<first two hex of sha256(id)>/<sha256(id)>` and its bookkeeping in `<dir>/index.json`.
//!
//! Rules this file keeps:
//! * Serving a request never walks a directory. Sizes live in running counters (`used_bytes`).
//! * File work runs on tokio's file API or `spawn_blocking`, and never while a std lock is held.
//! * The index is rewritten atomically (temp file, fsync, rename), at most every few seconds for
//!   changes and once a minute for last-read stamps, and again at shutdown. A crash can therefore
//!   lose the last few seconds of the index: at the next start a file with no entry is deleted
//!   (printed) and an entry with no file is dropped (printed). It is a cache; it never lies.
//! * A refusal always carries the figures that caused it.

use crate::state::DiskUsage;
use companion_protocol::card::UsedMb;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub const MIB: u64 = 1024 * 1024;
/// The host must keep this much free after a write (the Companion's own start-up floor, 2 GB).
pub const FREE_DISK_FLOOR_BYTES: u64 = 2048 * MIB;
pub const INDEX_FILE: &str = "index.json";
pub const BUDGET_FILE: &str = "budget.json";
const TMP_DIR: &str = "tmp";
/// Last-read stamps alone are flushed no more often than this.
/// A streamed put looks at the host's free space again after each this many bytes.
pub const FREE_RECHECK_EVERY_BYTES: u64 = 8 * MIB;
pub const READ_FLUSH_EVERY_MS: i64 = 60_000;
/// R14: a picture put flushes the index no more often than this.
pub const PICTURE_FLUSH_MIN_MS: i64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Ai,
    Guide,
    Picture,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Ai, Kind::Guide, Kind::Picture];

    fn idx(self) -> usize {
        match self {
            Kind::Ai => 0,
            Kind::Guide => 1,
            Kind::Picture => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Ai => "ai",
            Kind::Guide => "guide",
            Kind::Picture => "picture",
        }
    }
}

/// What a store knows about a blob besides its bytes. `stored_at_ms` is the television's own
/// stamp for an AI answer: it is carried, never re-dated.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub stored_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// A guide's source status, or any short note a later store wants kept beside the bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// A guide's parsed-guide artefact, once written (`<body path>.parsed.gz`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parsed: Option<crate::guide_parse::ParsedSummary>,
    /// What became of the parse, in words; empty until a guide body has been stored.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parse_status: String,
}

impl Meta {
    pub fn at(stored_at_ms: i64) -> Meta {
        Meta { stored_at_ms, ..Meta::default() }
    }
}

/// The parsed-guide artefact that sits beside a blob.
pub fn artefact_path(blob: &Path) -> PathBuf {
    let mut name = blob.as_os_str().to_owned();
    name.push(".parsed.gz");
    PathBuf::from(name)
}

/// A put that was not done, and why, with the figures.
#[derive(Debug, PartialEq, Clone)]
pub struct StoreRefusal {
    pub reason: String,
}

fn refuse<T>(reason: impl Into<String>) -> Result<T, StoreRefusal> {
    Err(StoreRefusal { reason: reason.into() })
}

/// What happens when the id is already held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // `Replace` is what Task 7's guides and pictures use
pub enum Conflict {
    Replace,
    /// Keep whichever has the newer `stored_at_ms`; an older offer is refused, an equal one is a no-op.
    KeepNewer,
}

/// Free bytes on the disk the store lives on. Measured per write, on a blocking thread.
pub trait FreeDisk: Send + Sync {
    fn free_bytes(&self) -> Result<u64, String>;
}

pub struct HostFreeDisk(pub PathBuf);

impl FreeDisk for HostFreeDisk {
    fn free_bytes(&self) -> Result<u64, String> {
        crate::host::free_disk_bytes(&self.0)
    }
}

// ---- the shared budget ------------------------------------------------------------------------

struct Entry {
    size: u64,
    meta: Meta,
    last_read_ms: i64,
    /// Position in the least-recently-read order; smaller is older.
    order: u64,
    /// Bytes of the artefact beside this blob (a guide's parsed file); counted in `used` and
    /// evicted with the blob. Not persisted: measured from the file at start-up.
    extra: u64,
}

#[derive(Default, Clone, Copy)]
struct Dirty {
    structural: bool,
    reads: bool,
    last_flush_ms: i64,
}

#[derive(Default)]
struct State {
    maps: [HashMap<String, Entry>; 3],
    lru: BTreeMap<u64, (usize, String)>,
    next_order: u64,
    used: [u64; 3],
    dirs: [Option<PathBuf>; 3],
    dirty: [Dirty; 3],
    /// Bumped by `reset`, so a put that started before an unpair cannot land after it.
    epoch: u64,
}

pub struct Victim {
    pub kind: Kind,
    pub hash: String,
    pub size: u64,
    pub path: PathBuf,
}

pub struct Budget {
    limit_bytes: u64,
    floor_bytes: u64,
    free: Arc<dyn FreeDisk>,
    state: Mutex<State>,
    used_atomic: [AtomicU64; 3],
    flush_locks: [tokio::sync::Mutex<()>; 3],
    /// Serialises the last step of a put of one kind, and the deletion of that kind's evicted files.
    put_locks: [tokio::sync::Mutex<()>; 3],
}

fn sha_hex(id: &str) -> String {
    Sha256::digest(id.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn blob_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join(&hash[..2]).join(hash)
}

impl Budget {
    pub fn new(limit_bytes: u64, floor_bytes: u64, free: Arc<dyn FreeDisk>) -> Arc<Budget> {
        Arc::new(Budget {
            limit_bytes,
            floor_bytes,
            free,
            state: Mutex::new(State::default()),
            used_atomic: Default::default(),
            flush_locks: Default::default(),
            put_locks: Default::default(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[allow(dead_code)] // read by Task 7's stores
    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    pub fn limit_mb(&self) -> u64 {
        self.limit_bytes / MIB
    }

    pub fn used_bytes(&self, kind: Kind) -> u64 {
        self.used_atomic[kind.idx()].load(Ordering::Relaxed)
    }

    fn publish_counters(&self, st: &State) {
        for k in 0..3 {
            self.used_atomic[k].store(st.used[k], Ordering::Relaxed);
        }
    }

    fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Takes a kind's loaded entries, then evicts (oldest read first, any kind) until the whole
    /// budget holds. Returns what was evicted so the caller can delete the files.
    fn register(&self, kind: Kind, dir: &Path, loaded: Vec<(String, Entry)>) -> Vec<Victim> {
        let mut st = self.lock();
        st.dirs[kind.idx()] = Some(dir.to_path_buf());
        let mut loaded = loaded;
        loaded.sort_by_key(|(_, e)| e.last_read_ms);
        for (hash, mut e) in loaded {
            e.order = st.next_order;
            st.next_order += 1;
            st.used[kind.idx()] += e.size + e.extra;
            st.lru.insert(e.order, (kind.idx(), hash.clone()));
            st.maps[kind.idx()].insert(hash, e);
        }
        let victims = Self::evict_to_fit(&mut st, self.limit_bytes, 0);
        for v in &victims {
            st.dirty[v.kind.idx()].structural = true;
        }
        self.publish_counters(&st);
        victims
    }

    /// Removes oldest-read entries until `used + extra <= limit`.
    fn evict_to_fit(st: &mut State, limit: u64, extra: u64) -> Vec<Victim> {
        let mut victims = Vec::new();
        loop {
            let total: u64 = st.used.iter().sum();
            if total + extra <= limit {
                break;
            }
            let Some((&order, _)) = st.lru.iter().next() else { break };
            let Some((k, hash)) = st.lru.remove(&order) else { break };
            let Some(e) = st.maps[k].remove(&hash) else { continue };
            st.used[k] = st.used[k].saturating_sub(e.size + e.extra);
            let kind = Kind::ALL[k];
            let path = st.dirs[k].as_ref().map(|d| blob_path(d, &hash)).unwrap_or_default();
            victims.push(Victim { kind, hash, size: e.size, path });
        }
        victims
    }

    fn remove_entry(st: &mut State, k: usize, hash: &str) -> Option<Entry> {
        let e = st.maps[k].remove(hash)?;
        st.lru.remove(&e.order);
        st.used[k] = st.used[k].saturating_sub(e.size + e.extra);
        Some(e)
    }

    /// Takes out every entry of `kind` stored before `cutoff_ms`; the caller deletes the files.
    fn take_older_than(&self, kind: Kind, cutoff_ms: i64) -> Vec<Victim> {
        let mut st = self.lock();
        let k = kind.idx();
        let old: Vec<String> = st.maps[k].iter().filter(|(_, e)| e.meta.stored_at_ms < cutoff_ms).map(|(h, _)| h.clone()).collect();
        let mut victims = Vec::new();
        for hash in old {
            let Some(e) = Self::remove_entry(&mut st, k, &hash) else { continue };
            let path = st.dirs[k].as_ref().map(|d| blob_path(d, &hash)).unwrap_or_default();
            victims.push(Victim { kind, hash, size: e.size, path });
        }
        if !victims.is_empty() {
            st.dirty[k].structural = true;
            self.publish_counters(&st);
        }
        victims
    }

    /// An entry whose file has gone: forget it, say so.
    fn forget(&self, kind: Kind, hash: &str, why: &str) {
        let mut st = self.lock();
        if Self::remove_entry(&mut st, kind.idx(), hash).is_some() {
            st.dirty[kind.idx()].structural = true;
            self.publish_counters(&st);
            eprintln!("{} store: dropped entry {}: {why}", kind.name(), &hash[..12]);
        }
    }

    fn meta_of(&self, kind: Kind, hash: &str) -> Option<Meta> {
        self.lock().maps[kind.idx()].get(hash).map(|e| e.meta.clone())
    }

    /// Marks a read for the least-recently-read order. Memory only; the index follows within a minute.
    fn touch(&self, kind: Kind, hash: &str) -> Option<Meta> {
        let mut st = self.lock();
        let order = st.next_order;
        let k = kind.idx();
        let (meta, old_order) = {
            let e = st.maps[k].get_mut(hash)?;
            let old = e.order;
            e.order = order;
            e.last_read_ms = crate::state::now_ms();
            (e.meta.clone(), old)
        };
        st.next_order += 1;
        st.lru.remove(&old_order);
        st.lru.insert(order, (k, hash.to_string()));
        st.dirty[k].reads = true;
        Some(meta)
    }

    /// Puts the entry in and makes room for it. No file work here: returns what to delete.
    fn commit_put(&self, kind: Kind, hash: &str, size: u64, meta: Meta, epoch: u64) -> Result<Vec<Victim>, String> {
        let mut st = self.lock();
        if st.epoch != epoch {
            return Err("the household was unpaired while this was being stored".to_string());
        }
        let k = kind.idx();
        Self::remove_entry(&mut st, k, hash);
        let victims = Self::evict_to_fit(&mut st, self.limit_bytes, size);
        let order = st.next_order;
        st.next_order += 1;
        st.used[k] += size;
        st.lru.insert(order, (k, hash.to_string()));
        st.maps[k].insert(hash.to_string(), Entry { size, meta, last_read_ms: crate::state::now_ms(), order, extra: 0 });
        st.dirty[k].structural = true;
        for v in &victims {
            st.dirty[v.kind.idx()].structural = true;
        }
        self.publish_counters(&st);
        Ok(victims)
    }

    /// Evicts, before a write, what an item of known `size` will need, so the peak on disk is
    /// never used + new. Returns the victims for `delete_victims`.
    fn make_room(&self, kind: Kind, hash: &str, size: u64, epoch: u64) -> Result<Vec<Victim>, String> {
        let mut st = self.lock();
        if st.epoch != epoch {
            return Err("the household was unpaired while this was being stored".to_string());
        }
        let existing = st.maps[kind.idx()].get(hash).map(|e| e.size).unwrap_or(0);
        let victims = Self::evict_to_fit(&mut st, self.limit_bytes, size.saturating_sub(existing));
        for v in &victims {
            st.dirty[v.kind.idx()].structural = true;
        }
        self.publish_counters(&st);
        Ok(victims)
    }

    /// Deletes evicted files. Each is unlinked under its kind's put lock and only if its id has
    /// not been stored again meanwhile, so a newer file renamed into the same path is never
    /// deleted by an older eviction.
    pub async fn delete_victims(&self, victims: Vec<Victim>) {
        for v in victims {
            let _g = self.put_locks[v.kind.idx()].lock().await;
            if self.lock().maps[v.kind.idx()].contains_key(&v.hash) {
                continue; // stored again: the file at this path is the new one
            }
            let _ = tokio::fs::remove_file(artefact_path(&v.path)).await;
            match tokio::fs::remove_file(&v.path).await {
                Ok(()) => eprintln!("{} store: evicted {} ({} bytes) to make room", v.kind.name(), &v.hash[..12], v.size),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("{} store: cannot delete evicted {}: {e}", v.kind.name(), v.path.display()),
            }
        }
    }

    /// Everything forgotten and the folders emptied: what unpair means for the stores.
    pub async fn reset(&self) {
        let dirs = {
            let mut st = self.lock();
            st.epoch += 1;
            st.maps = Default::default();
            st.lru.clear();
            st.used = [0; 3];
            for d in st.dirty.iter_mut() {
                d.structural = false;
                d.reads = false;
            }
            self.publish_counters(&st);
            st.dirs.clone()
        };
        for dir in dirs.into_iter().flatten() {
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("store reset: cannot delete {}: {e}", dir.display()),
            }
        }
    }

    pub(crate) async fn check_floor(&self, size: u64) -> Result<(), StoreRefusal> {
        let free = self.free.clone();
        let measured = tokio::task::spawn_blocking(move || free.free_bytes()).await.unwrap_or_else(|e| Err(format!("the free-space check failed: {e}")));
        match measured {
            Err(why) => refuse(format!("not stored: cannot tell how much disk is free ({why})")),
            Ok(free) if free < self.floor_bytes.saturating_add(size) => refuse(format!(
                "not stored: the host has {} MB free and the Companion keeps {} MB (2 GB) clear; this item is {} bytes",
                free / MIB,
                self.floor_bytes / MIB,
                size
            )),
            Ok(_) => Ok(()),
        }
    }

    // ---- index files -------------------------------------------------------------------------

    async fn flush_kind(&self, kind: Kind) -> Result<(), String> {
        let _guard = self.flush_locks[kind.idx()].lock().await;
        let k = kind.idx();
        let (dir, entries) = {
            let mut st = self.lock();
            let Some(dir) = st.dirs[k].clone() else { return Ok(()) };
            let entries: Vec<IndexEntry> = st.maps[k]
                .iter()
                .map(|(hash, e)| IndexEntry { hash: hash.clone(), size: e.size, last_read_ms: e.last_read_ms, meta: e.meta.clone() })
                .collect();
            st.dirty[k].structural = false;
            st.dirty[k].reads = false;
            st.dirty[k].last_flush_ms = crate::state::now_ms();
            (dir, entries)
        };
        let written = tokio::task::spawn_blocking(move || write_index(&dir, &entries)).await.unwrap_or_else(|e| Err(format!("index write task failed: {e}")));
        if let Err(why) = &written {
            self.lock().dirty[k].structural = true;
            eprintln!("{} store: cannot write its index, will retry: {why}", kind.name());
        }
        written
    }

    /// False when this kind's index was written less than `PICTURE_FLUSH_MIN_MS` ago.
    fn flush_due(&self, kind: Kind, now_ms: i64) -> bool {
        now_ms.saturating_sub(self.lock().dirty[kind.idx()].last_flush_ms) >= PICTURE_FLUSH_MIN_MS
    }

    /// Replaces what is kept beside a held blob (a guide's status or fetched time) without
    /// touching its bytes. False when it is not held. The index follows on the next tick.
    fn set_meta(&self, kind: Kind, hash: &str, meta: Meta) -> bool {
        let mut st = self.lock();
        let k = kind.idx();
        let Some(e) = st.maps[k].get_mut(hash) else { return false };
        e.meta = meta;
        st.dirty[k].structural = true;
        true
    }

    fn edit_meta(&self, kind: Kind, hash: &str, edit: impl FnOnce(&mut Meta)) -> bool {
        let mut st = self.lock();
        let k = kind.idx();
        let Some(e) = st.maps[k].get_mut(hash) else { return false };
        edit(&mut e.meta);
        st.dirty[k].structural = true;
        true
    }

    fn set_extra(&self, kind: Kind, hash: &str, bytes: u64) -> bool {
        let mut st = self.lock();
        let k = kind.idx();
        let Some(e) = st.maps[k].get_mut(hash) else { return false };
        let old = std::mem::replace(&mut e.extra, bytes);
        st.used[k] = st.used[k].saturating_sub(old) + bytes;
        self.publish_counters(&st);
        true
    }

    /// Called every few seconds: flushes a kind whose entries changed, or whose last-read stamps
    /// are a minute old.
    pub async fn maintain(&self, now_ms: i64) {
        for kind in Kind::ALL {
            let due = {
                let st = self.lock();
                let d = st.dirty[kind.idx()];
                d.structural || (d.reads && now_ms.saturating_sub(d.last_flush_ms) >= READ_FLUSH_EVERY_MS)
            };
            if due {
                let _ = self.flush_kind(kind).await;
            }
        }
    }

    /// Shutdown: every kind with anything unflushed is written.
    pub async fn flush_all(&self) {
        for kind in Kind::ALL {
            let due = {
                let st = self.lock();
                let d = st.dirty[kind.idx()];
                d.structural || d.reads
            };
            if due {
                let _ = self.flush_kind(kind).await;
            }
        }
    }
}

impl DiskUsage for Arc<Budget> {
    fn used_mb(&self) -> UsedMb {
        let mb = |k: Kind| self.used_bytes(k).div_ceil(MIB);
        UsedMb { ai: mb(Kind::Ai), guide: mb(Kind::Guide), picture: mb(Kind::Picture), dvr: 0, stage: 0 }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    hash: String,
    size: u64,
    last_read_ms: i64,
    meta: Meta,
}

#[derive(Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    entries: Vec<IndexEntry>,
}

fn sync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(dir).and_then(|d| d.sync_all()).map_err(|e| format!("cannot sync {}: {e}", dir.display()))?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Writes `bytes` to `path` so a crash leaves the old file or the new one, never half of either.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    let dir = path.parent().ok_or_else(|| format!("{} has no folder", path.display()))?;
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
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
    sync_dir(dir)
}

fn write_index(dir: &Path, entries: &[IndexEntry]) -> Result<(), String> {
    let body = serde_json::to_vec(&serde_json::json!({ "version": 1, "entries": entries })).map_err(|e| format!("cannot encode the index: {e}"))?;
    write_atomic(&dir.join(INDEX_FILE), &body)
}

// ---- one kind ---------------------------------------------------------------------------------

pub struct BlobStore {
    kind: Kind,
    dir: PathBuf,
    budget: Arc<Budget>,
}

#[allow(dead_code)] // put, put_bytes, used_bytes, meta: Task 7 and tests
impl BlobStore {
    /// Loads `<dir>/index.json`, drops entries whose file is gone and deletes files no entry
    /// names (each printed), then evicts if the budget has shrunk. Start-up work: blocking.
    pub fn open(dir: &Path, kind: Kind, budget: Arc<Budget>) -> Result<BlobStore, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create the {} store folder {}: {e}", kind.name(), dir.display()))?;
        let name = kind.name();
        let index_path = dir.join(INDEX_FILE);
        let listed: Vec<IndexEntry> = match std::fs::read(&index_path) {
            Ok(bytes) => match serde_json::from_slice::<IndexFile>(&bytes) {
                Ok(f) => f.entries,
                Err(e) => {
                    eprintln!("{name} store: {} is unreadable ({e}); starting it empty and clearing its files", index_path.display());
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", index_path.display())),
        };
        let mut loaded: Vec<(String, Entry)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for e in listed {
            if !is_hash(&e.hash) || !seen.insert(e.hash.clone()) {
                eprintln!("{name} store: dropped an index entry with a bad or repeated name");
                continue;
            }
            match std::fs::metadata(blob_path(dir, &e.hash)) {
                Ok(m) if m.is_file() => {
                    let extra = std::fs::metadata(artefact_path(&blob_path(dir, &e.hash))).map(|a| a.len()).unwrap_or(0);
                    if m.len() != e.size {
                        eprintln!("{name} store: entry {} says {} bytes but the file holds {}; using the file", &e.hash[..12], e.size, m.len());
                    }
                    loaded.push((e.hash, Entry { size: m.len(), meta: e.meta, last_read_ms: e.last_read_ms, order: 0, extra }));
                }
                _ => eprintln!("{name} store: dropped entry {}: its file is missing", &e.hash[..12]),
            }
        }
        // Files nothing names. Only our own shape is touched: <2 hex>/<64 hex>, and tmp/.
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                for shard in rd.flatten() {
                    let sname = shard.file_name().to_string_lossy().to_string();
                    let path = shard.path();
                    if sname == TMP_DIR && path.is_dir() {
                        let _ = std::fs::remove_dir_all(&path);
                    } else if sname.len() == 2 && sname.bytes().all(|b| b.is_ascii_hexdigit()) && path.is_dir() {
                        if let Ok(files) = std::fs::read_dir(&path) {
                            for f in files.flatten() {
                                let fname = f.file_name().to_string_lossy().to_string();
                                let owner = fname.strip_suffix(".parsed.gz").or_else(|| fname.strip_suffix(".parsed.gz.tmp")).unwrap_or(&fname);
                                if is_hash(owner) && !seen.contains(owner) {
                                    match std::fs::remove_file(f.path()) {
                                        Ok(()) => eprintln!("{name} store: deleted {} (no index entry names it)", &fname[..12]),
                                        Err(e) => eprintln!("{name} store: cannot delete the unnamed file {}: {e}", f.path().display()),
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => return Err(format!("cannot list {}: {e}", dir.display())),
        }
        let victims = budget.register(kind, dir, loaded);
        for v in victims {
            let _ = std::fs::remove_file(artefact_path(&v.path));
            match std::fs::remove_file(&v.path) {
                Ok(()) => eprintln!("{} store: evicted {} ({} bytes) at start: over the budget", v.kind.name(), &v.hash[..12], v.size),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("{} store: cannot delete evicted {}: {e}", v.kind.name(), v.path.display()),
            }
        }
        Ok(BlobStore { kind, dir: dir.to_path_buf(), budget })
    }

    #[allow(dead_code)] // read by Task 7's stores
    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn used_bytes(&self) -> u64 {
        self.budget.used_bytes(self.kind)
    }

    /// Stores `bytes` under `id`, replacing what was there.
    pub async fn put_bytes(&self, id: &str, bytes: &[u8], meta: Meta) -> Result<(), StoreRefusal> {
        self.put_inner(id, bytes, Some(bytes.len() as u64), meta, Conflict::Replace).await
    }

    pub async fn put_bytes_with(&self, id: &str, bytes: &[u8], meta: Meta, conflict: Conflict) -> Result<(), StoreRefusal> {
        self.put_inner(id, bytes, Some(bytes.len() as u64), meta, conflict).await
    }

    /// Stores a stream of any size. It is written to a temporary file as it arrives and refused
    /// the moment it passes the whole budget.
    pub async fn put<R: AsyncRead + Unpin>(&self, id: &str, reader: R, meta: Meta) -> Result<(), StoreRefusal> {
        self.put_inner(id, reader, None, meta, Conflict::Replace).await
    }

    /// A stream whose size the caller knows (a Content-Length): the floor and the budget are
    /// judged up front and room is made before a byte is written.
    #[allow(dead_code)] // Task 7's guides and pictures
    pub async fn put_sized<R: AsyncRead + Unpin>(&self, id: &str, reader: R, size_hint: Option<u64>, meta: Meta) -> Result<(), StoreRefusal> {
        self.put_inner(id, reader, size_hint, meta, Conflict::Replace).await
    }

    async fn put_inner<R: AsyncRead + Unpin>(&self, id: &str, mut reader: R, hint: Option<u64>, meta: Meta, conflict: Conflict) -> Result<(), StoreRefusal> {
        let size_hint = hint.unwrap_or(0);
        if id.is_empty() {
            return refuse("not stored: the id is empty");
        }
        let budget = &self.budget;
        let hash = sha_hex(id);
        let epoch = budget.epoch();
        if size_hint > budget.limit_bytes {
            return refuse(format!(
                "not stored: this item is {size_hint} bytes and the whole disk budget is {} bytes ({} MB)",
                budget.limit_bytes,
                budget.limit_mb()
            ));
        }
        if let Some(refusal) = self.newer_held(&hash, &meta, conflict) {
            return refusal;
        }
        budget.check_floor(size_hint).await?;
        if hint.is_some() {
            // Known size: make room first, so the peak on disk is not used + new.
            match budget.make_room(self.kind, &hash, size_hint, epoch) {
                Ok(victims) => budget.delete_victims(victims).await,
                Err(why) => return refuse(format!("not stored: {why}")),
            }
        }

        let tmp_dir = self.dir.join(TMP_DIR);
        if let Err(e) = tokio::fs::create_dir_all(&tmp_dir).await {
            return refuse(format!("not stored: cannot create {}: {e}", tmp_dir.display()));
        }
        let tmp = tmp_dir.join(format!("{}-{:016x}", &hash[..16], rand::random::<u64>()));
        let size = match self.write_tmp(&tmp, &mut reader).await {
            Ok(n) => n,
            Err(r) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                eprintln!("{} store: write of {} aborted: {}", self.kind.name(), &hash[..12], r.reason);
                return Err(r);
            }
        };

        let guard = budget.put_locks[self.kind.idx()].lock().await;
        let outcome = self.land(&tmp, &hash, size, meta, conflict, epoch).await;
        drop(guard);
        let _ = tokio::fs::remove_file(&tmp).await; // a no-op once renamed
        let victims = outcome?;
        budget.delete_victims(victims).await;
        // R13: a guide or picture is expensive to fetch again, so its index is on disk before
        // the put returns and a crash cannot make the next start delete it. AI answers wait for
        // the 5 s flush.
        // R14: a picture index is flushed at most once a second (a screen of pictures is dozens of
        // puts); the 5 s maintenance tick writes what this skipped.
        if self.kind == Kind::Guide || (self.kind == Kind::Picture && budget.flush_due(self.kind, crate::state::now_ms())) {
            if let Err(why) = budget.flush_kind(self.kind).await {
                return refuse(format!("stored, but the {} index could not be written ({why}); it will be retried", self.kind.name()));
            }
        }
        Ok(())
    }

    /// `Some(refusal-or-ok)` when `KeepNewer` says to stop before writing anything.
    fn newer_held(&self, hash: &str, meta: &Meta, conflict: Conflict) -> Option<Result<(), StoreRefusal>> {
        if conflict != Conflict::KeepNewer {
            return None;
        }
        let held = self.budget.meta_of(self.kind, hash)?;
        if held.stored_at_ms > meta.stored_at_ms {
            return Some(refuse(format!(
                "kept the newer answer already held (stored at {} ms; this offer is from {} ms)",
                held.stored_at_ms, meta.stored_at_ms
            )));
        }
        if held.stored_at_ms == meta.stored_at_ms {
            return Some(Ok(()));
        }
        None
    }

    async fn write_tmp<R: AsyncRead + Unpin>(&self, tmp: &Path, reader: &mut R) -> Result<u64, StoreRefusal> {
        let io = |what: &str, e: std::io::Error| StoreRefusal { reason: format!("not stored: {what} {}: {e}", tmp.display()) };
        let mut file = tokio::fs::File::create(tmp).await.map_err(|e| io("cannot create", e))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut size: u64 = 0;
        let mut since_check: u64 = 0;
        loop {
            let n = reader.read(&mut buf).await.map_err(|e| StoreRefusal { reason: format!("not stored: the incoming data stopped after {size} bytes: {e}") })?;
            if n == 0 {
                break;
            }
            size += n as u64;
            if size > self.budget.limit_bytes {
                return refuse(format!("not stored: this item passed {size} bytes and the whole disk budget is {} bytes ({} MB)", self.budget.limit_bytes, self.budget.limit_mb()));
            }
            file.write_all(&buf[..n]).await.map_err(|e| io("cannot write", e))?;
            since_check += n as u64;
            if since_check >= FREE_RECHECK_EVERY_BYTES {
                since_check = 0;
                // The bytes written so far are already gone from the free figure.
                self.budget.check_floor(0).await.map_err(|r| StoreRefusal { reason: format!("{} (stopped after {size} bytes)", r.reason) })?;
            }
        }
        file.sync_data().await.map_err(|e| io("cannot sync", e))?;
        Ok(size)
    }

    /// The last step, under the put lock: conflict check again, move the file into place, commit.
    async fn land(&self, tmp: &Path, hash: &str, size: u64, meta: Meta, conflict: Conflict, epoch: u64) -> Result<Vec<Victim>, StoreRefusal> {
        if let Some(r) = self.newer_held(hash, &meta, conflict) {
            return r.map(|()| Vec::new());
        }
        if self.budget.epoch() != epoch {
            return refuse("not stored: the household was unpaired while this was being stored");
        }
        let final_path = blob_path(&self.dir, hash);
        if let Some(parent) = final_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return refuse(format!("not stored: cannot create {}: {e}", parent.display()));
            }
        }
        // A new body makes the parse of the old one wrong: it goes before the new one is in place.
        let _ = tokio::fs::remove_file(artefact_path(&final_path)).await;
        if let Err(e) = tokio::fs::rename(tmp, &final_path).await {
            return refuse(format!("not stored: cannot move the file into place at {}: {e}", final_path.display()));
        }
        match self.budget.commit_put(self.kind, hash, size, meta, epoch) {
            Ok(v) => Ok(v),
            Err(why) => {
                let _ = tokio::fs::remove_file(&final_path).await;
                refuse(format!("not stored: {why}"))
            }
        }
    }

    /// The bytes and what was kept with them. Marks the read for the eviction order. A file that
    /// has vanished is dropped from the index and reads as a miss, with a printed line.
    pub async fn get(&self, id: &str) -> Option<(Meta, tokio::fs::File)> {
        let hash = sha_hex(id);
        let meta = self.budget.touch(self.kind, &hash)?;
        match tokio::fs::File::open(blob_path(&self.dir, &hash)).await {
            Ok(f) => Some((meta, f)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.budget.forget(self.kind, &hash, "its file is missing");
                None
            }
            Err(e) => {
                // Not "gone": a permission or I/O fault. Keep the entry and the counters.
                eprintln!("{} store: cannot open {}: {e}; entry kept", self.kind.name(), &hash[..12]);
                None
            }
        }
    }

    /// Where `id`'s blob lives (whether or not it is held).
    pub fn path_of(&self, id: &str) -> PathBuf {
        blob_path(&self.dir, &sha_hex(id))
    }

    /// Changes what is kept beside a held `id` in one step. False when it is not held.
    pub fn edit_meta(&self, id: &str, edit: impl FnOnce(&mut Meta)) -> bool {
        self.budget.edit_meta(self.kind, &sha_hex(id), edit)
    }

    /// Counts the artefact file beside `id`'s blob (as it is on disk now, 0 if absent) in this
    /// kind's use. False when `id` is not held.
    pub fn count_artefact(&self, id: &str) -> bool {
        let hash = sha_hex(id);
        let bytes = std::fs::metadata(artefact_path(&blob_path(&self.dir, &hash))).map(|m| m.len()).unwrap_or(0);
        self.budget.set_extra(self.kind, &hash, bytes)
    }

    /// Deletes the artefact beside `id`'s blob and stops counting it.
    pub async fn drop_artefact(&self, id: &str) {
        let hash = sha_hex(id);
        let _ = tokio::fs::remove_file(artefact_path(&blob_path(&self.dir, &hash))).await;
        self.budget.set_extra(self.kind, &hash, 0);
    }

    /// Rewrites what is kept beside a held `id`. False when it is not held.
    pub fn update_meta(&self, id: &str, meta: Meta) -> bool {
        self.budget.set_meta(self.kind, &sha_hex(id), meta)
    }

    /// What is kept beside `id`, without counting as a read.
    pub fn meta(&self, id: &str) -> Option<Meta> {
        self.budget.meta_of(self.kind, &sha_hex(id))
    }

    /// Deletes every entry stored before `cutoff_ms`, files included. Returns how many went.
    pub async fn expire_older_than(&self, cutoff_ms: i64) -> usize {
        let victims = self.budget.take_older_than(self.kind, cutoff_ms);
        let n = victims.len();
        self.budget.delete_victims(victims).await;
        n
    }

    /// Drops an entry whose bytes turned out to be unusable (printed by the caller's reason).
    pub fn forget(&self, id: &str, why: &str) {
        self.budget.forget(self.kind, &sha_hex(id), why);
    }
}

// ---- all the stores, and the budget -----------------------------------------------------------

#[allow(dead_code)] // `guide` and `picture` are used from Task 7 on
pub struct Stores {
    pub budget: Arc<Budget>,
    pub ai: BlobStore,
    pub guide: BlobStore,
    /// `None` while the picture store is off (the default).
    pub picture: Option<BlobStore>,
}

impl Stores {
    pub fn open(config: &crate::config::Config, disk_free_mb: u64) -> Result<Stores, String> {
        let free: Arc<dyn FreeDisk> = Arc::new(HostFreeDisk(config.data_dir.clone()));
        Stores::open_with(config, disk_free_mb, free)
    }

    pub fn open_with(config: &crate::config::Config, disk_free_mb: u64, free: Arc<dyn FreeDisk>) -> Result<Stores, String> {
        let budget_mb = resolve_budget_mb(&config.data_dir, config.disk_budget_mb, disk_free_mb);
        let budget = Budget::new(budget_mb * MIB, FREE_DISK_FLOOR_BYTES, free);
        let ai = BlobStore::open(&config.data_dir.join(Kind::Ai.name()), Kind::Ai, budget.clone())?;
        let guide = BlobStore::open(&config.data_dir.join(Kind::Guide.name()), Kind::Guide, budget.clone())?;
        let picture = if config.picture_store_enabled {
            let dir = picture_root(config.picture_dir.as_deref(), &config.data_dir);
            Some(BlobStore::open(&dir, Kind::Picture, budget.clone())?)
        } else {
            None
        };
        Ok(Stores { budget, ai, guide, picture })
    }

    /// Unpair: forget everything held and empty the folders.
    pub async fn reset(&self) {
        self.budget.reset().await;
    }
}

/// The folder the picture store owns. An operator-chosen `picture_dir` is theirs, with whatever else
/// they keep in it, so the Companion works in a subfolder of its own and unpair deletes only that.
pub const PICTURE_SUBDIR: &str = "aurelis-companion";

pub fn picture_root(picture_dir: Option<&Path>, data_dir: &Path) -> PathBuf {
    match picture_dir {
        Some(d) => d.join(PICTURE_SUBDIR),
        None => data_dir.join(Kind::Picture.name()),
    }
}

/// The disk budget in MB (ruling R12). An explicit non-zero setting wins and is never saved.
/// Otherwise it is 10% of the free space (at most 20 GB) worked out ONCE, at first start, and kept
/// in `<data>/budget.json`; later starts reuse it, so a filling disk does not shrink it.
pub fn resolve_budget_mb(data_dir: &Path, explicit_mb: Option<u64>, disk_free_mb: u64) -> u64 {
    if let Some(mb) = explicit_mb.filter(|mb| *mb > 0) {
        return mb;
    }
    let path = data_dir.join(BUDGET_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("budgetMb").and_then(|b| b.as_u64())) {
            Some(mb) if mb > 0 => return mb,
            _ => eprintln!("{} is not a budget file; working the budget out again", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("cannot read {}: {e}; working the budget out again", path.display()),
    }
    let mb = crate::host::default_budget_mb(disk_free_mb);
    let body = serde_json::json!({ "budgetMb": mb, "computedAtMs": crate::state::now_ms(), "fromFreeMb": disk_free_mb }).to_string();
    match write_atomic(&path, body.as_bytes()) {
        Ok(()) => println!("disk budget set to {mb} MB (10% of {disk_free_mb} MB free, capped at 20 GB); kept in {}", path.display()),
        Err(e) => eprintln!("disk budget is {mb} MB this run but could not be saved: {e}"),
    }
    mb
}

#[cfg(test)]
pub struct FixedFree(pub AtomicU64);

#[cfg(test)]
impl FreeDisk for FixedFree {
    fn free_bytes(&self) -> Result<u64, String> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::household::TempDir;
    use tokio::io::AsyncReadExt;

    const GB: u64 = 1024 * MIB;

    struct Rig {
        dir: TempDir,
        free: Arc<FixedFree>,
        budget: Arc<Budget>,
        ai: BlobStore,
        guide: BlobStore,
    }

    fn rig(limit: u64) -> Rig {
        let dir = TempDir::new();
        let free = Arc::new(FixedFree(AtomicU64::new(500 * GB)));
        let budget = Budget::new(limit, FREE_DISK_FLOOR_BYTES, free.clone());
        let ai = BlobStore::open(&dir.0.join("ai"), Kind::Ai, budget.clone()).unwrap();
        let guide = BlobStore::open(&dir.0.join("guide"), Kind::Guide, budget.clone()).unwrap();
        Rig { dir, free, budget, ai, guide }
    }

    async fn read_all(mut f: tokio::fs::File) -> Vec<u8> {
        let mut v = Vec::new();
        f.read_to_end(&mut v).await.unwrap();
        v
    }

    #[tokio::test]
    async fn picture_index_flush_is_debounced_to_once_a_second_but_guides_flush_every_put() {
        let r = rig(MIB);
        let pic = BlobStore::open(&r.dir.0.join("picture"), Kind::Picture, r.budget.clone()).unwrap();
        let listed = |dir: &str| -> usize {
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(r.dir.0.join(dir).join(INDEX_FILE)).unwrap()).unwrap();
            v["entries"].as_array().unwrap().len()
        };
        pic.put_bytes("p1", b"1", Meta::at(1)).await.unwrap();
        pic.put_bytes("p2", b"2", Meta::at(1)).await.unwrap();
        assert_eq!(listed("picture"), 1, "the second put came inside a second of the first flush");
        r.budget.maintain(crate::state::now_ms()).await;
        assert_eq!(listed("picture"), 2, "the maintenance tick writes what the put skipped");
        r.guide.put_bytes("g1", b"1", Meta::at(1)).await.unwrap();
        r.guide.put_bytes("g2", b"2", Meta::at(1)).await.unwrap();
        assert_eq!(listed("guide"), 2, "guides flush on every put");
    }

    #[tokio::test]
    async fn put_then_get() {
        let r = rig(MIB);
        r.ai.put_bytes("k1", b"hello", Meta::at(77)).await.unwrap();
        let (meta, file) = r.ai.get("k1").await.expect("held");
        assert_eq!(meta.stored_at_ms, 77);
        assert_eq!(read_all(file).await, b"hello");
        assert_eq!(r.ai.used_bytes(), 5);
        assert!(r.ai.get("nope").await.is_none());
        // Same id again replaces, not adds.
        r.ai.put_bytes("k1", b"hello!!", Meta::at(78)).await.unwrap();
        assert_eq!(r.ai.used_bytes(), 7);
        // Laid out as <2 hex>/<sha256>, with nothing left in tmp/.
        let hash = sha_hex("k1");
        assert!(r.dir.0.join("ai").join(&hash[..2]).join(&hash).is_file());
        assert_eq!(std::fs::read_dir(r.dir.0.join("ai").join("tmp")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn streams_arrive_through_the_same_door() {
        let r = rig(MIB);
        let data = vec![7u8; 200_000];
        r.guide.put("g", &data[..], Meta::at(1)).await.unwrap();
        assert_eq!(r.guide.used_bytes(), 200_000);
        let (_, f) = r.guide.get("g").await.unwrap();
        assert_eq!(read_all(f).await, data);
        // A stream past the whole budget is refused as it passes it, and leaves nothing behind.
        let big = vec![0u8; MIB as usize + 5];
        let e = r.guide.put("huge", &big[..], Meta::at(1)).await.unwrap_err();
        assert!(e.reason.contains("whole disk budget"), "{}", e.reason);
        assert_eq!(std::fs::read_dir(r.dir.0.join("guide").join("tmp")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn evicts_least_recently_read_when_over_budget() {
        let r = rig(300);
        for id in ["a", "b", "c"] {
            r.ai.put_bytes(id, &[1u8; 100], Meta::at(1)).await.unwrap();
        }
        assert!(r.ai.get("a").await.is_some(), "reading a makes b the oldest");
        r.ai.put_bytes("d", &[1u8; 100], Meta::at(1)).await.unwrap();
        assert!(r.ai.get("b").await.is_none(), "b was the least recently read");
        for id in ["a", "c", "d"] {
            assert!(r.ai.get(id).await.is_some(), "{id}");
        }
        assert_eq!(r.ai.used_bytes(), 300);
        let hb = sha_hex("b");
        assert!(!r.dir.0.join("ai").join(&hb[..2]).join(&hb).exists(), "the evicted file is deleted");
    }

    #[tokio::test]
    async fn eviction_is_across_kinds() {
        let r = rig(300);
        r.guide.put_bytes("old-guide", &[1u8; 200], Meta::at(1)).await.unwrap();
        r.ai.put_bytes("x", &[1u8; 100], Meta::at(1)).await.unwrap();
        r.ai.put_bytes("y", &[1u8; 100], Meta::at(1)).await.unwrap();
        assert!(r.guide.get("old-guide").await.is_none(), "the oldest read goes, whatever its kind");
        assert_eq!((r.guide.used_bytes(), r.ai.used_bytes()), (0, 200));
    }

    #[tokio::test]
    async fn item_bigger_than_the_whole_budget_is_refused_with_figures() {
        let r = rig(100);
        let e = r.ai.put_bytes("k", &[0u8; 101], Meta::at(1)).await.unwrap_err();
        assert!(e.reason.contains("101 bytes") && e.reason.contains("100 bytes"), "{}", e.reason);
        assert_eq!(r.ai.used_bytes(), 0);
    }

    #[tokio::test]
    async fn refuses_with_reason_when_disk_free_below_2gb_floor_and_still_serves_reads() {
        let r = rig(MIB);
        r.ai.put_bytes("held", b"before", Meta::at(5)).await.unwrap();
        r.free.0.store(2 * GB + 100, Ordering::SeqCst);
        // 2 GB + 100 free and a 200-byte item would leave less than the floor.
        let e = r.ai.put_bytes("new", &[0u8; 200], Meta::at(6)).await.unwrap_err();
        assert!(e.reason.contains("2048 MB") && e.reason.contains("2 GB") && e.reason.contains("200 bytes"), "{}", e.reason);
        assert!(r.ai.get("new").await.is_none());
        // Reads carry on.
        let (_, f) = r.ai.get("held").await.expect("still served");
        assert_eq!(read_all(f).await, b"before");
        // An item that fits above the floor is still taken.
        r.ai.put_bytes("small", &[0u8; 50], Meta::at(7)).await.unwrap();
        // A stream is judged after it has landed, on what is then free.
        r.free.0.store(2 * GB - 1, Ordering::SeqCst);
        let e = r.guide.put("s", &[0u8; 10][..], Meta::at(1)).await.unwrap_err();
        assert!(e.reason.contains("2048 MB") || e.reason.contains("2047 MB"), "{}", e.reason);
        assert!(r.guide.get("s").await.is_none());
    }

    #[tokio::test]
    async fn a_put_that_only_needs_eviction_succeeds_even_on_a_tight_disk() {
        let r = rig(100);
        r.ai.put_bytes("a", &[0u8; 100], Meta::at(1)).await.unwrap();
        r.free.0.store(2 * GB + 1000, Ordering::SeqCst);
        r.ai.put_bytes("b", &[0u8; 100], Meta::at(2)).await.unwrap();
        assert!(r.ai.get("a").await.is_none());
        assert!(r.ai.get("b").await.is_some());
    }

    #[tokio::test]
    async fn reopen_rebuilds_counters_drops_missing_files_and_deletes_unnamed_ones() {
        let r = rig(MIB);
        r.ai.put_bytes("keep", &[1u8; 40], Meta::at(11)).await.unwrap();
        r.ai.put_bytes("lost", &[1u8; 60], Meta::at(12)).await.unwrap();
        r.guide.put_bytes("g", &[1u8; 5], Meta::at(13)).await.unwrap();
        r.budget.flush_all().await;
        // The file of "lost" goes missing, and a stray blob with no entry appears.
        let hl = sha_hex("lost");
        std::fs::remove_file(r.dir.0.join("ai").join(&hl[..2]).join(&hl)).unwrap();
        let stray = sha_hex("stray");
        std::fs::create_dir_all(r.dir.0.join("ai").join(&stray[..2])).unwrap();
        let stray_path = r.dir.0.join("ai").join(&stray[..2]).join(&stray);
        std::fs::write(&stray_path, b"orphan").unwrap();
        let foreign = r.dir.0.join("ai").join("notes.txt");
        std::fs::write(&foreign, b"not ours").unwrap();

        let budget = Budget::new(MIB, FREE_DISK_FLOOR_BYTES, r.free.clone());
        let ai = BlobStore::open(&r.dir.0.join("ai"), Kind::Ai, budget.clone()).unwrap();
        let guide = BlobStore::open(&r.dir.0.join("guide"), Kind::Guide, budget.clone()).unwrap();
        assert_eq!((ai.used_bytes(), guide.used_bytes()), (40, 5));
        assert_eq!(ai.meta("keep").map(|m| m.stored_at_ms), Some(11), "meta survives a restart");
        assert!(ai.get("lost").await.is_none());
        assert!(!stray_path.exists(), "an unnamed blob is deleted");
        assert!(foreign.exists(), "a file that is not ours is left alone");
    }

    #[tokio::test]
    async fn index_is_written_atomically_and_reads_flush_at_most_once_a_minute() {
        let r = rig(MIB);
        r.ai.put_bytes("k", b"v", Meta::at(1)).await.unwrap();
        r.budget.maintain(crate::state::now_ms()).await;
        let idx = r.dir.0.join("ai").join(INDEX_FILE);
        assert!(idx.is_file());
        assert!(!r.dir.0.join("ai").join("index.json.tmp").exists());
        let before = std::fs::read_to_string(&idx).unwrap();
        r.ai.get("k").await.unwrap();
        let now = crate::state::now_ms();
        r.budget.maintain(now + 1000).await; // a read alone, 1 s on: not yet
        assert_eq!(std::fs::read_to_string(&idx).unwrap(), before);
        r.budget.maintain(now + READ_FLUSH_EVERY_MS + 1000).await;
        assert_ne!(std::fs::read_to_string(&idx).unwrap(), before, "the new last-read stamp is written");
    }

    #[tokio::test]
    async fn reset_forgets_everything() {
        let r = rig(MIB);
        r.ai.put_bytes("k", b"v", Meta::at(1)).await.unwrap();
        r.guide.put_bytes("g", b"v", Meta::at(1)).await.unwrap();
        r.budget.reset().await;
        assert_eq!((r.ai.used_bytes(), r.guide.used_bytes()), (0, 0));
        assert!(r.ai.get("k").await.is_none());
        r.ai.put_bytes("k", b"w", Meta::at(2)).await.unwrap();
        assert_eq!(r.ai.used_bytes(), 1);
    }

    /// Each look at the disk shows 8 MiB less, as a growing temporary file would.
    struct Shrinking(AtomicU64);

    impl FreeDisk for Shrinking {
        fn free_bytes(&self) -> Result<u64, String> {
            Ok(self.0.fetch_sub(8 * MIB, Ordering::SeqCst))
        }
    }

    #[tokio::test]
    async fn an_unsized_stream_that_crosses_the_floor_is_aborted_mid_stream() {
        let dir = TempDir::new();
        let free = Arc::new(Shrinking(AtomicU64::new(FREE_DISK_FLOOR_BYTES + 20 * MIB)));
        let budget = Budget::new(GB, FREE_DISK_FLOOR_BYTES, free);
        let guide = BlobStore::open(&dir.0.join("guide"), Kind::Guide, budget.clone()).unwrap();
        let stream = tokio::io::repeat(0).take(64 * MIB);
        let e = guide.put("big", stream, Meta::at(1)).await.unwrap_err();
        assert!(e.reason.contains("2 GB") && e.reason.contains("stopped after"), "{}", e.reason);
        assert_eq!(guide.used_bytes(), 0);
        assert_eq!(std::fs::read_dir(dir.0.join("guide").join("tmp")).unwrap().count(), 0, "the temporary file is removed");
        assert!(guide.get("big").await.is_none());
    }

    #[tokio::test]
    async fn a_sized_stream_is_judged_up_front_and_makes_room_before_writing() {
        let r = rig(300);
        r.guide.put_bytes("old", &[1u8; 200], Meta::at(1)).await.unwrap();
        r.ai.put_bytes("a", &[1u8; 80], Meta::at(1)).await.unwrap();
        // Known to be too big for the whole budget: refused before anything is written.
        let e = r.guide.put_sized("n", &[0u8; 10][..], Some(301), Meta::at(2)).await.unwrap_err();
        assert!(e.reason.contains("301 bytes") && e.reason.contains("300 bytes"), "{}", e.reason);
        // Known size 150 with 280 held: the oldest go first, then it is written.
        r.guide.put_sized("n", &[3u8; 150][..], Some(150), Meta::at(2)).await.unwrap();
        assert!(r.guide.get("old").await.is_none());
        assert_eq!(r.guide.used_bytes() + r.ai.used_bytes(), 230);
        // And a known size that would cross the floor is refused up front.
        r.free.0.store(FREE_DISK_FLOOR_BYTES + 10, Ordering::SeqCst);
        let e = r.guide.put_sized("z", &[0u8; 10][..], Some(50), Meta::at(3)).await.unwrap_err();
        assert!(e.reason.contains("2 GB"), "{}", e.reason);
    }

    #[tokio::test]
    async fn guide_and_picture_indexes_are_on_disk_when_put_returns_but_ai_waits_for_the_ticker() {
        let r = rig(MIB);
        r.guide.put_bytes("g", b"guide", Meta::at(9)).await.unwrap();
        let idx = std::fs::read_to_string(r.dir.0.join("guide").join(INDEX_FILE)).expect("indexed at once");
        assert!(idx.contains(&sha_hex("g")), "{idx}");
        r.ai.put_bytes("a", b"ai", Meta::at(9)).await.unwrap();
        assert!(!r.dir.0.join("ai").join(INDEX_FILE).exists(), "AI answers wait for the 5 s flush");
        // A restart straight after the guide put keeps it.
        let budget = Budget::new(MIB, FREE_DISK_FLOOR_BYTES, r.free.clone());
        let guide = BlobStore::open(&r.dir.0.join("guide"), Kind::Guide, budget).unwrap();
        assert_eq!(guide.used_bytes(), 5);
    }

    #[tokio::test]
    async fn an_old_eviction_never_deletes_a_file_stored_again_at_the_same_path() {
        let r = rig(MIB);
        r.ai.put_bytes("k", b"old", Meta::at(1)).await.unwrap();
        let hash = sha_hex("k");
        let path = r.dir.0.join("ai").join(&hash[..2]).join(&hash);
        // The eviction decided on "k" ...
        let victim = Victim { kind: Kind::Ai, hash: hash.clone(), size: 3, path: path.clone() };
        // ... but "k" was stored again before the unlink ran.
        r.ai.put_bytes("k", b"newer", Meta::at(2)).await.unwrap();
        r.budget.delete_victims(vec![victim]).await;
        let (_, f) = r.ai.get("k").await.expect("the new file survives");
        assert_eq!(read_all(f).await, b"newer");
        // A victim whose id really is gone is unlinked.
        r.ai.put_bytes("gone", b"x", Meta::at(1)).await.unwrap();
        let hg = sha_hex("gone");
        let pg = r.dir.0.join("ai").join(&hg[..2]).join(&hg);
        let v = Victim { kind: Kind::Ai, hash: hg.clone(), size: 1, path: pg.clone() };
        r.budget.lock().maps[0].remove(&hg);
        r.budget.delete_victims(vec![v]).await;
        assert!(!pg.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_file_is_a_miss_but_keeps_its_entry_and_counters() {
        use std::os::unix::fs::PermissionsExt;
        let r = rig(MIB);
        r.ai.put_bytes("k", b"abc", Meta::at(1)).await.unwrap();
        let h = sha_hex("k");
        let path = r.dir.0.join("ai").join(&h[..2]).join(&h);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            return; // running as root: permissions do not apply
        }
        assert!(r.ai.get("k").await.is_none());
        assert_eq!(r.ai.used_bytes(), 3, "not dropped: the file is there, only unreadable");
        assert!(r.ai.meta("k").is_some());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(r.ai.get("k").await.is_some());
    }

    #[test]
    fn budget_is_worked_out_once_saved_and_overridden_by_an_explicit_figure() {
        let dir = TempDir::new();
        assert_eq!(resolve_budget_mb(&dir.0, None, 100_000), 10_000);
        assert!(dir.0.join(BUDGET_FILE).is_file());
        assert_eq!(resolve_budget_mb(&dir.0, None, 10_000), 10_000, "a later start reuses it, whatever is free now");
        assert_eq!(resolve_budget_mb(&dir.0, Some(0), 10_000), 10_000, "0 means automatic");
        assert_eq!(resolve_budget_mb(&dir.0, Some(512), 10_000), 512, "an explicit figure wins");
        assert_eq!(resolve_budget_mb(&dir.0, None, 10_000), 10_000, "and does not overwrite the saved one");
        let big = TempDir::new();
        assert_eq!(resolve_budget_mb(&big.0, None, 900_000), 20_480, "capped at 20 GB");
    }

    #[test]
    fn budget_that_shrinks_evicts_at_start() {
        let dir = TempDir::new();
        let free = Arc::new(FixedFree(AtomicU64::new(500 * GB)));
        {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let budget = Budget::new(MIB, FREE_DISK_FLOOR_BYTES, free.clone());
                let ai = BlobStore::open(&dir.0.join("ai"), Kind::Ai, budget.clone()).unwrap();
                ai.put_bytes("a", &[0u8; 100], Meta::at(1)).await.unwrap();
                ai.put_bytes("b", &[0u8; 100], Meta::at(2)).await.unwrap();
                budget.flush_all().await;
            });
        }
        let budget = Budget::new(150, FREE_DISK_FLOOR_BYTES, free);
        let ai = BlobStore::open(&dir.0.join("ai"), Kind::Ai, budget).unwrap();
        assert_eq!(ai.used_bytes(), 100);
    }

    #[tokio::test]
    async fn an_artefact_counts_and_goes_with_its_guide() {
        let r = rig(300);
        r.guide.put_bytes("g", &[1u8; 100], Meta::at(1)).await.unwrap();
        let art = artefact_path(&r.guide.path_of("g"));
        std::fs::write(&art, [2u8; 150]).unwrap();
        assert!(r.guide.count_artefact("g"));
        assert_eq!(r.guide.used_bytes(), 250);
        // A restart measures it from the file.
        let budget = Budget::new(300, 0, Arc::new(FixedFree(AtomicU64::new(10 * GB))));
        let again = BlobStore::open(&r.dir.0.join("guide"), Kind::Guide, budget).unwrap();
        assert_eq!(again.used_bytes(), 250);
        // Something big enough to need its room evicts both.
        r.ai.put_bytes("x", &[1u8; 100], Meta::at(1)).await.unwrap();
        assert!(r.guide.get("g").await.is_none());
        assert!(!art.exists(), "the artefact is deleted with its guide");
        assert_eq!((r.guide.used_bytes(), r.ai.used_bytes()), (0, 100));
        // Replacing a body deletes the old parse.
        r.guide.put_bytes("h", &[1u8; 10], Meta::at(1)).await.unwrap();
        let art = artefact_of_h(&r);
        std::fs::write(&art, [2u8; 5]).unwrap();
        assert!(r.guide.count_artefact("h"));
        r.guide.put_bytes("h", &[3u8; 10], Meta::at(2)).await.unwrap();
        assert!(!art.exists());
        assert_eq!(r.guide.used_bytes(), 10);
    }

    fn artefact_of_h(r: &Rig) -> PathBuf {
        artefact_path(&r.guide.path_of("h"))
    }
}
