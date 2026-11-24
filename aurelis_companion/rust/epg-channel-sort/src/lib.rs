//! Native channel-ordering engine for Aurelis's `core/domain` `EpgChannelSort` and
//! `ChannelCurationSort`.
//!
//! Ports `EpgChannelSort.sortBaseline`/`applyEditorialOverlay` (including the
//! smart-ordering watch lane, which this crate silently omitted until v3) and
//! `ChannelCurationSort.curate` line-for-line as a single three-stage pipeline
//! (baseline sort -> brand/family curation -> editorial overlay). Every
//! Aurelis-specific heuristic (`ChannelNameKeys.isTimeshift`/`normalizeForMatch`,
//! `ChannelCurationKeys.familyKey`/`brandKey`, `LocaleChannelPlan.guideRank`,
//! `ChannelSortKeys.dialNumberFromPlaylistOrName`) is a caller-precomputed primitive
//! on [`ChannelIn`], mirroring `rust/channel-merge`'s convention -- this crate only
//! implements the generic multi-key stable sort and brand-clustering algorithm over
//! already-resolved values.
//!
//! Owns no long-lived state: one call receives the whole channel list and returns
//! the final ordering as a permutation of input indices. Not wired in as the app's
//! active sort logic anywhere -- `EpgChannelSort`/`ChannelCurationSort` remain the
//! only implementations actually selected, mirroring `rust/channel-merge`'s dormant
//! status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 3;

/// Sentinel mirroring Kotlin's `Int.MAX_VALUE` "unknown/unranked" fallback.
const UNRANKED: i64 = i64::MAX;

/// Mirrors `EpgChannelSort.SMART_LANE_PER_TAB`. Keep the two in step.
const SMART_LANE_PER_TAB: usize = 5;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelIn {
    #[serde(default)]
    pub source_order_index: Option<i64>,
    #[serde(default)]
    pub playlist_order: i64,
    #[serde(default)]
    pub is_timeshift: bool,
    #[serde(default)]
    pub name_lower: String,
    #[serde(default)]
    pub family_key: String,
    #[serde(default)]
    pub brand_key: String,
    #[serde(default)]
    pub plan_rank: Option<i64>,
    #[serde(default)]
    pub dial_number: Option<i64>,
    #[serde(default)]
    pub manual_position: Option<i64>,
    #[serde(default)]
    pub country_priority: i64,
    #[serde(default)]
    pub is_local_or_regional: bool,
    /// Household-wide affinity rank from `ChannelWatchAffinity.order` (lower is
    /// stronger), or `None` if this channel did not clear the lane threshold.
    /// Densified per tab inside [`apply_editorial_overlay`], never used raw.
    #[serde(default)]
    pub watch_order: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SortInput {
    pub channels: Vec<ChannelIn>,
    #[serde(default)]
    pub apply_editorial_overlay: bool,
    #[serde(default)]
    pub plan_category_is_entertainment: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SortOutput {
    /// Permutation of the input `channels` indices in final display order.
    pub order: Vec<usize>,
}

/// Byte-for-byte port of `sortBaseline` (EpgChannelSort.kt).
fn sort_baseline(channels: &[ChannelIn]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..channels.len()).collect();
    order.sort_by(|&a, &b| {
        let ca = &channels[a];
        let cb = &channels[b];
        ca.source_order_index
            .unwrap_or(UNRANKED)
            .cmp(&cb.source_order_index.unwrap_or(UNRANKED))
            .then(ca.playlist_order.cmp(&cb.playlist_order))
            .then(ca.is_timeshift.cmp(&cb.is_timeshift))
            .then(ca.name_lower.cmp(&cb.name_lower))
            .then(a.cmp(&b))
    });
    order
}

fn trailing_digits(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    let end = bytes.len();
    let mut start = end;
    while start > 0 && bytes[start - 1].is_ascii_digit() && end - (start - 1) <= 3 {
        start -= 1;
    }
    if start == end {
        None
    } else {
        s[start..end].parse::<i64>().ok()
    }
}

/// Byte-for-byte port of `ChannelCurationSort.curate`, operating on the baseline
/// order (a permutation of original indices) rather than re-deriving it.
fn curate(baseline: &[usize], channels: &[ChannelIn]) -> Vec<usize> {
    let n = baseline.len();
    if n <= 1 {
        return baseline.to_vec();
    }

    let family: Vec<&str> = baseline.iter().map(|&i| channels[i].family_key.as_str()).collect();
    let brand: Vec<&str> = baseline.iter().map(|&i| channels[i].brand_key.as_str()).collect();
    let is_shift: Vec<bool> = baseline.iter().map(|&i| channels[i].is_timeshift).collect();

    let mut first_index_by_brand: HashMap<&str, usize> = HashMap::new();
    for (pos, &b) in brand.iter().enumerate() {
        first_index_by_brand.entry(b).or_insert(pos);
    }
    let mut first_index_by_family: HashMap<&str, usize> = HashMap::new();
    for (pos, &f) in family.iter().enumerate() {
        first_index_by_family.entry(f).or_insert(pos);
    }

    let mut families_per_brand: HashMap<&str, HashSet<&str>> = HashMap::new();
    for pos in 0..n {
        families_per_brand.entry(brand[pos]).or_default().insert(family[pos]);
    }

    let family_number_key = |pos: usize| -> i64 {
        let count = families_per_brand.get(brand[pos]).map(|s| s.len()).unwrap_or(0);
        if count < 2 {
            return -1;
        }
        trailing_digits(family[pos]).unwrap_or(-1)
    };

    let mut positions: Vec<usize> = (0..n).collect();
    positions.sort_by(|&pa, &pb| {
        first_index_by_brand[brand[pa]]
            .cmp(&first_index_by_brand[brand[pb]])
            .then(family_number_key(pa).cmp(&family_number_key(pb)))
            .then(first_index_by_family[family[pa]].cmp(&first_index_by_family[family[pb]]))
            .then(is_shift[pa].cmp(&is_shift[pb]))
            .then(pa.cmp(&pb))
    });

    positions.into_iter().map(|pos| baseline[pos]).collect()
}

/// Byte-for-byte port of `applyEditorialOverlay` (EpgChannelSort.kt).
fn apply_editorial_overlay(curated: &[usize], channels: &[ChannelIn], input: &SortInput) -> Vec<usize> {
    if !input.apply_editorial_overlay {
        return curated.to_vec();
    }

    let mut family_best: HashMap<&str, i64> = HashMap::new();
    for &i in curated {
        let ch = &channels[i];
        if !ch.is_timeshift {
            let rank = ch.plan_rank.unwrap_or(UNRANKED);
            let entry = family_best.entry(ch.family_key.as_str()).or_insert(UNRANKED);
            if rank < *entry {
                *entry = rank;
            }
        }
    }

    let effective_rank = |i: usize| -> i64 {
        let ch = &channels[i];
        let fam_best = *family_best.get(ch.family_key.as_str()).unwrap_or(&UNRANKED);
        if fam_best != UNRANKED {
            fam_best
        } else {
            ch.plan_rank.unwrap_or(UNRANKED)
        }
    };
    // Kotlin sends membership of EntertainmentLocalBlock in `is_local_or_regional`. No family
    // inheritance: that carried the +1s into the block (Kotlin parity).
    let entertainment_tier = |i: usize| -> i32 {
        if input.plan_category_is_entertainment && !channels[i].is_local_or_regional {
            1
        } else {
            0
        }
    };

    // Smart lane, densified against THIS tab -- mirrors the Kotlin overlay. Taking
    // `watch_order` raw would tie the lane to how a channel ranks across everything
    // the household watches, so a tab watched less often than the rest would get no
    // lane at all rather than its own most-watched channels.
    let mut lane_candidates: Vec<(i64, usize)> = curated
        .iter()
        .filter_map(|&i| channels[i].watch_order.map(|w| (w, i)))
        .collect();
    lane_candidates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    lane_candidates.truncate(SMART_LANE_PER_TAB);
    let lane_rank: HashMap<usize, i64> = lane_candidates
        .into_iter()
        .enumerate()
        .map(|(rank, (_, i))| (i, rank as i64))
        .collect();
    let lane = |i: usize| -> i64 { *lane_rank.get(&i).unwrap_or(&UNRANKED) };

    let mut positions: Vec<usize> = (0..curated.len()).collect();
    positions.sort_by(|&pa, &pb| {
        let a = curated[pa];
        let b = curated[pb];
        channels[a]
            .manual_position
            .unwrap_or(UNRANKED)
            .cmp(&channels[b].manual_position.unwrap_or(UNRANKED))
            .then(channels[a].country_priority.cmp(&channels[b].country_priority))
            .then(entertainment_tier(a).cmp(&entertainment_tier(b)))
            // Above the plan, below manual positions -- same slot as Kotlin.
            .then(lane(a).cmp(&lane(b)))
            .then(effective_rank(a).cmp(&effective_rank(b)))
            // Family + parent-before-+1 before dial — mirrors Kotlin EpgChannelSort overlay.
            .then(channels[a].family_key.cmp(&channels[b].family_key))
            .then(channels[a].is_timeshift.cmp(&channels[b].is_timeshift))
            .then(channels[a].dial_number.unwrap_or(UNRANKED).cmp(&channels[b].dial_number.unwrap_or(UNRANKED)))
            .then(pa.cmp(&pb))
    });

    positions.into_iter().map(|pos| curated[pos]).collect()
}

/// Byte-for-byte port of `EpgChannelSort.orderChannels`'s full pipeline.
pub fn order_channels(input: &SortInput) -> Vec<usize> {
    let n = input.channels.len();
    if n <= 1 {
        return (0..n).collect();
    }
    let baseline = sort_baseline(&input.channels);
    let curated = curate(&baseline, &input.channels);
    apply_editorial_overlay(&curated, &input.channels, input)
}

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgChannelSortNativeBridge_nativeOrderChannels<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let raw = jstring_to_string(env, &input_json);
        let output = match serde_json::from_str::<SortInput>(&raw) {
            Ok(input) => SortOutput { order: order_channels(&input) },
            Err(_) => SortOutput { order: Vec::new() },
        };
        let json = serde_json::to_string(&output).unwrap_or_else(|_| "{\"order\":[]}".to_string());
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// ── Raw-input entry point (Phase 3.2): precompute + sort in one JNI call ──

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawChannelIn {
    #[serde(default)]
    pub source_order_index: Option<i64>,
    #[serde(default)]
    pub playlist_order: i64,
    pub name: String,
    #[serde(default)]
    pub playlist_channel_id: Option<String>,
    #[serde(default)]
    pub manual_position: Option<i64>,
    #[serde(default)]
    pub country_priority: i64,
    #[serde(default)]
    pub is_local_or_regional: bool,
    #[serde(default)]
    pub watch_order: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawSortInput {
    pub channels: Vec<RawChannelIn>,
    #[serde(default)]
    pub apply_editorial_overlay: bool,
    #[serde(default)]
    pub plan_category_is_entertainment: bool,
    #[serde(default)]
    pub plan_country: Option<String>,
    #[serde(default)]
    pub plan_category: Option<String>,
}

/// Mirrors `EpgChannelSort.toNativeIn`'s per-channel precompute (`isTimeshift`,
/// `familyKey`/`brandKey`, `planRank`, `dialNumber`) -- the five small JNI calls
/// per channel Kotlin used to make -- now computed directly via same-workspace
/// `aurelis-heuristics` Rust calls. `sourceOrderIndex`/`playlistOrder`/
/// `manualPosition`/`isLocalOrRegional` were never JNI-derived (map lookups /
/// plain model fields already cheap in Kotlin), so they pass through unchanged.
fn precompute_channel(
    raw: &RawChannelIn,
    plan_cc: Option<&str>,
    plan_category: Option<aurelis_heuristics::ContentCategory>,
) -> ChannelIn {
    let is_timeshift = aurelis_heuristics::channel_name_keys::is_timeshift(&raw.name);
    let name_lower = raw.name.to_lowercase();
    let family_key = aurelis_heuristics::channel_curation_keys::family_key(&raw.name);
    let brand_key = aurelis_heuristics::channel_curation_keys::brand_key(&raw.name);
    let plan_rank = plan_cc
        .and_then(|cc| aurelis_heuristics::locale_channel_plan::guide_rank(Some(cc), plan_category, &raw.name))
        .map(|v| v as i64);
    let dial_number = plan_cc
        .filter(|cc| !cc.trim().is_empty())
        .and_then(|_| {
            aurelis_heuristics::channel_sort_keys::dial_number_from_playlist_or_name(
                &raw.name,
                raw.playlist_channel_id.as_deref(),
            )
        })
        .map(|v| v as i64);

    ChannelIn {
        source_order_index: raw.source_order_index,
        playlist_order: raw.playlist_order,
        is_timeshift,
        name_lower,
        family_key,
        brand_key,
        plan_rank,
        dial_number,
        manual_position: raw.manual_position,
        country_priority: raw.country_priority,
        is_local_or_regional: raw.is_local_or_regional,
        watch_order: raw.watch_order,
    }
}

pub fn order_channels_from_raw(input: &RawSortInput) -> Vec<usize> {
    let plan_cc = input.plan_country.as_deref();
    let plan_category = input.plan_category.as_deref().and_then(aurelis_heuristics::ContentCategory::from_str);
    let channels: Vec<ChannelIn> =
        input.channels.iter().map(|c| precompute_channel(c, plan_cc, plan_category)).collect();
    let sort_input = SortInput {
        channels,
        apply_editorial_overlay: input.apply_editorial_overlay,
        plan_category_is_entertainment: input.plan_category_is_entertainment,
    };
    order_channels(&sort_input)
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgChannelSortNativeBridge_nativeOrderChannelsFromRaw<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let raw = jstring_to_string(env, &input_json);
        let output = match serde_json::from_str::<RawSortInput>(&raw) {
            Ok(input) => SortOutput { order: order_channels_from_raw(&input) },
            Err(_) => SortOutput { order: Vec::new() },
        };
        let json = serde_json::to_string(&output).unwrap_or_else(|_| "{\"order\":[]}".to_string());
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgChannelSortNativeBridge_nativeEngineVersion<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
) -> jint {
    ENGINE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(playlist_order: i64, name_lower: &str) -> ChannelIn {
        ChannelIn {
            source_order_index: None,
            playlist_order,
            is_timeshift: false,
            name_lower: name_lower.to_string(),
            family_key: name_lower.to_string(),
            brand_key: name_lower.to_string(),
            plan_rank: None,
            dial_number: None,
            manual_position: None,
            country_priority: 0,
            is_local_or_regional: false,
            watch_order: None,
        }
    }

    // Mirrors EpgChannelSortTest.`respects playlistOrder straight from provider`.
    #[test]
    fn respects_playlist_order() {
        let channels = vec![ch(2, "alpha"), ch(1, "beta"), ch(0, "zebra")];
        let input = SortInput { channels, apply_editorial_overlay: false, plan_category_is_entertainment: false };
        assert_eq!(order_channels(&input), vec![2, 1, 0]);
    }

    // Mirrors EpgChannelSortTest.`playlistOrder then name for ties`.
    #[test]
    fn playlist_order_then_name_for_ties() {
        let channels = vec![ch(5, "bravo 2"), ch(5, "bravo 1"), ch(1, "alpha")];
        let input = SortInput { channels, apply_editorial_overlay: false, plan_category_is_entertainment: false };
        assert_eq!(order_channels(&input), vec![2, 1, 0]);
    }

    // Mirrors EpgChannelSortTest.`sourceOrderById groups sources before within order`.
    #[test]
    fn source_order_groups_before_playlist_order() {
        let mut s1c0 = ch(0, "s1-0");
        s1c0.source_order_index = Some(0);
        let mut s2c5 = ch(5, "s2-5");
        s2c5.source_order_index = Some(1);
        let mut s2c0 = ch(0, "s2-0");
        s2c0.source_order_index = Some(1);
        let channels = vec![s1c0, s2c5, s2c0];
        let input = SortInput { channels, apply_editorial_overlay: false, plan_category_is_entertainment: false };
        assert_eq!(order_channels(&input), vec![0, 2, 1]);
    }

    // Mirrors ChannelCurationSortTest.`same-brand Sky Sports channels cluster together, BBC unaffected`.
    #[test]
    fn same_brand_channels_cluster_together() {
        let mut golf = ch(0, "sky sports golf");
        golf.brand_key = "skysports".into();
        golf.family_key = "skysportsgolf".into();
        let mut news = ch(1, "sky sports news");
        news.brand_key = "skysports".into();
        news.family_key = "skysportsnews".into();
        let mut bbc1 = ch(2, "bbc one");
        bbc1.brand_key = "bbcone".into();
        bbc1.family_key = "bbcone".into();
        let mut bbc2 = ch(3, "bbc two");
        bbc2.brand_key = "bbctwo".into();
        bbc2.family_key = "bbctwo".into();
        let mut sky1 = ch(4, "sky sports 1");
        sky1.brand_key = "skysports".into();
        sky1.family_key = "skysports1".into();
        let mut sky2 = ch(5, "sky sports 2");
        sky2.brand_key = "skysports".into();
        sky2.family_key = "skysports2".into();

        let channels = vec![golf, news, bbc1, bbc2, sky1, sky2];
        let input = SortInput { channels, apply_editorial_overlay: false, plan_category_is_entertainment: false };
        let order = order_channels(&input);

        let sky_positions: Vec<usize> = order
            .iter()
            .enumerate()
            .filter(|(_, &orig)| orig == 0 || orig == 1 || orig == 4 || orig == 5)
            .map(|(pos, _)| pos)
            .collect();
        assert_eq!(sky_positions.len(), 4);
        let min = *sky_positions.iter().min().unwrap();
        let max = *sky_positions.iter().max().unwrap();
        assert_eq!(max - min + 1, 4, "Sky Sports channels must be contiguous");
        assert_eq!(min, 0, "Sky Sports cluster should lead since it appeared first");
    }

    // Mirrors ChannelCurationSortTest.`+1 channel is placed immediately after its parent`.
    #[test]
    fn timeshift_channel_follows_its_parent() {
        let mut itv = ch(0, "itv");
        itv.brand_key = "itv".into();
        itv.family_key = "itv".into();
        let mut bbc = ch(1, "bbc one");
        bbc.brand_key = "bbcone".into();
        bbc.family_key = "bbcone".into();
        let mut itv_plus1 = ch(2, "itv +1");
        itv_plus1.brand_key = "itv".into();
        itv_plus1.family_key = "itv".into();
        itv_plus1.is_timeshift = true;

        let channels = vec![itv, bbc, itv_plus1];
        let input = SortInput { channels, apply_editorial_overlay: false, plan_category_is_entertainment: false };
        let order = order_channels(&input);
        let itv_pos = order.iter().position(|&i| i == 0).unwrap();
        let plus1_pos = order.iter().position(|&i| i == 2).unwrap();
        assert_eq!(itv_pos + 1, plus1_pos);
    }

    // Mirrors ChannelCurationSortTest.`numbered channels in a brand cluster sort ascending, not provider order`.
    #[test]
    fn numbered_channels_sort_ascending_within_brand() {
        let mk = |playlist: i64, family: &str| {
            let mut c = ch(playlist, family);
            c.brand_key = "tntsports".into();
            c.family_key = family.to_string();
            c
        };
        let channels = vec![
            mk(0, "tntsports3"),
            mk(1, "tntsports1"),
            mk(2, "tntsports5"),
            mk(3, "tntsports2"),
        ];
        let input = SortInput { channels, apply_editorial_overlay: false, plan_category_is_entertainment: false };
        let order = order_channels(&input);
        assert_eq!(order, vec![1, 3, 0, 2]);
    }

    // Mirrors EpgChannelSortTest.`manualPositions take precedence over plan rank`.
    #[test]
    fn manual_position_takes_precedence() {
        let mut sky = ch(0, "sky sports main event");
        sky.plan_rank = Some(5);
        sky.manual_position = Some(1);
        let mut bbc = ch(1, "bbc news");
        bbc.plan_rank = Some(1);
        bbc.manual_position = Some(0);

        let channels = vec![sky, bbc];
        let input = SortInput { channels, apply_editorial_overlay: true, plan_category_is_entertainment: false };
        assert_eq!(order_channels(&input), vec![1, 0]);
    }

    #[test]
    fn single_and_empty_lists_pass_through() {
        let input = SortInput { channels: vec![], apply_editorial_overlay: false, plan_category_is_entertainment: false };
        assert_eq!(order_channels(&input), Vec::<usize>::new());
        let input = SortInput { channels: vec![ch(0, "solo")], apply_editorial_overlay: false, plan_category_is_entertainment: false };
        assert_eq!(order_channels(&input), vec![0]);
    }

    #[test]
    fn country_priority_tiers_before_plan_rank() {
        let mut home = ch(0, "bbc news");
        home.plan_rank = Some(5);
        home.country_priority = 0;
        let mut peer = ch(1, "cnn");
        peer.plan_rank = Some(1);
        peer.country_priority = 1;
        let mut foreign = ch(2, "ard");
        foreign.plan_rank = Some(0);
        foreign.country_priority = 2;

        let channels = vec![foreign, peer, home];
        let input = SortInput { channels, apply_editorial_overlay: true, plan_category_is_entertainment: false };
        assert_eq!(order_channels(&input), vec![2, 1, 0]);
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 3);
    }

    #[test]
    fn watch_lane_lifts_watched_channels_above_the_plan() {
        // Plan says a(1) then b(2) then c(3); the household watches c.
        let mut a = ch(0, "a");
        a.plan_rank = Some(1);
        let mut b = ch(1, "b");
        b.plan_rank = Some(2);
        let mut c = ch(2, "c");
        c.plan_rank = Some(3);
        c.watch_order = Some(0);

        let input = SortInput {
            channels: vec![a, b, c],
            apply_editorial_overlay: true,
            plan_category_is_entertainment: false,
        };
        // c leads despite the worst plan rank -- this is the Fox-News-in-the-UK case.
        assert_eq!(order_channels(&input), vec![2, 0, 1]);
    }

    #[test]
    fn watch_lane_is_densified_per_tab_not_taken_raw() {
        // Household ranks: this tab holds only #7 and #4, both far down the
        // global list. They must still form this tab's lane, in their relative
        // order -- before the fix a tab like this got no lane at all.
        let mut a = ch(0, "a");
        a.plan_rank = Some(1);
        let mut b = ch(1, "b");
        b.plan_rank = Some(2);
        b.watch_order = Some(7);
        let mut c = ch(2, "c");
        c.plan_rank = Some(3);
        c.watch_order = Some(4);

        let input = SortInput {
            channels: vec![a, b, c],
            apply_editorial_overlay: true,
            plan_category_is_entertainment: false,
        };
        assert_eq!(order_channels(&input), vec![2, 1, 0]);
    }

    #[test]
    fn watch_lane_is_capped_so_it_cannot_reorder_a_whole_tab() {
        // Six watched channels, cap is five: the sixth stays in plan order.
        let channels: Vec<ChannelIn> = (0..7)
            .map(|i| {
                let mut c = ch(i, &format!("c{}", i));
                // Plan order is the reverse of watch order, so any channel in the
                // lane is visibly out of plan position.
                c.plan_rank = Some(100 - i);
                if i < 6 {
                    c.watch_order = Some(i);
                }
                c
            })
            .collect();

        let input = SortInput {
            channels,
            apply_editorial_overlay: true,
            plan_category_is_entertainment: false,
        };
        let order = order_channels(&input);
        // First five are the lane, in watch order.
        assert_eq!(&order[..5], &[0, 1, 2, 3, 4]);
        // Channel 5 was watched but missed the cap, so it falls back to the plan
        // tail and sorts behind the unwatched-but-better-ranked channel 6.
        assert_eq!(&order[5..], &[6, 5]);
    }

    // ── Raw-input entry point (Phase 3.2) parity tests ──────────────────────
    // These call `order_channels_from_raw` with only raw channel names -- no
    // precomputed isTimeshift/familyKey/brandKey/planRank/dialNumber -- and check
    // it reproduces the same ordering the precomputed-input path makes.

    fn raw_ch(playlist_order: i64, name: &str) -> RawChannelIn {
        RawChannelIn {
            source_order_index: None,
            playlist_order,
            name: name.to_string(),
            playlist_channel_id: None,
            manual_position: None,
            country_priority: 0,
            is_local_or_regional: false,
            watch_order: None,
        }
    }

    #[test]
    fn raw_entry_point_respects_playlist_order() {
        let channels = vec![raw_ch(2, "Alpha"), raw_ch(1, "Beta"), raw_ch(0, "Zebra")];
        let input = RawSortInput {
            channels,
            apply_editorial_overlay: false,
            plan_category_is_entertainment: false,
            plan_country: None,
            plan_category: None,
        };
        assert_eq!(order_channels_from_raw(&input), vec![2, 1, 0]);
    }

    // "ITV" and "ITV +1" must derive isTimeshift/familyKey from the raw name and
    // place the timeshift variant immediately after its parent.
    #[test]
    fn raw_entry_point_derives_timeshift_ordering_from_name() {
        let channels = vec![raw_ch(0, "ITV +1"), raw_ch(1, "BBC One"), raw_ch(2, "ITV")];
        let input = RawSortInput {
            channels,
            apply_editorial_overlay: false,
            plan_category_is_entertainment: false,
            plan_country: None,
            plan_category: None,
        };
        let order = order_channels_from_raw(&input);
        let itv_pos = order.iter().position(|&i| i == 2).unwrap();
        let plus1_pos = order.iter().position(|&i| i == 0).unwrap();
        assert_eq!(itv_pos + 1, plus1_pos);
    }

    #[test]
    fn raw_entry_point_manual_position_takes_precedence() {
        let mut sky = raw_ch(0, "Sky Sports Main Event");
        sky.manual_position = Some(1);
        let mut bbc = raw_ch(1, "BBC News");
        bbc.manual_position = Some(0);
        let input = RawSortInput {
            channels: vec![sky, bbc],
            apply_editorial_overlay: true,
            plan_category_is_entertainment: false,
            plan_country: None,
            plan_category: None,
        };
        assert_eq!(order_channels_from_raw(&input), vec![1, 0]);
    }

    #[test]
    fn raw_entry_point_single_and_empty_lists_pass_through() {
        let input = RawSortInput {
            channels: vec![],
            apply_editorial_overlay: false,
            plan_category_is_entertainment: false,
            plan_country: None,
            plan_category: None,
        };
        assert_eq!(order_channels_from_raw(&input), Vec::<usize>::new());
        let input = RawSortInput {
            channels: vec![raw_ch(0, "Solo")],
            apply_editorial_overlay: false,
            plan_category_is_entertainment: false,
            plan_country: None,
            plan_category: None,
        };
        assert_eq!(order_channels_from_raw(&input), vec![0]);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
