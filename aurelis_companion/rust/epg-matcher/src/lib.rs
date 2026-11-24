//! Native EPG-to-channel matching/merge engine for Aurelis's `core/domain`
//! `EpgInteractor.matchEpg` + `EpgMultiSourceMerge.merge`.
//!
//! Ports the 3-tier match (strict id / normalized key / loose fuzzy) and the
//! sort-dedupe-gapfill programme merge line-for-line. `ChannelNameKeys.normalizeForMatch`
//! never needs porting here -- like `ChannelNameKeys`/`CountryAffinity` for
//! `rust/channel-merge`, the caller precomputes every normalized key
//! (`channelKey`, entry `idKey`/`nameKey`, `normalizedStrictIds`) in Kotlin and
//! this crate only implements the pure matching/merge algorithm over those
//! already-normalized primitives plus `looseKeyMatch`'s own string algorithm
//! (itself already pure and portable, mirrored verbatim below).
//!
//! Owns no long-lived state: one call receives the whole channel + EPG-entry
//! snapshot and returns the per-channel matched/merged programme lists. Not
//! wired in as the app's active matching logic anywhere --
//! `EpgInteractor.matchEpg` remains the only implementation actually
//! selected, mirroring `rust/epg-grid`'s, `rust/xmltv-parser`'s,
//! `rust/m3u-parser`'s, `rust/channel-merge`'s, and `rust/xtream-parser`'s
//! status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jlong, jstring};
use jni::Env;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use rayon::prelude::*;

/// Minimum item count to justify parallel dispatch overhead on Fire TV's
/// small cores. Below this, sequential is faster.
const PARALLEL_THRESHOLD: usize = 32;

/// Configure rayon's global thread pool to 3 threads (Fire TV has 4 small
/// cores shared with the media player and Compose renderer). Idempotent --
/// the first crate to call this wins; subsequent calls are no-ops.
fn init_rayon_pool() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(3).build_global();
    });
}

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

const GAP_TOLERANCE_MS: i64 = 15 * 60 * 1_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpgEntryIn {
    /// Raw XMLTV channel id -- only used for the Tier-1 strict map and (for the
    /// entry immediately preceding a gap) the synthetic gap entry's id/channelId.
    pub channel_id: String,
    pub channel_name: String,
    /// `ChannelNameKeys.normalizeForMatch(channelId)` -- may be empty.
    pub id_key: String,
    /// `ChannelNameKeys.normalizeForMatch(channelName)`, only set when non-empty
    /// AND different from `id_key`, mirroring `EpgInteractor.matchEpg`'s guard.
    pub name_key: Option<String>,
    pub start_time_ms: i64,
    pub end_time_ms: i64,
    /// `!description.isNullOrBlank()` -- richness scoring only needs presence.
    pub has_description: bool,
    /// `!imageUrl.isNullOrBlank()`.
    pub has_image: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelIn {
    /// Primary id + epgId + every merged-variant epgMatchId, each also expanded
    /// with its post-':' tail (mirrors `EpgInteractor.matchEpg`'s `strictIds`
    /// `buildList{}.flatMap{}`), in the same order -- order determines which
    /// match wins under `firstOrNull()`.
    pub strict_ids: Vec<String>,
    /// `ChannelNameKeys.normalizeForMatch(channel.name)`.
    pub channel_key: String,
    /// `strictIds.map(normalizeForMatch)`, same order/length as `strict_ids`
    /// (including empty-string entries, which never match anything).
    pub normalized_strict_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchInput {
    pub epg_entries: Vec<EpgEntryIn>,
    pub channels: Vec<ChannelIn>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum MergedItem {
    /// A (possibly start-trimmed) original entry, referenced by index into the
    /// input `epg_entries` array.
    #[serde(rename = "entry")]
    Entry {
        index: usize,
        /// Set when overlap-dedup trimmed this entry's effective start time.
        #[serde(rename = "startTimeMsOverride", skip_serializing_if = "Option::is_none")]
        start_time_ms_override: Option<i64>,
    },
    /// A synthetic "No information" filler between two entries, mirroring
    /// `EpgMultiSourceMerge.fillGaps`. `afterIndex` is the input-array index of
    /// the entry immediately preceding the gap, so Kotlin can pull its
    /// `channelId`/`channelName` for the synthetic entry.
    #[serde(rename = "gap")]
    Gap {
        after_index: usize,
        start_time_ms: i64,
        end_time_ms: i64,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchOutput {
    /// Aligned 1:1 with input `channels`.
    pub channels: Vec<Vec<MergedItem>>,
}

/// Mirrors `ChannelNameKeys.looseKeyMatch`: containment (>=5 chars) or edit
/// distance <=1 for similar-length keys. Operates on already-normalized keys.
fn loose_key_match(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (shorter, longer) = if a.chars().count() <= b.chars().count() { (a, b) } else { (b, a) };
    let shorter_chars: Vec<char> = shorter.chars().collect();
    let longer_chars: Vec<char> = longer.chars().collect();
    if shorter_chars.len() >= 5 && longer.contains(shorter) {
        return true;
    }
    if longer_chars.len() as isize - shorter_chars.len() as isize > 1 || shorter_chars.len() < 6 {
        return false;
    }
    let same_length = shorter_chars.len() == longer_chars.len();
    let (mut i, mut j, mut edits) = (0usize, 0usize, 0i32);
    while i < shorter_chars.len() && j < longer_chars.len() {
        if shorter_chars[i] == longer_chars[j] {
            i += 1;
            j += 1;
            continue;
        }
        edits += 1;
        if edits > 1 {
            return false;
        }
        if same_length {
            i += 1;
            j += 1;
        } else {
            j += 1;
        }
    }
    edits as usize + (longer_chars.len() - j) <= 1
}

fn richness_penalty(e: &EpgEntryIn) -> i32 {
    let mut score = 0;
    if !e.has_description {
        score += 10;
    }
    if !e.has_image {
        score += 5;
    }
    score
}

/// Mirrors `EpgMultiSourceMerge.merge` over indices into `entries` rather than
/// owned `EpgEntry` values: sort by (start, richness), dedupe/trim overlaps,
/// then fill sub-tolerance gaps with synthetic markers.
fn merge_matches(indices: &[usize], entries: &[EpgEntryIn]) -> Vec<MergedItem> {
    if indices.is_empty() {
        return Vec::new();
    }

    let mut sorted: Vec<usize> = indices
        .iter()
        .copied()
        .filter(|&i| entries[i].end_time_ms > entries[i].start_time_ms)
        .collect();
    sorted.sort_by(|&a, &b| {
        entries[a]
            .start_time_ms
            .cmp(&entries[b].start_time_ms)
            .then(richness_penalty(&entries[a]).cmp(&richness_penalty(&entries[b])))
    });

    // Dedupe overlapping windows, trimming the start of any entry that overlaps
    // the previous one's effective end. Track each surviving item's effective
    // (possibly overridden) start/end alongside its MergedItem for gap-filling.
    struct Deduped {
        item: MergedItem,
        original_index: usize,
        effective_start_ms: i64,
        end_ms: i64,
    }
    let mut deduped: Vec<Deduped> = Vec::new();
    let mut cursor = i64::MIN;
    for &idx in &sorted {
        let e = &entries[idx];
        if e.end_time_ms <= cursor {
            continue;
        }
        if e.start_time_ms < cursor {
            let trimmed_start = cursor;
            if e.end_time_ms > trimmed_start {
                deduped.push(Deduped {
                    item: MergedItem::Entry { index: idx, start_time_ms_override: Some(trimmed_start) },
                    original_index: idx,
                    effective_start_ms: trimmed_start,
                    end_ms: e.end_time_ms,
                });
                cursor = e.end_time_ms;
            }
        } else {
            deduped.push(Deduped {
                item: MergedItem::Entry { index: idx, start_time_ms_override: None },
                original_index: idx,
                effective_start_ms: e.start_time_ms,
                end_ms: e.end_time_ms,
            });
            cursor = e.end_time_ms;
        }
    }

    if deduped.len() < 2 {
        return deduped.into_iter().map(|d| d.item).collect();
    }

    let mut out = Vec::with_capacity(deduped.len() * 2);
    for i in 0..deduped.len() - 1 {
        let gap = deduped[i + 1].effective_start_ms - deduped[i].end_ms;
        out.push(deduped[i].item.clone());
        if gap > 0 && gap <= GAP_TOLERANCE_MS {
            out.push(MergedItem::Gap {
                after_index: deduped[i].original_index,
                start_time_ms: deduped[i].end_ms,
                end_time_ms: deduped[i + 1].effective_start_ms,
            });
        }
    }
    out.push(deduped[deduped.len() - 1].item.clone());
    out
}

/// Per-channel 3-tier match (extracted for rayon parallelism).
fn match_one_channel(
    channel: &ChannelIn,
    by_channel_id: &HashMap<&str, Vec<usize>>,
    by_normalized_key: &HashMap<&str, Vec<usize>>,
    all_epg_keys: &[&str],
    epg_entries: &[EpgEntryIn],
) -> Vec<MergedItem> {
    // Tier 1: strict ID match (primary ID, playlist epgId, or any merged variant id).
    let mut matches: Option<&Vec<usize>> =
        channel.strict_ids.iter().find_map(|id| by_channel_id.get(id.as_str()));

    // Tier 2: normalized-key match.
    if matches.map_or(true, |m| m.is_empty()) && !channel.channel_key.is_empty() {
        matches = by_normalized_key.get(channel.channel_key.as_str());
    }
    // Tier 2b: any strict id normalized.
    if matches.map_or(true, |m| m.is_empty()) {
        matches = channel
            .normalized_strict_ids
            .iter()
            .filter(|k| !k.is_empty())
            .find_map(|k| by_normalized_key.get(k.as_str()));
    }
    // Tier 3: loose similarity over EPG keys, only for still-unmatched channels.
    if matches.map_or(true, |m| m.is_empty()) && channel.channel_key.chars().count() >= 5 {
        if let Some(&loose_key) =
            all_epg_keys.iter().find(|&&k| loose_key_match(&channel.channel_key, k))
        {
            matches = by_normalized_key.get(loose_key);
        }
    }

    let match_indices = matches.cloned().unwrap_or_default();
    merge_matches(&match_indices, epg_entries)
}

/// Mirrors `EpgInteractor.matchEpg` end to end: single-pass strict-id/normalized-key
/// index build, then per-channel 3-tier match, then [merge_matches].
pub fn match_epg(input: &MatchInput) -> MatchOutput {
    if input.channels.is_empty() {
        return MatchOutput { channels: Vec::new() };
    }
    if input.epg_entries.is_empty() {
        return MatchOutput { channels: vec![Vec::new(); input.channels.len()] };
    }

    let mut by_channel_id: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_normalized_key: HashMap<&str, Vec<usize>> = HashMap::new();
    // Insertion-ordered key list, mirroring Kotlin's LinkedHashMap-backed
    // `byNormalizedKey.keys.toList()` so Tier-3's `firstOrNull` tie-break is
    // deterministic and matches the reference implementation.
    let mut all_epg_keys: Vec<&str> = Vec::new();
    let mut seen_keys: HashMap<&str, ()> = HashMap::new();

    for (i, entry) in input.epg_entries.iter().enumerate() {
        by_channel_id.entry(entry.channel_id.as_str()).or_default().push(i);

        if !entry.id_key.is_empty() {
            by_normalized_key.entry(entry.id_key.as_str()).or_default().push(i);
            if seen_keys.insert(entry.id_key.as_str(), ()).is_none() {
                all_epg_keys.push(entry.id_key.as_str());
            }
        }
        if let Some(name_key) = entry.name_key.as_deref() {
            if !name_key.is_empty() {
                by_normalized_key.entry(name_key).or_default().push(i);
                if seen_keys.insert(name_key, ()).is_none() {
                    all_epg_keys.push(name_key);
                }
            }
        }
    }

    let channels_out = if input.channels.len() > PARALLEL_THRESHOLD {
        init_rayon_pool();
        input
            .channels
            .par_iter()
            .map(|channel| match_one_channel(channel, &by_channel_id, &by_normalized_key, &all_epg_keys, &input.epg_entries))
            .collect()
    } else {
        input
            .channels
            .iter()
            .map(|channel| match_one_channel(channel, &by_channel_id, &by_normalized_key, &all_epg_keys, &input.epg_entries))
            .collect()
    };

    MatchOutput { channels: channels_out }
}

fn match_epg_from_json(input_json: &str) -> String {
    match serde_json::from_str::<MatchInput>(input_json) {
        Ok(input) => {
            let output = match_epg(&input);
            serde_json::to_string(&output).unwrap_or_else(|_| "{\"channels\":[]}".to_string())
        }
        // Malformed input must never crash a sync -- return an empty result.
        Err(_) => "{\"channels\":[]}".to_string(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgMatchNativeBridge_nativeMatchEpg(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let input_str: String = match env.get_string(&input_json) {
            Ok(s) => s.into(),
            Err(_) => return Ok(env.new_string("{\"channels\":[]}").unwrap().into_raw()),
        };
        let output_json = match_epg_from_json(&input_str);
        env.new_string(output_json)
            .unwrap_or_else(|_| env.new_string("{\"channels\":[]}").unwrap())
            .into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// ── Raw-input entry point (Phase 3.2): precompute + match in one JNI call ──

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawEpgEntryIn {
    pub channel_id: String,
    pub channel_name: String,
    pub start_time_ms: i64,
    pub end_time_ms: i64,
    pub has_description: bool,
    pub has_image: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawChannelIn {
    /// Primary id + epgId + every merged-variant epgMatchId, each also expanded
    /// with its post-':' tail -- same construction as `ChannelIn.strict_ids`,
    /// just not normalized yet.
    pub strict_ids: Vec<String>,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawMatchInput {
    pub epg_entries: Vec<RawEpgEntryIn>,
    pub channels: Vec<RawChannelIn>,
}

/// Mirrors `EpgInteractor.matchEpg`'s entry precompute: `idKey = norm(channelId)`,
/// `nameKey = norm(channelName)` kept only when the RAW name is non-empty AND the
/// normalized result is non-empty and differs from `idKey`.
fn precompute_epg_entry(raw: &RawEpgEntryIn) -> EpgEntryIn {
    let id_key = aurelis_heuristics::channel_name_keys::normalize_for_match(&raw.channel_id);
    let name_key = if !raw.channel_name.is_empty() {
        let nk = aurelis_heuristics::channel_name_keys::normalize_for_match(&raw.channel_name);
        if !nk.is_empty() && nk != id_key { Some(nk) } else { None }
    } else {
        None
    };
    EpgEntryIn {
        channel_id: raw.channel_id.clone(),
        channel_name: raw.channel_name.clone(),
        id_key,
        name_key,
        start_time_ms: raw.start_time_ms,
        end_time_ms: raw.end_time_ms,
        has_description: raw.has_description,
        has_image: raw.has_image,
    }
}

/// Mirrors `RustEpgMatchEngine.matchEpg`'s channel precompute: `channelKey =
/// norm(channel.name)`, `normalizedStrictIds = strictIds.map(norm)`.
fn precompute_channel(raw: &RawChannelIn) -> ChannelIn {
    let channel_key = aurelis_heuristics::channel_name_keys::normalize_for_match(&raw.name);
    let normalized_strict_ids = raw
        .strict_ids
        .iter()
        .map(|id| aurelis_heuristics::channel_name_keys::normalize_for_match(id))
        .collect();
    ChannelIn { strict_ids: raw.strict_ids.clone(), channel_key, normalized_strict_ids }
}

pub fn match_epg_from_raw(input: &RawMatchInput) -> MatchOutput {
    init_rayon_pool();
    let epg_entries: Vec<EpgEntryIn> = if input.epg_entries.len() > PARALLEL_THRESHOLD {
        input.epg_entries.par_iter().map(precompute_epg_entry).collect()
    } else {
        input.epg_entries.iter().map(precompute_epg_entry).collect()
    };
    let channels: Vec<ChannelIn> = if input.channels.len() > PARALLEL_THRESHOLD {
        input.channels.par_iter().map(precompute_channel).collect()
    } else {
        input.channels.iter().map(precompute_channel).collect()
    };
    match_epg(&MatchInput { epg_entries, channels })
}

fn match_epg_from_raw_json(input_json: &str) -> String {
    match serde_json::from_str::<RawMatchInput>(input_json) {
        Ok(input) => {
            let output = match_epg_from_raw(&input);
            serde_json::to_string(&output).unwrap_or_else(|_| "{\"channels\":[]}".to_string())
        }
        Err(_) => "{\"channels\":[]}".to_string(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgMatchNativeBridge_nativeMatchEpgFromRaw(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let input_str: String = match env.get_string(&input_json) {
            Ok(s) => s.into(),
            Err(_) => return Ok(env.new_string("{\"channels\":[]}").unwrap().into_raw()),
        };
        let output_json = match_epg_from_raw_json(&input_str);
        env.new_string(output_json)
            .unwrap_or_else(|_| env.new_string("{\"channels\":[]}").unwrap())
            .into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgMatchNativeBridge_nativeEngineVersion(
    _env: EnvUnowned,
    _class: JClass,
) -> jint {
    ENGINE_VERSION
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FuzzySearchChannelIn {
    pub id: String,
    pub name: String,
    pub genre: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FuzzySearchInput {
    pub query: String,
    pub channels: Vec<FuzzySearchChannelIn>,
}

fn levenshtein_distance(s1: &str, s2: &str) -> usize {
    let s1_chars: Vec<char> = s1.chars().collect();
    let s2_chars: Vec<char> = s2.chars().collect();
    let mut dp = vec![0; s2_chars.len() + 1];
    for j in 0..=s2_chars.len() {
        dp[j] = j;
    }
    for i in 1..=s1_chars.len() {
        let mut prev = dp[0];
        dp[0] = i;
        for j in 1..=s2_chars.len() {
            let temp = dp[j];
            if s1_chars[i - 1] == s2_chars[j - 1] {
                dp[j] = prev;
            } else {
                dp[j] = std::cmp::min(dp[j - 1], std::cmp::min(dp[j], prev)) + 1;
            }
            prev = temp;
        }
    }
    dp[s2_chars.len()]
}

fn levenshtein_similarity(s1: &str, s2: &str) -> f64 {
    let s1_len = s1.chars().count();
    let s2_len = s2.chars().count();
    let max_len = std::cmp::max(s1_len, s2_len);
    if max_len == 0 {
        return 1.0;
    }
    1.0 - (levenshtein_distance(s1, s2) as f64) / (max_len as f64)
}

/// A typo is about one letter in four: below this similarity the query is a different word
/// ("paranormal" against "paramountn" is 0.5 — owner, 16 Oct 2026).
const TYPO_MIN_SIMILARITY: f64 = 0.75;
/// Shorter queries are matched by containment only: one edit in three letters is any word.
const TYPO_MIN_QUERY_CHARS: usize = 4;

fn calculate_match_score(query: &str, target: &str) -> f64 {
    let q = query.to_lowercase().replace(' ', "");
    let t = target.to_lowercase().replace(' ', "");
    if q == t {
        return 1.0;
    }
    if t.contains(&q) {
        return 0.8 + 0.19 * (q.len() as f64 / t.len() as f64);
    }
    let q_len = q.chars().count();
    if q_len < TYPO_MIN_QUERY_CHARS {
        return 0.0;
    }

    // Against the start of the target, one letter either side of the query's length, so a
    // letter dropped or doubled while typing still lines up ("skysprt" / "skysport").
    let t_len = t.chars().count();
    let mut best = levenshtein_similarity(&q, &t);
    for len in q_len.saturating_sub(1)..=q_len + 1 {
        if len == 0 || len > t_len {
            continue;
        }
        let prefix: String = t.chars().take(len).collect();
        best = best.max(levenshtein_similarity(&q, &prefix));
    }
    // Below containment's 0.8, so a channel that holds the word always ranks first.
    if best >= TYPO_MIN_SIMILARITY { best * 0.75 } else { 0.0 }
}

fn fuzzy_match_channels_impl(query: &str, channels: &[FuzzySearchChannelIn]) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(&FuzzySearchChannelIn, f64)> = channels
        .iter()
        .map(|ch| {
            let name_score = calculate_match_score(query, &ch.name);
            let genre_score = ch
                .genre
                .as_ref()
                .map(|g| calculate_match_score(query, g) * 0.9)
                .unwrap_or(0.0);
            let final_score = name_score.max(genre_score);
            (ch, final_score)
        })
        .filter(|&(_, score)| score >= 0.35)
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(ch, _)| ch.id.clone()).collect()
}

pub fn fuzzy_match_channels(input: &FuzzySearchInput) -> Vec<String> {
    fuzzy_match_channels_impl(&input.query, &input.channels)
}

// ── Resident handle (Phase 3.3): upload the channel list once per generation,
// search against the resident copy per keystroke instead of re-marshalling the
// whole list every call. ──────────────────────────────────────────────────────

struct SearchState {
    generation_id: i64,
    channels: Vec<FuzzySearchChannelIn>,
}

fn search_state() -> &'static Mutex<Option<SearchState>> {
    static STATE: OnceLock<Mutex<Option<SearchState>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadSearchChannelsInput {
    pub generation_id: i64,
    pub channels: Vec<FuzzySearchChannelIn>,
}

pub fn upload_search_channels(generation_id: i64, channels: Vec<FuzzySearchChannelIn>) {
    let mut guard = search_state().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(SearchState { generation_id, channels });
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleSearchOutput {
    /// False when the resident generation doesn't match `generation_id` (e.g. the
    /// process was killed and restarted, or the caller never uploaded) -- tells
    /// Kotlin to re-upload and retry once rather than silently searching stale or
    /// absent data.
    pub ok: bool,
    pub ids: Vec<String>,
}

/// Searches the resident channel list uploaded via [upload_search_channels],
/// only if its generation still matches. Never clones the resident list.
pub fn fuzzy_search_by_handle(generation_id: i64, query: &str) -> HandleSearchOutput {
    let guard = search_state().lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(state) if state.generation_id == generation_id => {
            HandleSearchOutput { ok: true, ids: fuzzy_match_channels_impl(query, &state.channels) }
        }
        _ => HandleSearchOutput { ok: false, ids: Vec::new() },
    }
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgMatchNativeBridge_nativeFuzzyMatchChannels(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let input_str: String = match env.get_string(&input_json) {
            Ok(s) => s.into(),
            Err(_) => return Ok(env.new_string("[]").unwrap().into_raw()),
        };
    
        let result = match serde_json::from_str::<FuzzySearchInput>(&input_str) {
            Ok(input) => {
                let matched_ids = fuzzy_match_channels(&input);
                serde_json::to_string(&matched_ids).unwrap_or_else(|_| "[]".to_string())
            }
            Err(_) => "[]".to_string(),
        };

        env.new_string(result)
            .unwrap_or_else(|_| env.new_string("[]").unwrap())
            .into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgMatchNativeBridge_nativeUploadSearchChannels(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) {
    env.with_env(|env| -> std::result::Result<(), jni::errors::Error> {
        Ok({
        let input_str: String = match env.get_string(&input_json) {
            Ok(s) => s.into(),
            Err(_) => return Ok(()),
        };
        if let Ok(input) = serde_json::from_str::<UploadSearchChannelsInput>(&input_str) {
            upload_search_channels(input.generation_id, input.channels);
        }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgMatchNativeBridge_nativeFuzzySearchByHandle(
    mut env: EnvUnowned,
    _class: JClass,
    generation_id: jlong,
    query: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        const FALLBACK: &str = "{\"ok\":false,\"ids\":[]}";
        let q: String = match env.get_string(&query) {
            Ok(s) => s.into(),
            Err(_) => return Ok(env.new_string(FALLBACK).unwrap().into_raw()),
        };
        let output = fuzzy_search_by_handle(generation_id, &q);
        let json = serde_json::to_string(&output).unwrap_or_else(|_| FALLBACK.to_string());
        env.new_string(json).unwrap_or_else(|_| env.new_string(FALLBACK).unwrap()).into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// Phase 6 marshalling upgrade: returns a `jobjectArray` of String IDs directly,
/// eliminating Gson serialization on the per-keystroke hot path. Returns `null`
/// when the generation doesn't match (ok=false), so the Kotlin caller can check
/// for null and retry with a fresh upload.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgMatchNativeBridge_nativeFuzzySearchByHandleFast(
    mut env: EnvUnowned,
    _class: JClass,
    generation_id: jlong,
    query: JString,
) -> jni::sys::jobject {
    env.with_env(|env| -> std::result::Result<jni::sys::jobject, jni::errors::Error> {
        Ok({
        let q: String = match env.get_string(&query) {
            Ok(s) => s.into(),
            Err(_) => return Ok(std::ptr::null_mut()),
        };
        let output = fuzzy_search_by_handle(generation_id, &q);
        if !output.ok {
            return Ok(std::ptr::null_mut());
        }

        // Uses the process-lifetime cached java/lang/String class populated in
        // aurelis-core's JNI_OnLoad, avoiding a find_class on every keystroke.
        // Falls back to find_class when the cache was never populated.
        let array = match jni_cache::new_string_object_array(env, output.ids.len() as jni::sys::jsize) {
            Ok(a) => a,
            Err(_) => return Ok(std::ptr::null_mut()),
        };

        for (i, id) in output.ids.iter().enumerate() {
            match env.new_string(id) {
                Ok(jstr) => {
                    if env.set_object_array_element(&array, i, &jstr).is_err() {
                        return Ok(std::ptr::null_mut());
                    }
                }
                Err(_) => return Ok(std::ptr::null_mut()),
            }
        }

        array.into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelPlanIn {
    pub id: String,
    pub name_key: String,
    pub manual_epg_id: Option<String>,
    pub manual_epg_id_norm: Option<String>,
    pub exact_id_candidates: Vec<String>,
    pub exact_id_candidates_norm: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildPlanInput {
    pub channels: Vec<ChannelPlanIn>,
    pub db_channel_ids: Vec<String>,
    pub normalized_to_db_ids: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildPlanOutput {
    pub attempts_by_channel_id: HashMap<String, Vec<Vec<String>>>,
    pub needed_db_ids: Vec<String>,
}

pub fn build_plan(input: &BuildPlanInput) -> BuildPlanOutput {
    let db_id_set: std::collections::HashSet<&str> = input.db_channel_ids.iter().map(|s| s.as_str()).collect();
    let all_epg_keys: Vec<&str> = input.normalized_to_db_ids.keys().map(|k| k.as_str()).collect();
    let mut needed = std::collections::HashSet::new();
    let mut attempts = HashMap::new();

    for channel in &input.channels {
        let mut channel_attempts = Vec::new();

        // 0) Manual override (C2): the user pinned this channel to a stored EPG id.
        if let Some(manual) = &channel.manual_epg_id {
            let manual_trimmed = manual.trim();
            if !manual_trimmed.is_empty() {
                let mut ids = Vec::new();
                if db_id_set.contains(manual_trimmed) {
                    ids.push(manual_trimmed.to_string());
                }
                let manual_lower = manual_trimmed.to_lowercase();
                if db_id_set.contains(manual_lower.as_str()) {
                    ids.push(manual_lower);
                }
                if let Some(norm_manual) = &channel.manual_epg_id_norm {
                    if let Some(mapped_ids) = input.normalized_to_db_ids.get(norm_manual) {
                        ids.extend(mapped_ids.clone());
                    }
                }
                ids.sort();
                ids.dedup();
                if !ids.is_empty() {
                    channel_attempts.push(ids);
                }
            }
        }

        // 1) Raw exact ids, in candidate order, each its own attempt
        for candidate in &channel.exact_id_candidates {
            if db_id_set.contains(candidate.as_str()) {
                channel_attempts.push(vec![candidate.clone()]);
            }
        }

        // 2) Every stored id sharing the channel NAME's normalized key
        if let Some(mapped_ids) = input.normalized_to_db_ids.get(&channel.name_key) {
            channel_attempts.push(mapped_ids.clone());
        }

        // 3) Every stored id sharing a candidate id's normalized key, in order
        for norm_candidate in &channel.exact_id_candidates_norm {
            if let Some(mapped_ids) = input.normalized_to_db_ids.get(norm_candidate) {
                channel_attempts.push(mapped_ids.clone());
            }
        }

        // 4) Loose fuzzy only when nothing above matched at all
        if channel_attempts.is_empty() && channel.name_key.chars().count() >= 3 {
            let mut best_loose: Option<&str> = None;
            let mut best_diff = i32::MAX;
            let channel_key_len = channel.name_key.chars().count() as i32;

            for &loose_key in &all_epg_keys {
                if loose_key_match(&channel.name_key, loose_key) {
                    let diff = (loose_key.chars().count() as i32 - channel_key_len).abs();
                    if diff < best_diff {
                        best_diff = diff;
                        best_loose = Some(loose_key);
                    }
                }
            }

            if let Some(loose_key) = best_loose {
                if let Some(mapped_ids) = input.normalized_to_db_ids.get(loose_key) {
                    channel_attempts.push(mapped_ids.clone());
                }
            }
        }

        for att in &channel_attempts {
            for id in att {
                needed.insert(id.clone());
            }
        }
        attempts.insert(channel.id.clone(), channel_attempts);
    }

    let mut needed_list: Vec<String> = needed.into_iter().collect();
    needed_list.sort();

    BuildPlanOutput {
        attempts_by_channel_id: attempts,
        needed_db_ids: needed_list,
    }
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgMatchNativeBridge_nativeBuildPlan(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let input_str: String = match env.get_string(&input_json) {
            Ok(s) => s.into(),
            Err(_) => return Ok(env.new_string("{\"attemptsByChannelId\":{},\"neededDbIds\":[]}").unwrap().into_raw()),
        };
    
        let result = match serde_json::from_str::<BuildPlanInput>(&input_str) {
            Ok(input) => {
                let plan = build_plan(&input);
                serde_json::to_string(&plan).unwrap_or_else(|_| "{\"attemptsByChannelId\":{},\"neededDbIds\":[]}".to_string())
            }
            Err(e) => {
                let err_msg = format!("{{\"attemptsByChannelId\":{{}},\"neededDbIds\":[],\"error\":\"{}\"}}", e);
                err_msg
            }
        };

        env.new_string(result)
            .unwrap_or_else(|_| env.new_string("{\"attemptsByChannelId\":{},\"neededDbIds\":[]}").unwrap())
            .into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[cfg(test)]

mod tests {
    use super::*;

    fn entry(channel_id: &str, id_key: &str, start: i64, end: i64) -> EpgEntryIn {
        EpgEntryIn {
            channel_id: channel_id.to_string(),
            channel_name: String::new(),
            id_key: id_key.to_string(),
            name_key: None,
            start_time_ms: start,
            end_time_ms: end,
            has_description: true,
            has_image: true,
        }
    }

    fn channel(strict_ids: &[&str], channel_key: &str) -> ChannelIn {
        let ids: Vec<String> = strict_ids.iter().map(|s| s.to_string()).collect();
        ChannelIn {
            normalized_strict_ids: ids.iter().map(|_| String::new()).collect(),
            strict_ids: ids,
            channel_key: channel_key.to_string(),
        }
    }

    #[test]
    fn empty_channels_returns_empty() {
        let input = MatchInput { epg_entries: vec![], channels: vec![] };
        assert!(match_epg(&input).channels.is_empty());
    }

    #[test]
    fn empty_epg_returns_one_empty_list_per_channel() {
        let input = MatchInput {
            epg_entries: vec![],
            channels: vec![channel(&["bbc1"], "bbcone"), channel(&["bbc2"], "bbctwo")],
        };
        let out = match_epg(&input);
        assert_eq!(out.channels.len(), 2);
        assert!(out.channels[0].is_empty());
        assert!(out.channels[1].is_empty());
    }

    #[test]
    fn tier1_strict_id_match_wins() {
        let input = MatchInput {
            epg_entries: vec![entry("bbc.one.uk", "bbcone", 0, 1000)],
            channels: vec![channel(&["bbc.one.uk"], "bbcone")],
        };
        let out = match_epg(&input);
        assert_eq!(out.channels[0].len(), 1);
        assert!(matches!(out.channels[0][0], MergedItem::Entry { index: 0, .. }));
    }

    #[test]
    fn tier2_normalized_key_match_when_strict_id_differs() {
        let input = MatchInput {
            epg_entries: vec![entry("some.other.id", "bbcone", 0, 1000)],
            channels: vec![channel(&["playlist:123"], "bbcone")],
        };
        let out = match_epg(&input);
        assert_eq!(out.channels[0].len(), 1);
    }

    #[test]
    fn tier3_loose_match_catches_typo() {
        let input = MatchInput {
            epg_entries: vec![entry("id1", "skysportsmainevent", 0, 1000)],
            channels: vec![channel(&["unrelated"], "skysprtsmainevent")],
        };
        let out = match_epg(&input);
        assert_eq!(out.channels[0].len(), 1);
    }

    #[test]
    fn no_match_returns_empty_list() {
        let input = MatchInput {
            epg_entries: vec![entry("id1", "completelydifferent", 0, 1000)],
            channels: vec![channel(&["unrelated"], "bbcone")],
        };
        let out = match_epg(&input);
        assert!(out.channels[0].is_empty());
    }

    #[test]
    fn merge_dedupes_overlap_and_trims_start() {
        let entries = vec![
            entry("id1", "bbcone", 0, 1000),
            entry("id1", "bbcone", 500, 1500),
        ];
        let indices = vec![0, 1];
        let merged = merge_matches(&indices, &entries);
        assert_eq!(merged.len(), 2);
        match &merged[0] {
            MergedItem::Entry { index, start_time_ms_override } => {
                assert_eq!(*index, 0);
                assert!(start_time_ms_override.is_none());
            }
            _ => panic!("expected entry"),
        }
        match &merged[1] {
            MergedItem::Entry { index, start_time_ms_override } => {
                assert_eq!(*index, 1);
                assert_eq!(*start_time_ms_override, Some(1000));
            }
            _ => panic!("expected entry"),
        }
    }

    #[test]
    fn merge_drops_entry_fully_contained_in_previous() {
        let entries = vec![
            entry("id1", "bbcone", 0, 2000),
            entry("id1", "bbcone", 500, 1500),
        ];
        let indices = vec![0, 1];
        let merged = merge_matches(&indices, &entries);
        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0], MergedItem::Entry { index: 0, .. }));
    }

    #[test]
    fn merge_fills_small_gap_and_skips_large_gap() {
        let mut e1 = entry("id1", "bbcone", 0, 1000);
        e1.has_description = false;
        e1.has_image = false;
        let entries = vec![e1, entry("id1", "bbcone", 1500, 2000)];
        let indices = vec![0, 1];
        let merged = merge_matches(&indices, &entries);
        assert_eq!(merged.len(), 3);
        match &merged[1] {
            MergedItem::Gap { after_index, start_time_ms, end_time_ms } => {
                assert_eq!(*after_index, 0);
                assert_eq!(*start_time_ms, 1000);
                assert_eq!(*end_time_ms, 1500);
            }
            _ => panic!("expected gap"),
        }

        let far_entries = vec![entry("id1", "bbcone", 0, 1000), entry("id1", "bbcone", 2_000_000, 2_001_000)];
        let far_merged = merge_matches(&indices, &far_entries);
        assert_eq!(far_merged.len(), 2);
    }

    #[test]
    fn merge_prefers_richer_entry_at_same_start_time() {
        let mut sparse = entry("id1", "bbcone", 0, 1000);
        sparse.has_description = false;
        sparse.has_image = false;
        let rich = entry("id1", "bbcone", 0, 1000);
        // sparse listed first in input order but richer one should sort first.
        let entries = vec![sparse, rich];
        let indices = vec![0, 1];
        let merged = merge_matches(&indices, &entries);
        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0], MergedItem::Entry { index: 1, .. }));
    }

    #[test]
    fn loose_key_match_containment_and_edit_distance() {
        assert!(loose_key_match("skysports", "skysportsmainevent"));
        assert!(loose_key_match("skysportsmainevent", "skysprtsmainevent"));
        assert!(!loose_key_match("bbcone", "itv"));
        assert!(loose_key_match("same", "same"));
        assert!(!loose_key_match("", "anything"));
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }

    #[test]
    fn fuzzy_match_channels_picks_correct_names() {
        let input = FuzzySearchInput {
            query: "skysprt".to_string(),
            channels: vec![
                FuzzySearchChannelIn {
                    id: "1".to_string(),
                    name: "Sky Sports Main Event".to_string(),
                    genre: Some("Sports".to_string()),
                },
                FuzzySearchChannelIn {
                    id: "2".to_string(),
                    name: "BBC One".to_string(),
                    genre: Some("Entertainment".to_string()),
                },
            ],
        };
        let out = fuzzy_match_channels(&input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "1");
    }

    fn search(query: &str, names: &[&str]) -> Vec<String> {
        let channels = names
            .iter()
            .enumerate()
            .map(|(i, n)| FuzzySearchChannelIn { id: i.to_string(), name: n.to_string(), genre: None })
            .collect::<Vec<_>>();
        fuzzy_match_channels(&FuzzySearchInput { query: query.to_string(), channels })
    }

    /// Owner, 16 Oct 2026: "paranormal" offered Paramount — five letters wrong in ten passed
    /// a bar set at 0.35. A typo is one or two letters, not half the word.
    #[test]
    fn fuzzy_search_does_not_offer_a_different_word() {
        assert!(search("paranormal", &["Paramount Network", "Paramount+", "Paramount Comedy"]).is_empty());
        assert_eq!(search("paranormal", &["Paramount Network", "Paranormal TV"]), vec!["1".to_string()]);
    }

    #[test]
    fn fuzzy_search_still_forgives_a_typo() {
        assert_eq!(search("skysprots", &["Sky Sports Main Event", "Sky News"]), vec!["0".to_string()]);
        assert_eq!(search("discovry", &["Discovery", "Dave"]), vec!["0".to_string()]);
    }

    #[test]
    fn fuzzy_search_short_queries_match_only_what_contains_them() {
        assert_eq!(search("bbc", &["BBC One", "ITV1", "CBS"]), vec!["0".to_string()]);
    }

    // ── Raw-input entry point (Phase 3.2) parity tests ──────────────────────
    // These call `match_epg_from_raw` with only raw channel-id/name strings --
    // no precomputed keys -- and check it reproduces the same tiered-match
    // decisions the precomputed-input path makes for equivalent real-world shapes.

    fn raw_entry(channel_id: &str, channel_name: &str, start: i64, end: i64) -> RawEpgEntryIn {
        RawEpgEntryIn {
            channel_id: channel_id.to_string(),
            channel_name: channel_name.to_string(),
            start_time_ms: start,
            end_time_ms: end,
            has_description: true,
            has_image: true,
        }
    }

    fn raw_channel(strict_ids: &[&str], name: &str) -> RawChannelIn {
        RawChannelIn {
            strict_ids: strict_ids.iter().map(|s| s.to_string()).collect(),
            name: name.to_string(),
        }
    }

    #[test]
    fn raw_entry_point_tier1_strict_id_match() {
        let input = RawMatchInput {
            epg_entries: vec![raw_entry("bbc.one.uk", "BBC One", 0, 1000)],
            channels: vec![raw_channel(&["bbc.one.uk"], "BBC One")],
        };
        let out = match_epg_from_raw(&input);
        assert_eq!(out.channels[0].len(), 1);
        assert!(matches!(out.channels[0][0], MergedItem::Entry { index: 0, .. }));
    }

    // Different ids but names normalize to the same key ("BBC One" / "BBC One HD" ->
    // "bbc1") -- must match via Tier 2, computed entirely from raw names now.
    #[test]
    fn raw_entry_point_tier2_normalized_name_match() {
        let input = RawMatchInput {
            epg_entries: vec![raw_entry("xmltv.bbc1.uk", "BBC One HD", 0, 1000)],
            channels: vec![raw_channel(&["playlist:123"], "BBC One")],
        };
        let out = match_epg_from_raw(&input);
        assert_eq!(out.channels[0].len(), 1);
    }

    #[test]
    fn raw_entry_point_tier3_loose_typo_match() {
        let input = RawMatchInput {
            epg_entries: vec![raw_entry("id1", "Sky Sports Main Event", 0, 1000)],
            channels: vec![raw_channel(&["unrelated"], "Sky Sprts Main Event")],
        };
        let out = match_epg_from_raw(&input);
        assert_eq!(out.channels[0].len(), 1);
    }

    #[test]
    fn raw_entry_point_no_match_stays_empty() {
        let input = RawMatchInput {
            epg_entries: vec![raw_entry("id1", "Completely Different", 0, 1000)],
            channels: vec![raw_channel(&["unrelated"], "BBC One")],
        };
        let out = match_epg_from_raw(&input);
        assert!(out.channels[0].is_empty());
    }

    // ── Resident handle (Phase 3.3) ─────────────────────────────────────────
    // Single test: `search_state` is one process-wide global, and cargo test
    // runs tests in parallel threads within the same process, so multiple tests
    // touching it would race each other. Everything that needs sequencing
    // against that shared state lives in one function.
    #[test]
    fn resident_handle_upload_search_generation_lifecycle() {
        let sky = FuzzySearchChannelIn {
            id: "1".to_string(),
            name: "Sky Sports Main Event".to_string(),
            genre: Some("Sports".to_string()),
        };
        let bbc = FuzzySearchChannelIn {
            id: "2".to_string(),
            name: "BBC One".to_string(),
            genre: Some("Entertainment".to_string()),
        };

        // Before any upload in this generation, a search against an unseen
        // generation id must report a mismatch, never silently search nothing.
        let miss = fuzzy_search_by_handle(999_111, "sky");
        assert!(!miss.ok);
        assert!(miss.ids.is_empty());

        // Upload generation 1, search it -- must find the Sky channel by name.
        upload_search_channels(1, vec![sky.clone(), bbc.clone()]);
        let hit = fuzzy_search_by_handle(1, "skysprt");
        assert!(hit.ok);
        assert_eq!(hit.ids, vec!["1".to_string()]);

        // Same generation id, different query -- resident data still valid.
        let hit2 = fuzzy_search_by_handle(1, "bbc");
        assert!(hit2.ok);
        assert_eq!(hit2.ids, vec!["2".to_string()]);

        // Searching a stale generation id (channel list changed since, e.g. a
        // playlist refresh) must report a mismatch, not stale results.
        let stale = fuzzy_search_by_handle(0, "sky");
        assert!(!stale.ok);

        // Re-upload as generation 2 with a different channel set -- generation 1
        // must no longer be searchable, and generation 2 must reflect the new set.
        let itv = FuzzySearchChannelIn { id: "3".to_string(), name: "ITV1".to_string(), genre: None };
        upload_search_channels(2, vec![itv]);
        let old_gen = fuzzy_search_by_handle(1, "sky");
        assert!(!old_gen.ok);
        let new_gen = fuzzy_search_by_handle(2, "itv");
        assert!(new_gen.ok);
        assert_eq!(new_gen.ids, vec!["3".to_string()]);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
