//! Native channel-grouping engine for Aurelis's `core/domain` `ChannelArbiter`.
//!
//! Ports `buildChannelGroups`/`shouldCombineStripped`/`extractNumbers`/
//! `levenshteinDistance` (`ChannelArbiter.kt`) line-for-line. Two JNI entry
//! points share this pure grouping core:
//!
//! - `nativeBuildGroups` -- takes every Aurelis-specific heuristic as a
//!   caller-precomputed primitive (`normalizedName`, `strippedName`,
//!   `countryCode`, `tzFeed`, `isUhd`, `category`). Kept for the Kotlin
//!   fallback-parity path and as the precomputed-input half of the parity
//!   tests.
//! - `nativeBuildGroupsFromRaw` -- takes only raw `{name, genre}` per channel
//!   and computes every precomputed field itself via `aurelis-heuristics`,
//!   `channel-quality-score`, and `regional-channel-filter` (all same-workspace
//!   Rust library calls, no JNI/marshalling involved). This is what
//!   `ChannelArbiter.unify()` calls: it collapses the ~6 small JNI calls per
//!   channel Kotlin used to make to precompute these fields into zero --
//!   the whole precompute-then-group pipeline happens in one JNI round trip.
//!
//! Owns no long-lived state: one call receives the whole channel list and
//! returns the grouping.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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
pub const ENGINE_VERSION: i32 = 3;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelIn {
    /// Raw provider name -- only used for `extractNumbers`/word-splitting/the
    /// all-empty-normalized-key fallback, mirroring `LocalChannel.name`'s sole
    /// use inside `buildChannelGroups`.
    pub name: String,
    pub normalized_name: String,
    pub stripped_name: String,
    pub country_code: Option<String>,
    pub tz_feed: bool,
    /// Kotlin's `uhdFlags` parameter defaults to `emptyList()` (a call-site
    /// convenience for tests/callers that don't care about UHD), which flips
    /// the Phase-A key and the `uhdCompatible` guard to always-true. This
    /// port assumes the flag is always populated -- the only way `unify()`
    /// actually calls it in production -- so `is_uhd` is a plain `bool`
    /// rather than modelling the "flags list omitted entirely" case.
    pub is_uhd: bool,
    /// Broadcast region detected in the name ("Wales", "Scotland", ...), or `None` for a
    /// national feed. A grouping discriminator, not a label.
    ///
    /// `normalize_for_match` deliberately strips the regional suffix so `BBC One Wales` and
    /// `UK: BBC ONE WALES FHD` share a key -- which is right, and is what lets the same feed
    /// from two providers combine. But with nothing putting the region back, EVERY BBC One
    /// region shared that key too: real device data grouped 22 channels (Wales, Scotland,
    /// Northern Ireland, Yorkshire, Channel Islands...) into one, so choosing "stream 7 of 22"
    /// could put a different nation on screen.
    #[serde(default)]
    pub region: Option<String>,
    /// Mapped `ContentCategory` name (e.g. "SPORT", "OTHERS"). Mirrors
    /// Kotlin's `categoryKeys` parameter, which defaults to `emptyList()` --
    /// modelled here as `None` per-channel (via serde's missing-field
    /// default) rather than a list-level flag, since JSON has no notion of
    /// "this whole array was omitted". `None` behaves exactly like
    /// `categoryKeys.isEmpty()` did in Kotlin: the category guard becomes a
    /// no-op and every pair is category-compatible.
    #[serde(default)]
    pub category: Option<String>,
    /// "That's TV (UK)" on a tuner's own lineup: a different service from "That's TV", not a
    /// territory. Set by `precompute_channel_in` from the raw name and genre; see
    /// `has_tuner_service_tag`, the twin of `ChannelArbiter.hasTunerServiceTag`.
    #[serde(default)]
    pub tuner_service_tag: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInput {
    pub channels: Vec<ChannelIn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupOut {
    pub stable_key: String,
    /// Sorted indices into the input `channels` list.
    pub member_indices: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupOutput {
    pub groups: Vec<GroupOut>,
}

fn number_words() -> &'static HashMap<&'static str, &'static str> {
    static MAP: std::sync::OnceLock<HashMap<&'static str, &'static str>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        HashMap::from([
            ("one", "1"), ("two", "2"), ("three", "3"), ("four", "4"), ("five", "5"),
            ("six", "6"), ("seven", "7"), ("eight", "8"), ("nine", "9"), ("ten", "10"),
            ("first", "1"), ("second", "2"), ("third", "3"),
        ])
    })
}

/// Mirrors `extractNumbers` (ChannelArbiter.kt): lowercase, split on non-alnum
/// runs, keep digit-only tokens as-is, map number words, drop everything else.
fn extract_numbers(s: &str) -> Vec<String> {
    let lower = s.to_lowercase();
    let words = number_words();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .filter_map(|token| {
            if token.chars().all(|c| c.is_ascii_digit()) {
                Some(token.to_string())
            } else {
                words.get(token).map(|s| s.to_string())
            }
        })
        .collect()
}

/// Mirrors `levenshteinDistance` (ChannelArbiter.kt): two-row optimization.
fn levenshtein_distance(s1: &str, s2: &str) -> usize {
    let a: Vec<char> = s1.chars().collect();
    let b: Vec<char> = s2.chars().collect();
    let (m, n) = (a.len(), b.len());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];
    for i in 1..=m {
        curr[0] = i;
        let c1 = a[i - 1];
        for j in 1..=n {
            let cost = if c1 == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Mirrors `levenshteinSimilarity` (ChannelArbiter.kt).
fn levenshtein_similarity(s1: &str, s2: &str) -> f64 {
    let distance = levenshtein_distance(s1, s2);
    let max_len = s1.chars().count().max(s2.chars().count());
    if max_len == 0 {
        1.0
    } else {
        1.0 - (distance as f64 / max_len as f64)
    }
}

/// True when a category mismatch should BLOCK the merge. Mirrors
/// `shouldCombineStripped`'s category guard (ChannelArbiter.kt): categories that
/// differ only block the merge unless one side is a generic catch-all
/// (OTHERS/ENTERTAINMENT) AND the stripped names are exact or near-identical
/// (>= 0.85 similarity). `None` on either side means category-checking is
/// disabled for this call (mirrors `categoryKeys.isEmpty()`), so it never blocks.
fn category_blocks_merge(
    category1: &Option<String>,
    category2: &Option<String>,
    stripped1: &str,
    stripped2: &str,
) -> bool {
    let (Some(c1), Some(c2)) = (category1, category2) else {
        return false;
    };
    if c1 == c2 {
        return false;
    }
    let one_is_generic =
        c1 == "OTHERS" || c2 == "OTHERS" || c1 == "ENTERTAINMENT" || c2 == "ENTERTAINMENT";
    let exact_or_high_similarity =
        stripped1 == stripped2 || levenshtein_similarity(stripped1, stripped2) >= 0.85;
    !one_is_generic || !exact_or_high_similarity
}

/// Names whose "+" is the word "and", spelt without spaces. Mirrors `ChannelArbiter.PLUS_MEANS_AND`.
const PLUS_MEANS_AND: [&str; 3] = ["crime+inv", "sci-fi+horror", "scifi+horror"];

/// True when `name` carries a service-plus marker.
///
/// Mirrors `ChannelArbiter.hasServicePlusMarker`. A "+" on a brand marks a DIFFERENT SERVICE,
/// not a decoration: Sky Sports+ is the overflow event bank, not Sky Sports; ESPN+ and
/// Sportsnet+ are streaming services sitting alongside the channels they are named after. Both
/// derived keys have already lost the marker by the time the merge sees them --
/// `normalize_for_match` drops the "+" and `stripQualityAndRegional` removes the word "plus" as
/// a flavour tag -- so this reads the RAW provider name, where it survives.
///
/// The `+1` timeshift case is not this: it has its own discriminator, and both sides of such a
/// pair carry a plus anyway, so the two rules agree rather than compete.
///
/// A "+" that JOINS two words is "and", not a marker: "Crime+Inv" is Crime & Investigation, and
/// on `.246` it sat in a row of its own beside the other providers' copy (8 Oct 2026). Only the
/// names in `PLUS_MEANS_AND` are read that way -- "CANAL+SPORT" is written the same and its plus
/// IS the brand.
pub fn has_service_plus_marker(name: &str) -> bool {
    let lower = name.to_lowercase();
    let mut compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    for phrase in PLUS_MEANS_AND {
        compact = compact.replace(phrase, "");
    }
    if compact.contains('+') || compact.contains('⁺') {
        return true;
    }
    // The word "plus" standing alone -- not "Plusnet", not "Surplus".
    lower.match_indices("plus").any(|(at, _)| {
        let before_ok = at == 0
            || !lower[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric());
        let after = at + "plus".len();
        let after_ok = after >= lower.len()
            || !lower[after..].chars().next().is_some_and(|c| c.is_alphanumeric());
        before_ok && after_ok
    })
}

/// The genre every HDHomeRun lineup channel is imported under.
const TUNER_GENRE: &str = "HDHomeRun";

/// Mirrors `ChannelArbiter.hasTunerServiceTag`. A bracketed country tag in a TUNER's own lineup
/// name -- "That's TV (UK)" on 56 beside "That's TV" on 8 -- names a different service, not a
/// territory: one aerial serves one country, so the broadcaster has no reason to label the same
/// service twice. On a provider's list the same tag is territory noise and still merges, so only
/// a tuner's channels are read. Hand-rolled to match Kotlin's `[(\[]\s*(uk|gb)\s*[)\]]`.
pub fn has_tuner_service_tag(name: &str, genre: &str) -> bool {
    if !genre.trim().eq_ignore_ascii_case(TUNER_GENRE) {
        return false;
    }
    let chars: Vec<char> = name.to_lowercase().chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != '(' && c != '[' {
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        let start = j;
        while j < chars.len() && chars[j].is_ascii_alphabetic() {
            j += 1;
        }
        let word: String = chars[start..j].iter().collect();
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if (word == "uk" || word == "gb") && j < chars.len() && (chars[j] == ')' || chars[j] == ']') {
            return true;
        }
    }
    false
}

/// Mirrors `ChannelArbiter.hasSiblingMarker`. "Extra" names a sibling channel, not a stream:
/// E4 Extra, Your TV Extra, BBC Radio 4 Extra (listed as "BBC Radio 4 Ex"). The stripped key
/// loses the word as a flavour tag, so "e4 extra" compared as "e4" and merged; this reads the RAW
/// name, where it survives.
pub fn has_sibling_marker(name: &str) -> bool {
    name.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| t == "extra" || t == "xtra" || t == "ex")
}

/// True when `stripped` holds a token mixing letters and digits -- a channel designation
/// ("f1", "u21", "t20") rather than a decoration. Mirrors `ChannelArbiter.hasMixedAlnumToken`.
fn has_mixed_alnum_token(stripped: &str) -> bool {
    stripped
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| t.chars().count() >= 2 && t.chars().any(|c| c.is_ascii_digit()) && t.chars().any(|c| c.is_alphabetic()))
}

/// `stripped` with every non-alphanumeric run removed, for the spacing-variant exemption.
fn squash_alnum(stripped: &str) -> String {
    stripped.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// The letter-and-digit tokens of `stripped`, sorted. Mirrors `ChannelArbiter.mixedAlnumTokens`.
fn mixed_alnum_tokens(stripped: &str) -> Vec<String> {
    let mut tokens: Vec<String> = stripped
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| {
            t.chars().count() >= 2 && t.chars().any(|c| c.is_ascii_digit()) && t.chars().any(|c| c.is_alphabetic())
        })
        .map(|t| t.to_string())
        .collect();
    tokens.sort();
    tokens
}

#[allow(clippy::too_many_arguments)]
/// Mirrors `shouldCombineStripped` (ChannelArbiter.kt) guard-for-guard, in order.
/// Groups of stripped names that mean one channel, keyed by ISO-3166 country.
///
/// Twin of `core/common`'s `ChannelBrandEquivalence`. On a British dial the channel branded
/// CNN IS CNN International, and CNBC IS CNBC Europe. Country-scoped so that
/// `US: CNN INTERNATIONAL` and `US: CNN HD` — genuinely different channels — stay apart.
///
/// Keys match `strip_quality_and_regional` output: lower-case, prefix and quality tags
/// removed, single-spaced.
fn brand_equivalence_groups(country: &str) -> &'static [&'static [&'static str]] {
    match country {
        "GB" => &[
            &["cnn", "cnn international"],
            &["cnbc", "cnbc europe"],
            &[
                "horror channel",
                "horror xtra",
                "horror channel +1",
                "horror xtra +1",
            ],
            &["bbc red button", "bbc rb1", "bbc rb 1", "bbc red button 1"],
            &["sky arts", "sky arts 1"],
            &[
                "itv1 wales",
                "itv 1 wales",
                "itv wales",
                "itv cymru wales",
                "itv1 cymru wales",
            ],
        ],
        _ => &[],
    }
}

/// True when two stripped names are the same channel in a country both belong to.
///
/// Both countries must be known and equal — an unknown country is never a match, because the
/// equivalence is a fact about a specific line-up and guessing would reintroduce exactly the
/// cross-region merging this sits beside.
fn same_channel_brand(
    stripped1: &str,
    stripped2: &str,
    country1: &Option<String>,
    country2: &Option<String>,
) -> bool {
    fn norm(s: &str) -> String {
        s.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
    }
    let a = norm(stripped1);
    let b = norm(stripped2);
    if a.is_empty() || b.is_empty() || a == b {
        return false;
    }
    let (c1, c2) = match (country1, country2) {
        (Some(c1), Some(c2)) => (c1.trim().to_uppercase(), c2.trim().to_uppercase()),
        _ => return false,
    };
    if c1.is_empty() || c1 != c2 {
        return false;
    }
    brand_equivalence_groups(&c1)
        .iter()
        .any(|group| group.contains(&a.as_str()) && group.contains(&b.as_str()))
}

fn should_combine_stripped(
    stripped1: &str,
    stripped2: &str,
    numbers1: &[String],
    numbers2: &[String],
    words1: &[String],
    words2: &[String],
    words1_set: &HashSet<String>,
    words2_set: &HashSet<String>,
    country1: &Option<String>,
    country2: &Option<String>,
    tz_feed1: bool,
    tz_feed2: bool,
    category1: &Option<String>,
    category2: &Option<String>,
    region1: &Option<String>,
    region2: &Option<String>,
    timeshift1: bool,
    timeshift2: bool,
    service_plus1: bool,
    service_plus2: bool,
    sibling1: bool,
    sibling2: bool,
    tuner_tag1: bool,
    tuner_tag2: bool,
) -> bool {
    if stripped1.is_empty() || stripped2.is_empty() {
        return false;
    }

    // Tuner service tag: on one aerial "That's TV (UK)" is not "That's TV". Never exempted by an
    // equivalence, unlike the sibling rule below. See `has_tuner_service_tag`.
    if tuner_tag1 != tuner_tag2 {
        return false;
    }

    // Service-plus discriminator: Sky Sports+ is not Sky Sports. See `has_service_plus_marker`.
    if service_plus1 != service_plus2 {
        return false;
    }

    // Sibling discriminator: E4 Extra is not E4. See `has_sibling_marker`. An explicit
    // equivalence still wins (the owner's Horror Channel / Horror Xtra), as in Kotlin.
    if sibling1 != sibling2 && !same_channel_brand(stripped1, stripped2, country1, country2) {
        return false;
    }

    // Timeshift discriminator: a "+1" feed is a different channel from its parent.
    // Phase A never groups them (`normalize_for_match` turns the marker into its own
    // "tshift1" word), but the names compared HERE are the *stripped* ones, which still
    // carry a bare "+1". On a long parent name that difference clears every other guard:
    // "national geographic +1" vs "national geographic" is inside the 35% length check
    // and its 0.86 similarity clears the 0.85 bar the number-mismatch bypass demands --
    // so the +1 merged into its parent and vanished from the guide. Short names were only
    // ever saved by the length check, i.e. by accident of being short.
    if timeshift1 != timeshift2 {
        return false;
    }

    // Region discriminator: two different broadcast regions are two different channels, and a
    // regional feed is not the national one either. Same region from two providers still
    // merges -- that is the whole point, and the case this must not break.
    //
    // Skipped entirely for US timezone feeds. `detect_region_in_name` reads the "(East)" in
    // "TNT (East)" as a region, but that is a timezone feed, not a broadcast region, and
    // `tz_feed` already models it correctly -- applying both split feeds that are deliberately
    // merged.
    if !tz_feed1 && !tz_feed2 && region1 != region2 {
        return false;
    }

    // Category discriminator: never merge across a mapped ContentCategory
    // boundary, unless one side is a generic catch-all and the names are
    // exact/near-identical. Checked before country/tz -- mirrors Kotlin's guard
    // order in `shouldCombineStripped`.
    if category_blocks_merge(category1, category2, stripped1, stripped2) {
        return false;
    }

    // Timezone-feed discriminator: a US (East)/(West) feed must not merge with a
    // non-timezone variant of the same base name.
    if tz_feed1 != tz_feed2 {
        return false;
    }

    // Country discriminator: never merge two channels whose detected country
    // differs by NAME.
    if let (Some(c1), Some(c2)) = (country1, country2) {
        if c1 != c2 {
            return false;
        }
    }

    if stripped1 == stripped2 {
        return true;
    }

    // One service under two names, in one country. Twin of
    // `ChannelBrandEquivalence.sameChannel`; see that file for the reasoning.
    //
    // AFTER every hard discriminator, so it can never rescue a "+1", a regional variant or a
    // foreign feed. BEFORE the length check, because that is what rejects these pairs:
    // "cnn" against "cnn international" is a 13-character difference against a 5.6 budget.
    if same_channel_brand(stripped1, stripped2, country1, country2) {
        return true;
    }

    // Designator discriminator: a token mixing letters and digits names the channel, and two
    // different ones are two channels. The number guard never sees them (`extract_numbers`
    // counts only all-digit tokens) and the word guard waves them through ("itv2" is 0.75 like
    // "itv3"), so ITV2/3/4 merged into one channel on the aerial lineup and the four ITV +1s into
    // another (11 Sep 2026). Spacing variants are exempt: "ITV1" and "ITV 1" squash to one string.
    if mixed_alnum_tokens(stripped1) != mixed_alnum_tokens(stripped2) && squash_alnum(stripped1) != squash_alnum(stripped2)
    {
        return false;
    }

    // Fast length check: prune most pairs immediately.
    let len1 = stripped1.chars().count();
    let len2 = stripped2.chars().count();
    let max_len = len1.max(len2);
    if (len1 as isize - len2 as isize).unsigned_abs() as f64 > 0.35 * max_len as f64 {
        return false;
    }

    // Computed once, used both by the number-mismatch bypass below and the final
    // fuzzy-match decision.
    let similarity = levenshtein_similarity(stripped1, stripped2);

    // Number mismatch safety, with the BBC-One-style fallback. The bypass is narrowed to
    // names that are ALREADY near-identical (>= 0.85), matching Kotlin: as a blanket
    // bypass it let unrelated no-number-vs-"1" pairs through at the general 0.65
    // threshold. This guard had been dropped on the way over, so the native engine --
    // the one that actually runs on device -- was the looser of the two.
    //
    // Narrowed again, because "no number at all" was the wrong reading of a name like
    // "Sky Sports F1": `extract_numbers` counts only tokens that are ENTIRELY digits, so "f1"
    // registered as no number, the pair looked like the BBC One/BBC 1 shape, and
    // "sky sports f1" vs "sky sports 1" clears 0.85 on a single edit. The word-mismatch net
    // below cannot catch it either -- it drops tokens under three characters, so "f1" and "1"
    // are never compared. On the real lineup that merged Sky Sports F1 into New Zealand's Sky
    // Sport 1, and through it into the Sky Sports+ event bank.
    //
    // A token mixing letters and digits (f1, u21, t20) IS the channel's designation, so a
    // number mismatch alongside one is a real difference. Spacing variants are exempt: "ITV1"
    // vs "ITV 1" also mismatch on numbers and also carry a mixed token, but they are the same
    // string once the spaces go, which is the one case the bypass exists for.
    if numbers1 != numbers2 {
        let is_bbc_one_fallback = (numbers1.is_empty() && numbers2 == ["1".to_string()])
            || (numbers2.is_empty() && numbers1 == ["1".to_string()]);
        let designator_differs = squash_alnum(stripped1) != squash_alnum(stripped2)
            && (has_mixed_alnum_token(stripped1) || has_mixed_alnum_token(stripped2));
        if !is_bbc_one_fallback || designator_differs || similarity < 0.85 {
            return false;
        }
    }

    // Word mismatch safety: every significant word (len >= 3) must have a match
    // (exact or fuzzy >= 0.70) in the other name, both directions.
    for w1 in words1 {
        if words2_set.contains(w1) {
            continue;
        }
        if !words2.iter().any(|w2| w1 == w2 || levenshtein_similarity(w1, w2) >= 0.70) {
            return false;
        }
    }
    for w2 in words2 {
        if words1_set.contains(w2) {
            continue;
        }
        if !words1.iter().any(|w1| w1 == w2 || levenshtein_similarity(w1, w2) >= 0.70) {
            return false;
        }
    }

    similarity >= 0.65
}

/// Mirrors `buildChannelGroups` (ChannelArbiter.kt) end to end: Phase A exact
/// bucketing by composite key, Phase B deterministic-order greedy star-merge,
/// then final assembly with the same `stableKey` construction.
pub fn build_channel_groups(channels: &[ChannelIn]) -> Vec<GroupOut> {
    let n = channels.len();
    let extracted_numbers: Vec<Vec<String>> = channels.iter().map(|c| extract_numbers(&c.name)).collect();
    let significant_words: Vec<Vec<String>> = channels
        .iter()
        .map(|c| {
            c.stripped_name
                .split(' ')
                .filter(|w| w.len() >= 3)
                .map(|w| w.to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    let significant_word_sets: Vec<HashSet<String>> =
        significant_words.iter().map(|words| words.iter().cloned().collect()).collect();
    // Read off the normalized key rather than re-running the timeshift regex per channel:
    // `normalize_for_match` has already tokenised the marker, and this sweep is O(clusters²).
    let timeshift_flags: Vec<bool> = channels
        .iter()
        .map(|c| {
            c.normalized_name
                .contains(aurelis_heuristics::channel_name_keys::TIMESHIFT_TOKEN)
        })
        .collect();
    // Read off the RAW provider name: both derived keys have already lost the marker.
    // See `has_service_plus_marker`.
    let service_plus_flags: Vec<bool> =
        channels.iter().map(|c| has_service_plus_marker(&c.name)).collect();
    // Also off the raw name: the stripped key loses "extra". See `has_sibling_marker`.
    let sibling_flags: Vec<bool> = channels.iter().map(|c| has_sibling_marker(&c.name)).collect();
    // Off the raw name and genre, stamped by `precompute_channel_in`: the normalized key drops
    // the "(UK)". See `has_tuner_service_tag`.
    let tuner_tag_flags: Vec<bool> = channels.iter().map(|c| c.tuner_service_tag).collect();

    // Phase A — cluster channel indices by normalized key.
    let mut cluster_by_norm: HashMap<String, usize> = HashMap::new();
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    for i in 0..n {
        let base_nk = &channels[i].normalized_name;
        if base_nk.is_empty() {
            clusters.push(vec![i]);
        } else {
            let mut nk = base_nk.clone();
            if let Some(c) = &channels[i].country_code {
                nk.push_str("|c:");
                nk.push_str(c);
            }
            if channels[i].tz_feed {
                nk.push_str("|tz");
            }
            if channels[i].is_uhd {
                nk.push_str("|uhd");
            }
            if let Some(cat) = &channels[i].category {
                nk.push_str("|cat:");
                nk.push_str(cat);
            }
            if let Some(r) = &channels[i].region {
                nk.push_str("|r:");
                nk.push_str(r);
            }
            // Service-plus discriminator: "SkySp+ 1" and "SkySp 1" normalise to the same key,
            // and clustering them handed New Zealand's Sky Sport channels the Sky Sports+
            // event bank's group. See `has_service_plus_marker`.
            if service_plus_flags[i] {
                nk.push_str("|plus");
            }
            if tuner_tag_flags[i] {
                nk.push_str("|ttag");
            }
            match cluster_by_norm.get(&nk) {
                Some(&idx) => clusters[idx].push(i),
                None => {
                    cluster_by_norm.insert(nk, clusters.len());
                    clusters.push(vec![i]);
                }
            }
        }
    }

    // Phase B — greedy "star" merge of clusters whose representatives fuzzy-match,
    // processed in deterministic order (smallest normalized key / name first).
    let mut order: Vec<usize> = (0..clusters.len()).collect();
    order.sort_by(|&a, &b| {
        let key_a = {
            let rep = clusters[a][0];
            let nk = &channels[rep].normalized_name;
            if nk.is_empty() { channels[rep].name.as_str() } else { nk.as_str() }
        };
        let key_b = {
            let rep = clusters[b][0];
            let nk = &channels[rep].normalized_name;
            if nk.is_empty() { channels[rep].name.as_str() } else { nk.as_str() }
        };
        key_a.cmp(key_b)
    });

    let mut merge_into: Vec<usize> = (0..clusters.len()).collect();
    let mut merged_away = vec![false; clusters.len()];

    for ai in 0..order.len() {
        let a = order[ai];
        if merged_away[a] {
            continue;
        }
        let rep_a = clusters[a][0];
        for bi in (ai + 1)..order.len() {
            let b = order[bi];
            if merged_away[b] {
                continue;
            }
            let rep_b = clusters[b][0];

            let n_a = &channels[rep_a].normalized_name;
            let c_a = &channels[rep_a].country_code;
            let c_b = &channels[rep_b].country_code;
            let same_known_country = c_a.is_some() && c_a == c_b;
            let country_compatible = c_a.is_none() || c_b.is_none() || c_a == c_b;
            let tz_compatible = channels[rep_a].tz_feed == channels[rep_b].tz_feed || same_known_country;
            let uhd_compatible = channels[rep_a].is_uhd == channels[rep_b].is_uhd;
            let cat_a = &channels[rep_a].category;
            let cat_b = &channels[rep_b].category;
            // Mirrors Kotlin's `categoryCompatible`: no category info, equal
            // categories, or either side is a generic catch-all.
            let category_compatible = match (cat_a, cat_b) {
                (None, _) | (_, None) => true,
                (Some(a), Some(b)) => {
                    a == b || a == "OTHERS" || b == "OTHERS" || a == "ENTERTAINMENT" || b == "ENTERTAINMENT"
                }
            };
            // Two different broadcast regions are two different channels. This has to gate
            // `norm_key_match` as well as `should_combine_stripped`, because a normalized-key
            // match short-circuits the fuzzy path entirely -- and the normalized key is exactly
            // where the region was stripped out, so every region matched every other one.
            let region_compatible = channels[rep_a].tz_feed
                || channels[rep_b].tz_feed
                || channels[rep_a].region == channels[rep_b].region;
            // Same argument for the service-plus marker, and for the same reason: the
            // normalized key is exactly where the "+" was dropped, so without this gate
            // "SkySp+ 1" rejoins "SkySp 1" here having just been kept apart in Phase A.
            let service_plus_compatible = service_plus_flags[rep_a] == service_plus_flags[rep_b];
            // And for a tuner's service tag, for the same reason: the key dropped the "(UK)".
            let tuner_tag_compatible = tuner_tag_flags[rep_a] == tuner_tag_flags[rep_b];
            let norm_key_match = !n_a.is_empty()
                && *n_a == channels[rep_b].normalized_name
                && tz_compatible
                && country_compatible
                && uhd_compatible
                && category_compatible
                && region_compatible
                && service_plus_compatible
                && tuner_tag_compatible;

            let should_merge = category_compatible
                && region_compatible
                && (norm_key_match || uhd_compatible)
                && (norm_key_match
                    || should_combine_stripped(
                        &channels[rep_a].stripped_name,
                        &channels[rep_b].stripped_name,
                        &extracted_numbers[rep_a],
                        &extracted_numbers[rep_b],
                        &significant_words[rep_a],
                        &significant_words[rep_b],
                        &significant_word_sets[rep_a],
                        &significant_word_sets[rep_b],
                        c_a,
                        c_b,
                        channels[rep_a].tz_feed,
                        channels[rep_b].tz_feed,
                        cat_a,
                        cat_b,
                        &channels[rep_a].region,
                        &channels[rep_b].region,
                        timeshift_flags[rep_a],
                        timeshift_flags[rep_b],
                        service_plus_flags[rep_a],
                        service_plus_flags[rep_b],
                        sibling_flags[rep_a],
                        sibling_flags[rep_b],
                        tuner_tag_flags[rep_a],
                        tuner_tag_flags[rep_b],
                    ));

            if should_merge {
                merged_away[b] = true;
                merge_into[b] = a;
            }
        }
    }

    // Assemble: collect members per merge target, key by smallest normalized key.
    let mut by_root: Vec<Vec<usize>> = vec![Vec::new(); clusters.len()];
    for c in 0..clusters.len() {
        let root = merge_into[c];
        by_root[root].extend(clusters[c].iter().copied());
    }

    let mut groups = Vec::new();
    for members in by_root {
        if members.is_empty() {
            continue;
        }
        let mut sorted = members;
        sorted.sort_unstable();

        let base_key = sorted
            .iter()
            .filter_map(|&i| {
                let nk = &channels[i].normalized_name;
                if nk.is_empty() { None } else { Some(nk.clone()) }
            })
            .min()
            .unwrap_or_else(|| sorted.iter().map(|&i| channels[i].name.clone()).min().unwrap());

        let mut stable_key = base_key;
        if let Some(country) = sorted.iter().find_map(|&i| channels[i].country_code.clone()) {
            stable_key.push_str("|c:");
            stable_key.push_str(&country);
        }
        if sorted.iter().any(|&i| channels[i].tz_feed) {
            stable_key.push_str("|tz");
        }
        if sorted.iter().any(|&i| channels[i].is_uhd) {
            stable_key.push_str("|uhd");
        }
        // Mirrors Kotlin: keyed off the FIRST sorted member's category (not
        // "any member has X" like tz/uhd above).
        if let Some(cat) = &channels[sorted[0]].category {
            stable_key.push_str("|cat:");
            stable_key.push_str(cat);
        }
        // Region MUST be part of the stable key, not just the cluster key. The caller turns
        // this list into a map keyed by stable_key, so two groups sharing one key silently
        // collapse into one and the channels of whichever loses are simply gone -- and once
        // regions stopped merging, every BBC One region produced a group keyed `bbc1|c:GB`.
        if let Some(r) = &channels[sorted[0]].region {
            stable_key.push_str("|r:");
            stable_key.push_str(r);
        }
        // ...and the service-plus marker, for exactly the reason above: "SkySp+ 1" and
        // "SkySp 1" share a base normalized key, so without this the two clusters the
        // discriminator just kept apart collide here and one of them is dropped.
        if sorted.iter().any(|&i| service_plus_flags[i]) {
            stable_key.push_str("|plus");
        }
        if sorted.iter().any(|&i| tuner_tag_flags[i]) {
            stable_key.push_str("|ttag");
        }

        groups.push(GroupOut { stable_key, member_indices: sorted });
    }
    groups
}

fn build_groups_from_json(input_json: &str) -> String {
    match serde_json::from_str::<GroupInput>(input_json) {
        Ok(input) => {
            let groups = build_channel_groups(&input.channels);
            serde_json::to_string(&GroupOutput { groups })
                .unwrap_or_else(|_| "{\"groups\":[]}".to_string())
        }
        // Malformed input must never crash a sync -- return an empty result.
        Err(_) => "{\"groups\":[]}".to_string(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelGroupNativeBridge_nativeBuildGroups(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let input_str: String = match env.get_string(&input_json) {
            Ok(s) => s.into(),
            Err(_) => return Ok(env.new_string("{\"groups\":[]}").unwrap().into_raw()),
        };
        let output_json = build_groups_from_json(&input_str);
        env.new_string(output_json)
            .unwrap_or_else(|_| env.new_string("{\"groups\":[]}").unwrap())
            .into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// ── Raw-input entry point (Phase 3.1): precompute + group in one JNI call ──

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawChannelIn {
    pub name: String,
    pub genre: String,
    /// Stamped event-bank / club-folder label from DataServiceDetector. When present the
    /// merge category is forced to SportExtra so linear Sport channels cannot fuzzy-merge
    /// into a club roster (Racing TV ≈ Reading FC).
    #[serde(default)]
    pub data_service_group: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawGroupInput {
    pub channels: Vec<RawChannelIn>,
    pub user_region_for_label: Option<String>,
}

/// Computes every `ChannelIn` field from a raw `{name, genre}` pair. Mirrors
/// `ChannelArbiter.unify()`'s precompute block (`normalizedNames`,
/// `strippedNames`, `countryNames`, `tzFeedFlags`, `uhdFlags`, `categoryKeys`)
/// exactly, including the REGIONAL -> LOCAL collapse. The grouping discriminator
/// and the category lookup both read the country with `detect_for_channel` — they
/// used to differ, and the weaker of the two was the grouping one.

/// Region labels trusted enough to split two channels apart.
///
/// `detect_region_in_name` recognises every ITV region, including ones whose names are ordinary
/// words — Central, Border, Anglia, Granada, Calendar, Meridian — and a bare North/South/East/West.
/// Those are fine for deciding what belongs on the Local tab, but as a hard grouping discriminator
/// they split real channels: "Comedy Central" reads as the Central region, so it stopped merging
/// with "Comedy Cent Extra" and with its own "Comedy Centrl" typo variant.
///
/// Restricted to markers that are unambiguous inside a channel name: the four nations, the
/// compound compass regions, and London. Anything else is treated as no region at all, which is
/// the pre-existing behaviour and merges as before.
const GROUPING_REGIONS: &[&str] = &[
    "Wales",
    "Scotland",
    "Northern Ireland",
    "North West",
    "North East",
    "South West",
    "South East",
    "East Midlands",
    "West Midlands",
    "Yorkshire",
    "Channel Islands",
    "London",
    // Bare compass regions are real BBC regions (BBC One East/South/West). They are safe here
    // because US timezone feeds -- the other thing "(East)" means -- are exempt from the region
    // discriminator entirely, handled by tz_feed.
    "North",
    "South",
    "East",
    "West",
];

fn grouping_region(name: &str) -> Option<String> {
    let detected = regional_channel_filter::detect_region_in_name(name)?;
    if GROUPING_REGIONS.contains(&detected.as_str()) {
        Some(detected)
    } else {
        None
    }
}

fn precompute_channel_in(raw: &RawChannelIn, user_region_for_label: Option<&str>) -> ChannelIn {
    // Normalize with the regional marker removed: the region travels as its own key
    // component now, so leaving it in the name would keep two spellings of the same region
    // (`BBC One Scot` vs `UK: BBC ONE SCOTLAND FHD`) in separate groups.
    let grouping_region = grouping_region(&raw.name);
    let name_for_key = if grouping_region.is_some() {
        regional_channel_filter::strip_region_in_name(&raw.name)
    } else {
        raw.name.clone()
    };
    let normalized_name = aurelis_heuristics::channel_name_keys::normalize_for_match(&name_for_key);
    let stripped_name = aurelis_heuristics::channel_name_keys::strip_quality_and_regional(&raw.name);
    // Read the same way the merged channel's own country is read: name prefix, then genre,
    // then either as free text. The name alone left the commonest mis-merge unguarded --
    // "Discovery" under "UK Docs" and "Discovery Channel" under "USA Docs" both yield nothing
    // from the name, so the guard saw two unknowns and let them collapse into one channel
    // wearing whichever country the winning variant's genre said. The genre is precisely where
    // the two rows disagree.
    let country_code =
        aurelis_heuristics::country_affinity::detect_for_channel(&raw.name, &raw.genre).map(|c| c.code.to_string());
    let tz_feed = aurelis_heuristics::country_affinity::is_us_timezone_feed(&raw.name);
    let is_uhd = channel_quality_score::is_uhd_channel(&raw.name);

    let category_name = if is_uhd {
        "UHD".to_string()
    } else if raw
        .data_service_group
        .as_ref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        // Club / event-bank stamp — never fuzzy-merge into linear Sport.
        "SPORT_EXTRA".to_string()
    } else {
        let detected_region = regional_channel_filter::detect_region_in_name(&raw.name);
        let cat = aurelis_heuristics::provider_genre_mapper::categorize(
            country_code.as_deref(),
            &raw.genre,
            &raw.name,
            user_region_for_label,
            detected_region.as_deref(),
        );
        let cat = if cat == aurelis_heuristics::ContentCategory::Regional {
            aurelis_heuristics::ContentCategory::Local
        } else {
            cat
        };
        cat.name().to_string()
    };

    ChannelIn {
        name: raw.name.clone(),
        normalized_name,
        stripped_name,
        country_code,
        tz_feed,
        is_uhd,
        region: grouping_region,
        category: Some(category_name),
        tuner_service_tag: has_tuner_service_tag(&raw.name, &raw.genre),
    }
}

fn build_groups_from_raw_json(input_json: &str) -> String {
    match serde_json::from_str::<RawGroupInput>(input_json) {
        Ok(input) => {
            init_rayon_pool();
            let user_region = input.user_region_for_label.as_deref();
            let channels: Vec<ChannelIn> = if input.channels.len() > PARALLEL_THRESHOLD {
                input
                    .channels
                    .par_iter()
                    .map(|raw| precompute_channel_in(raw, user_region))
                    .collect()
            } else {
                input
                    .channels
                    .iter()
                    .map(|raw| precompute_channel_in(raw, user_region))
                    .collect()
            };
            let groups = build_channel_groups(&channels);
            serde_json::to_string(&GroupOutput { groups })
                .unwrap_or_else(|_| "{\"groups\":[]}".to_string())
        }
        // Malformed input must never crash a sync -- return an empty result.
        Err(_) => "{\"groups\":[]}".to_string(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelGroupNativeBridge_nativeBuildGroupsFromRaw(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let input_str: String = match env.get_string(&input_json) {
            Ok(s) => s.into(),
            Err(_) => return Ok(env.new_string("{\"groups\":[]}").unwrap().into_raw()),
        };
        let output_json = build_groups_from_raw_json(&input_str);
        env.new_string(output_json)
            .unwrap_or_else(|_| env.new_string("{\"groups\":[]}").unwrap())
            .into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ChannelGroupNativeBridge_nativeEngineVersion(
    _env: EnvUnowned,
    _class: JClass,
) -> jint {
    ENGINE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(name: &str, normalized: &str, stripped: &str) -> ChannelIn {
        ChannelIn {
            name: name.to_string(),
            normalized_name: normalized.to_string(),
            stripped_name: stripped.to_string(),
            country_code: None,
            tz_feed: false,
            is_uhd: false,
            region: None,
            category: None,
            tuner_service_tag: false,
        }
    }

    fn ch_full(
        name: &str,
        normalized: &str,
        stripped: &str,
        country: Option<&str>,
        tz_feed: bool,
        is_uhd: bool,
    ) -> ChannelIn {
        ChannelIn {
            name: name.to_string(),
            normalized_name: normalized.to_string(),
            stripped_name: stripped.to_string(),
            country_code: country.map(|s| s.to_string()),
            tz_feed,
            is_uhd,
            region: None,
            category: None,
            tuner_service_tag: false,
        }
    }

    fn ch_cat(
        name: &str,
        normalized: &str,
        stripped: &str,
        category: &str,
    ) -> ChannelIn {
        ChannelIn {
            name: name.to_string(),
            normalized_name: normalized.to_string(),
            stripped_name: stripped.to_string(),
            country_code: None,
            region: None,
            tz_feed: false,
            is_uhd: false,
            category: Some(category.to_string()),
            tuner_service_tag: false,
        }
    }

    fn stable_keys(groups: &[GroupOut]) -> HashSet<String> {
        groups.iter().map(|g| g.stable_key.clone()).collect()
    }

    // Mirrors ChannelArbiterTest.`unify should group channels by normalized name...`
    #[test]
    fn bbc_one_hd_sd_exact_key_merge() {
        let channels = vec![
            ch("BBC One SD", "bbcone", "bbc one"),
            ch("BBC One HD", "bbcone", "bbc one"),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].member_indices, vec![0, 1]);
    }

    // Mirrors ChannelArbiterTest.`unify should collapse UK regional variants...`
    #[test]
    fn uk_regional_variants_collapse_to_one_group() {
        let channels = vec![
            ch("BBC One London", "bbcone", "bbc one"),
            ch("BBC One North West", "bbcone", "bbc one"),
            ch("BBC One Scotland", "bbcone", "bbc one"),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].member_indices, vec![0, 1, 2]);
    }

    // Mirrors ChannelArbiterTest.`US East feed stays separate from UK channel...`
    #[test]
    fn us_east_feed_stays_separate_from_uk_channel() {
        let channels = vec![
            ch_full("Discovery", "discovery", "discovery", None, false, false),
            ch_full("Discovery (East)", "discovery", "discovery", Some("US"), true, false),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 2);
    }

    // Mirrors ChannelArbiterTest.`plain US feed merges with its own East timezone feed`
    #[test]
    fn same_known_country_tz_relaxation_merges() {
        let channels = vec![
            ch_full("TNT", "tnt", "tnt", Some("US"), false, false),
            ch_full("TNT (East)", "tnt", "tnt", Some("US"), true, false),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 1);
    }

    // Mirrors the intent of ChannelArbiterTest.`unify should merge bbc parl and bbc
    // parliament`: two channels with DIFFERENT normalized keys (so Phase A can't
    // bucket them together) must still merge via Phase B's fuzzy Levenshtein path.
    // ("Parl"/"Parliament" itself scores only ~0.4 word-similarity -- below the 0.70
    // word-match guard -- so a near-duplicate-spelling pair is used here instead to
    // exercise the same code path validly.)
    #[test]
    fn fuzzy_path_merges_near_duplicate_spelling() {
        let channels = vec![
            ch("Comedy Central", "comedycentral", "comedy central"),
            ch("Comedy Centrl HD", "comedycentrl", "comedy centrl"),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 1);
    }

    // Mirrors ChannelArbiterTest.`unify should group variants of same channel and rank by quality`
    // (UHD discriminator: CNN FHD vs CNN 4K must stay separate).
    #[test]
    fn uhd_discriminator_keeps_4k_separate() {
        let channels = vec![
            ch_full("CNN FHD", "cnn", "cnn", None, false, false),
            ch_full("CNN 4K", "cnn", "cnn", None, false, true),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 2);
    }

    // Mirrors ChannelArbiterTest.`unify should NOT combine different networks or
    // different numbered channels` -- word-mismatch and number-mismatch guards.
    #[test]
    fn distinct_networks_and_numbered_channels_stay_separate() {
        let channels = vec![
            ch("Sky Sports Golf", "skysportsgolf", "sky sports golf"),
            ch("Sky Sports News", "skysportsnews", "sky sports news"),
            ch("BBC One", "bbcone", "bbc one"),
            ch("BBC Two", "bbctwo", "bbc two"),
            ch("Sky Sports 1", "skysports1", "sky sports 1"),
            ch("Sky Sports 2", "skysports2", "sky sports 2"),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 6);
    }

    // Mirrors ChannelArbiterTest.`abbreviated variant merges with full name despite
    // name-only country split` -- the norm-key-match fast path across a name-only
    // country split (one side has a detected country, the other doesn't, but the
    // alias-normalised keys are identical).
    #[test]
    fn norm_key_match_fast_path_across_name_only_country_split() {
        let channels = vec![
            ch_full(
                "UK: SKY SPORTS FOOTBALL FHD",
                "sky sports football",
                "uk sky sports football",
                Some("GB"),
                false,
                false,
            ),
            ch_full("SkySp F'ball", "sky sports football", "skysp fball", None, false, false),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn order_independent_grouping() {
        let forward = vec![
            ch("BBC One HD", "bbcone", "bbc one"),
            ch("BBC One SD", "bbcone", "bbc one"),
            ch("Sky News FHD", "skynews", "sky news"),
            ch("Sky News HD", "skynews", "sky news"),
            ch("ITV1 FHD", "itv1", "itv 1"),
        ];
        let mut reversed = forward.clone();
        reversed.reverse();

        let groups_forward = build_channel_groups(&forward);
        let groups_reversed = build_channel_groups(&reversed);

        assert_eq!(groups_forward.len(), 3);
        assert_eq!(stable_keys(&groups_forward), stable_keys(&groups_reversed));
    }

    #[test]
    fn extract_numbers_handles_digits_and_number_words() {
        assert_eq!(extract_numbers("Sky Sports 1"), vec!["1".to_string()]);
        assert_eq!(extract_numbers("Channel One"), vec!["1".to_string()]);
        assert_eq!(extract_numbers("BBC One"), vec!["1".to_string()]);
        assert!(extract_numbers("Sky News").is_empty());
    }

    #[test]
    fn levenshtein_distance_basic_cases() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("abc", ""), 3);
        assert_eq!(levenshtein_distance("bbc one", "bbc one"), 0);
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 3);
    }

    // Same normalized/stripped name, different mapped category (e.g. a
    // "Sports News" channel filed under SPORT by one source and NEWS by
    // another) must NOT merge -- the bug this discriminator fixes.
    #[test]
    fn different_categories_do_not_merge() {
        let channels = vec![
            ch_cat("Sports News", "sportsnews", "sports news", "SPORT"),
            ch_cat("Sports News", "sportsnews", "sports news", "NEWS"),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 2);
    }

    // A generic catch-all category (OTHERS/ENTERTAINMENT) on one side still
    // allows the merge when the stripped names are exact/near-identical.
    #[test]
    fn generic_category_rescues_exact_name_merge() {
        let channels = vec![
            ch_cat("Comedy Central", "comedycentral", "comedy central", "ENTERTAINMENT"),
            ch_cat("Comedy Central", "comedycentral", "comedy central", "OTHERS"),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 1);
    }

    // Two non-generic, differing categories block the merge even though the
    // fuzzy-similarity/word-overlap guards alone would have allowed it
    // (mirrors: neither side is OTHERS/ENTERTAINMENT, so there's no rescue).
    #[test]
    fn distinct_non_generic_categories_block_otherwise_fuzzy_merge() {
        let channels = vec![
            ch_cat("Dave", "dave", "dave", "MUSIC"),
            ch_cat("Cave", "cave", "cave", "SPORT"),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 2);
    }

    // categoryKeys omitted entirely (None per-channel, mirroring Kotlin's
    // categoryKeys.isEmpty()) must not change existing merge behaviour.
    #[test]
    fn missing_category_is_backward_compatible() {
        let channels = vec![
            ch("BBC One SD", "bbcone", "bbc one"),
            ch("BBC One HD", "bbcone", "bbc one"),
        ];
        let groups = build_channel_groups(&channels);
        assert_eq!(groups.len(), 1);
    }

    // ── Raw-input entry point (Phase 3.1) parity tests ──────────────────────
    // These exercise the actual JNI-callable `build_groups_from_raw_json` on
    // raw {name, genre} pairs -- no precomputed keys supplied at all -- and
    // check it reproduces the same merge decisions the precomputed-input path
    // (and Kotlin's ChannelArbiter.unify()) makes for the same real-world
    // shapes covered above.

    fn raw_input(channels: &[(&str, &str)], user_region_for_label: Option<&str>) -> String {
        let channels: Vec<serde_json::Value> = channels
            .iter()
            .map(|(name, genre)| serde_json::json!({ "name": name, "genre": genre }))
            .collect();
        serde_json::json!({
            "channels": channels,
            "userRegionForLabel": user_region_for_label,
        })
        .to_string()
    }

    fn raw_groups(channels: &[(&str, &str)], user_region_for_label: Option<&str>) -> GroupOutput {
        let output_json = build_groups_from_raw_json(&raw_input(channels, user_region_for_label));
        serde_json::from_str(&output_json).unwrap()
    }

    /// BT Sport became TNT Sports: one channel, one row with both providers' streams (owner,
    /// 8 Oct 2026). Until the aliases reached this side they were two rows on every box.
    #[test]
    fn raw_entry_point_merges_bt_sport_into_tnt_sports() {
        let out = raw_groups(
            &[
                ("BT Sport 1", "UK Sports"),
                ("UK: TNT Sports 1 FHD", "UK: Sports"),
                ("Sky Sports Prem Sp", "UK Sports"),
                ("UK: Sky Sports Premier League FHD", "UK: Sky Sports"),
            ],
            None,
        );
        let mut members: Vec<Vec<usize>> = out.groups.iter().map(|g| g.member_indices.clone()).collect();
        members.sort();
        assert_eq!(members, vec![vec![0, 1], vec![2, 3]], "{members:?}");
    }

    /// "Crime+Inv" (Resoluted) beside "UK: CRIME & INVESTIGATION" (two other providers) on `.246`,
    /// 8 Oct 2026: the Documentaries tab showed "Crime And Investigation" and "Crime &
    /// Investigation" as two rows. "&" is "and" (owner, 8 Oct 2026).
    #[test]
    fn raw_entry_point_merges_crime_inv_with_crime_and_investigation() {
        let out = raw_groups(
            &[
                ("Crime+Inv", "UK Docs"),
                ("UK: CRIME & INVESTIGATION FHD", "UK: Documentary"),
                ("UK: CRIME & INVESTIGATION  HD", "UK| DOCUMENTARIES"),
            ],
            None,
        );
        let members: Vec<Vec<usize>> = out.groups.iter().map(|g| g.member_indices.clone()).collect();
        assert_eq!(members, vec![vec![0, 1, 2]], "{members:?}");
    }

    /// The aerial lineup on `.246`, 11 Sep 2026: distinct services that had merged (ITV2/3/4,
    /// the ITV +1s, the Extras, Radio Wales into Radio Cymru), beside the true variants that must
    /// still merge ("5" with "5 HD", and BBC One Wales in SD and HD).
    #[test]
    fn raw_entry_point_keeps_aerial_siblings_apart() {
        let names = [
            "ITV2", "ITV3", "ITV4", "ITV1+1", "ITV2+1", "ITV3+1", "ITV4+1", "E4", "E4 Extra",
            "Your TV", "Your TV Extra", "BBC Radio 4", "BBC Radio 4 Ex", "BBC Radio Wales",
            "BBC Radio Cymru", "5", "5 HD", "BBC ONE Wales", "BBC 1 Wales HD",
            "That's TV", "That's TV (UK)",
        ];
        let pairs: Vec<(&str, &str)> = names.iter().map(|n| (*n, "HDHomeRun")).collect();
        let out: serde_json::Value =
            serde_json::from_str(&build_groups_from_raw_json(&raw_input(&pairs, Some("Wales")))).unwrap();
        let groups: Vec<Vec<&str>> = out["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| {
                g["memberIndices"].as_array().unwrap().iter().map(|i| names[i.as_u64().unwrap() as usize]).collect()
            })
            .collect();
        assert_eq!(groups.len(), 19, "{groups:?}");
        assert!(groups.contains(&vec!["That's TV"]), "{groups:?}");
        assert!(groups.contains(&vec!["That's TV (UK)"]), "{groups:?}");
        assert!(groups.contains(&vec!["5", "5 HD"]), "{groups:?}");
        assert!(groups.contains(&vec!["BBC ONE Wales", "BBC 1 Wales HD"]), "{groups:?}");
    }

    /// The household's own names, 3 Oct 2026: the aerial's "BBC RB 1" beside the internet
    /// feeds' "BBC Red Button 1", NOW's "SKY ARTS 1" beside everyone else's "Sky Arts", and
    /// "ITV Cymru Wales" beside "ITV1 Wales". Mirrors `AerialDuplicateAndAliasTest`.
    #[test]
    fn raw_entry_point_folds_red_button_sky_arts_and_itv_wales_aliases() {
        let red = raw_groups(
            &[
                ("BBC RB 1", "HDHomeRun"),
                ("BBC Red Button", "HDHomeRun"),
                ("UK: BBC RED BUTTON 1 FHD", "UK: Entertainment"),
            ],
            Some("Wales"),
        );
        assert_eq!(red.groups.len(), 1, "the red button is one channel");
        let arts = raw_groups(
            &[
                ("Sky Arts", "HDHomeRun"),
                ("SKY ARTS 1", "UK| NOW TV ENTERTAINMENT ᴴᴰ/ᴿᴬᵂ"),
                ("UK: SKY ARTS", "UK| GENERAL"),
                ("UK: SKY ARTS FHD", "UK: Entertainment"),
                ("Sky Arts", "UK Ent"),
            ],
            Some("Wales"),
        );
        assert_eq!(arts.groups.len(), 1, "Sky Arts 1 is Sky Arts");
        let itv = raw_groups(
            &[
                ("ITV1 Wales", "HDHomeRun"),
                ("ITV1 Wales HD", "HDHomeRun"),
                ("UK: ITV CYMRU WALES FHD", "UK/EIR: Regions"),
                ("UK: ITV1 WALES", "UK| GENERAL"),
                ("ITV Wales", "UK Reg"),
            ],
            Some("Wales"),
        );
        assert_eq!(itv.groups.len(), 1, "ITV Cymru Wales is ITV1 Wales");
    }

    #[test]
    fn explicit_equivalence_outranks_the_sibling_marker() {
        let output = raw_groups(
            &[
                ("Horror Channel", "UK: Movies"),
                ("Horror Xtra", "UK: Movies"),
                ("E4", "UK: Entertainment"),
                ("E4 Extra", "UK: Entertainment"),
            ],
            None,
        );
        assert_eq!(output.groups.len(), 3, "Horror Channel = Horror Xtra; E4 Extra is not E4");
    }

    #[test]
    fn tuner_service_tag_reads_only_a_tuners_own_bracketed_country() {
        assert!(has_tuner_service_tag("That's TV (UK)", "HDHomeRun"));
        assert!(has_tuner_service_tag("That's TV [ gb ]", "hdhomerun"));
        assert!(!has_tuner_service_tag("That's TV", "HDHomeRun"));
        assert!(!has_tuner_service_tag("That's TV (UK)", "UK| Entertainment"));
        assert!(!has_tuner_service_tag("Sky News (UKTV)", "HDHomeRun"));
    }

    /// Mirrors ChannelArbiterTest's provider case: the tuner rule must not reach a provider list.
    #[test]
    fn a_providers_country_tag_still_merges() {
        let output = raw_groups(
            &[("That's TV", "UK| Entertainment"), ("That's TV (UK)", "UK| Entertainment")],
            Some("Wales"),
        );
        assert_eq!(output.groups.len(), 1, "{:?}", output.groups);
    }

    #[test]
    fn tuner_service_tag_does_not_collide_stable_key_when_iptv_supplies_gb_country() {
        let output = raw_groups(
            &[
                ("UK: That's TV", "UK| Entertainment"),
                ("That's TV", "HDHomeRun"),
                ("That's TV (UK)", "HDHomeRun"),
            ],
            Some("Wales"),
        );
        assert_eq!(output.groups.len(), 2, "{:?}", output.groups);
        let keys: HashSet<String> = output.groups.iter().map(|g| g.stable_key.clone()).collect();
        assert_eq!(
            keys.len(),
            2,
            "tuner-tagged service must have a distinct stable_key from untagged GB cluster: {keys:?}"
        );
    }

    #[test]
    fn sibling_marker_reads_extra_and_ex_as_words() {
        assert!(has_sibling_marker("E4 Extra"));
        assert!(has_sibling_marker("BBC Radio 4 Ex"));
        assert!(has_sibling_marker("Comedy Xtra"));
        assert!(!has_sibling_marker("E4"));
        assert!(!has_sibling_marker("Extreme Sports"));
        assert!(!has_sibling_marker("Sky Explorer"));
    }

    /// Real `.246` failure: Racing TV fuzzy-merged with Reading FC (League One Teams) because
    /// "racing"≈"reading", then inherited the club bank stamp and vanished from UK Sport.
    #[test]
    fn raw_entry_point_keeps_racing_tv_apart_from_reading_fc() {
        let input = serde_json::json!({
            "channels": [
                { "name": "UK: RACING UK", "genre": "UK| SPORT" },
                { "name": "Racing TV", "genre": "UK: Sports" },
                {
                    "name": "Reading",
                    "genre": "League One Teams",
                    "dataServiceGroup": "League One Teams"
                }
            ],
            "userRegionForLabel": null,
        })
        .to_string();
        let output: GroupOutput =
            serde_json::from_str(&build_groups_from_raw_json(&input)).unwrap();
        assert!(
            output.groups.len() >= 2,
            "Reading FC must not absorb Racing UK/TV (got {} groups)",
            output.groups.len()
        );
        // Reading is alone in its club-bank group.
        let reading_alone = output.groups.iter().any(|g| {
            g.member_indices.len() == 1 && g.member_indices[0] == 2
        });
        assert!(reading_alone, "Reading must stay its own channel: {:?}", output.groups);
    }

    /// Twin of `SkySportsF1MergeRegressionTest` (app unit tests).
    ///
    /// Taken from the real `.246` lineup, through the raw entry point that actually runs on
    /// device. Sky Sports F1 had been collapsed into New Zealand's Sky Sport 1 and, through it,
    /// into the Sky Sports+ event bank -- which moved Formula 1 into Sports Extra (the bank
    /// member carries the `dataServiceGroup` that decides the tab) and left the guide reading
    /// "No information" while 232 EPG rows sat on the copy it had been split from.
    #[test]
    fn raw_entry_point_keeps_f1_the_bank_and_the_numbered_channel_apart() {
        let output = raw_groups(
            &[
                ("UK: SKY SPORTS F1 FHD", "UK: Sky Sports"),
                ("SkySp F1", "UK Sports 50fps"),
                ("SkySp F1", "UK Sports"),
                ("SkySp 1", "AU/NZ Sports"),
                ("SkySp+ 1", "SkySP+"),
                ("SkySp+ 2", "SkySP+"),
            ],
            None,
        );
        assert_eq!(output.groups.len(), 4, "F1, NZ Sky Sport 1, and two bank members");

        let group_of = |i: usize| {
            output
                .groups
                .iter()
                .find(|g| g.member_indices.contains(&i))
                .unwrap_or_else(|| panic!("channel {i} was dropped entirely"))
        };
        // Both providers' Formula 1 on one channel -- including the abbreviated spellings.
        assert_eq!(group_of(0).member_indices, vec![0, 1, 2]);
        // ...and nothing else joins it.
        assert_ne!(group_of(3).member_indices, group_of(0).member_indices);
        assert_eq!(group_of(3).member_indices, vec![3]);
        assert_eq!(group_of(4).member_indices, vec![4]);
        assert_eq!(group_of(5).member_indices, vec![5]);

        // A discriminator that keeps clusters apart must reach the stable key too, or one
        // collides with the other and its channels are silently gone.
        let keys: HashSet<String> = output.groups.iter().map(|g| g.stable_key.clone()).collect();
        assert_eq!(keys.len(), 4, "stable keys must not collide: {keys:?}");
    }

    #[test]
    fn service_plus_marker_reads_either_spelling() {
        assert!(has_service_plus_marker("SkySp+ 1"));
        assert!(has_service_plus_marker("Sky Sports Plus"));
        assert!(has_service_plus_marker("GAA⁺"));
        assert!(!has_service_plus_marker("SkySp 1"));
        assert!(!has_service_plus_marker("Sky Sports F1"));
        // Not a marker: "plus" inside a longer word.
        assert!(!has_service_plus_marker("Plusnet TV"));
        assert!(!has_service_plus_marker("Surplus Channel"));
        // A plus that joins two words is "and".
        assert!(!has_service_plus_marker("Crime+Inv"));
        assert!(!has_service_plus_marker("UK: Crime + Investigation HD"));
        assert!(!has_service_plus_marker("Sky Sci-Fi + Horror"));
        assert!(has_service_plus_marker("Crime+Inv +1"), "its own +1 is still a plus");
        assert!(has_service_plus_marker("CANAL+SPORT"), "the plus is the brand");
    }

    #[test]
    fn raw_entry_point_merges_hd_sd_variants_from_names_alone() {
        let output = raw_groups(&[("BBC One SD", "Entertainment"), ("BBC One HD", "Entertainment")], None);
        assert_eq!(output.groups.len(), 1);
        assert_eq!(output.groups[0].member_indices, vec![0, 1]);
    }

    /// UK regional variants are DIFFERENT channels and must not collapse together.
    ///
    /// This test previously asserted the opposite -- that London, the North West and Scotland
    /// merged into one channel -- on the reasoning that they are all "BBC One". Real device
    /// data showed where that leads: 22 feeds, including Scotland, Northern Ireland, Wales,
    /// Yorkshire and the Channel Islands, presented as a single channel with 22 interchangeable
    /// streams. Picking one of those streams could put a different nation's programming on
    /// screen, and a viewer in Wales who selected "BBC One Wales" had no way to stay there.
    ///
    /// Merging the same feed across providers is still the goal and still works -- that is
    /// `same_regional_channel_from_two_sources_groups_together`. The distinction is region,
    /// not provider.
    #[test]
    fn raw_entry_point_keeps_uk_regional_variants_apart() {
        let output = raw_groups(
            &[
                ("BBC One London", "Entertainment"),
                ("BBC One North West", "Entertainment"),
                ("BBC One Scotland", "Entertainment"),
            ],
            None,
        );
        assert_eq!(output.groups.len(), 3, "each nation/region is its own channel");
    }

    #[test]
    fn raw_entry_point_keeps_uhd_separate_from_hd() {
        let output = raw_groups(&[("CNN FHD", "News"), ("CNN 4K", "News")], None);
        assert_eq!(output.groups.len(), 2);
    }

    // Sports vs News genre for the same base name must land in different
    // mapped categories and NOT merge -- the raw-input path must derive
    // categories itself and still apply the Stage-0 discriminator.
    #[test]
    fn raw_entry_point_applies_category_discriminator_from_genre() {
        let output = raw_groups(&[("Sky Sports News", "Sport"), ("Sky News", "News")], None);
        assert_eq!(output.groups.len(), 2);
    }

    #[test]
    fn raw_entry_point_us_east_feed_stays_separate_from_uk_channel() {
        let output = raw_groups(&[("Discovery", "Entertainment"), ("Discovery (East)", "Entertainment")], None);
        assert_eq!(output.groups.len(), 2);
    }

    // Mirrors ChannelArbiterTest.`a plus-one timeshift feed does not merge into its parent
    // channel`. The stripped names compared in Phase B still carry a bare "+1", and on a
    // long parent name that difference clears the 35% length check AND the 0.85 bar the
    // number-mismatch bypass demands -- so the +1 merged into its parent and stopped
    // existing as a channel. Short names were saved only by being short.
    #[test]
    fn raw_entry_point_keeps_a_plus_one_feed_apart_from_its_parent() {
        let output = raw_groups(
            &[
                ("National Geographic", "UK: Documentaries"),
                ("National Geographic +1", "UK: Documentaries"),
            ],
            None,
        );
        assert_eq!(output.groups.len(), 2, "a +1 timeshift is not its parent channel");
    }

    // The other half of the guard: it must split a +1 from its PARENT, never one provider's
    // +1 from another's. "+1" and "Plus 1" are the same feed spelled twice.
    #[test]
    fn raw_entry_point_merges_plus_one_feeds_from_two_sources() {
        let output = raw_groups(
            &[
                ("National Geographic +1", "UK: Documentaries"),
                ("UK: NATIONAL GEOGRAPHIC PLUS 1 FHD", "UK: Documentary"),
            ],
            None,
        );
        assert_eq!(output.groups.len(), 1);
        assert_eq!(output.groups[0].member_indices, vec![0, 1]);
    }

    // Mirrors ChannelArbiterTest.`US History does not merge with Sky History`. Neither name
    // carries a country token, both map to DOCUMENTARIES, the length gap is inside the
    // allowance and similarity is ~0.70 -- over the 0.65 threshold. The ONLY thing keeping
    // them apart is the significant-word rule: "sky" has no counterpart in "history".
    #[test]
    fn raw_entry_point_keeps_us_history_apart_from_sky_history() {
        let output = raw_groups(&[("Sky History", "UK Docs"), ("History", "USA Docs")], None);
        assert_eq!(output.groups.len(), 2, "the word rule is the only guard holding these apart");
    }

    #[test]
    fn raw_entry_point_keeps_discovery_history_apart_from_discovery() {
        let output = raw_groups(&[("Discovery", "UK Docs"), ("Discovery History", "UK Docs")], None);
        assert_eq!(output.groups.len(), 2);
    }

    /// Live data from a user's device: two Discovery rows whose names say nothing about
    /// where they are from, and whose folders say everything. Reading the name alone, the
    /// country guard saw two unknowns and merged them, and one of the two disappeared into
    /// the other's country section.
    #[test]
    fn raw_entry_point_keeps_us_discovery_out_of_the_uk_one() {
        let output = raw_groups(&[("Discovery", "UK Docs"), ("Discovery Channel", "USA Docs")], None);
        assert_eq!(output.groups.len(), 2, "the folder is the only thing that says these differ");
    }

    /// RTÉ arrives tagged `UK:` inside a `UK/EIR` folder, so every country signal on the row
    /// reads Britain until the broadcaster itself is recognised.
    #[test]
    fn raw_entry_point_reads_rte_as_irish_despite_the_uk_tag() {
        let country = aurelis_heuristics::country_affinity::detect_for_channel(
            "UK: RTE ONE HD",
            "UK/EIR: Regions",
        );
        assert_eq!(country.map(|c| c.code), Some("IE"));
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}

#[cfg(test)]
mod bbc_wales_regression {
    use super::*;

    /// The same real channel from two different providers must land in one group.
    ///
    /// Live data from a user's device: `BBC One Wales` (Resoluted) and
    /// `UK: BBC ONE WALES FHD` (SimplyVPS) are the same broadcast feed, but the stream
    /// picker showed 1/1 rather than 1/2 -- so only one provider's copy was reachable and
    /// the other was, from the viewer's point of view, a lost source.
    #[test]
    fn same_regional_channel_from_two_sources_groups_together() {
        let json = r#"{"channels":[
            {"name":"BBC One Wales","genre":"UK"},
            {"name":"UK: BBC ONE WALES FHD","genre":"UK"}
        ],"userRegionForLabel":"Wales"}"#;
        let out = build_groups_from_raw_json(json);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let groups = parsed["groups"].as_array().unwrap();
        println!("GROUPS: {}", serde_json::to_string_pretty(&parsed).unwrap());
        assert_eq!(groups.len(), 1, "expected ONE group, got {}", groups.len());
    }

    /// ...while genuinely different regions must stay apart.
    #[test]
    fn different_regions_stay_separate() {
        let json = r#"{"channels":[
            {"name":"BBC One Wales","genre":"UK"},
            {"name":"UK: BBC ONE SCOTLAND FHD","genre":"UK"}
        ],"userRegionForLabel":"Wales"}"#;
        let out = build_groups_from_raw_json(json);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let groups = parsed["groups"].as_array().unwrap();
        println!("GROUPS: {}", serde_json::to_string_pretty(&parsed).unwrap());
        assert_eq!(groups.len(), 2, "Wales and Scotland must not merge");
    }
}

#[cfg(test)]
mod bbc_real_device_data {
    use super::*;

    /// The exact 26 BBC One rows present on the user's device, with their real provider genres.
    ///
    /// Reads a scratch capture rather than an embedded fixture, so it can only run on a machine
    /// that still has the file. It used to `unwrap()` that read and so failed the whole crate's
    /// `cargo test` everywhere else — and `cargo test` is the verification step AGENTS.md asks
    /// for on every change to this crate, so a gate that is red by default is worse than no
    /// gate. Skips when the capture is absent; still runs, unchanged, when it is there.
    #[test]
    fn real_bbc_one_lineup_grouping() {
        const CAPTURE: &str = "/tmp/bbc.json";
        let Ok(json) = std::fs::read_to_string(CAPTURE) else {
            eprintln!("skipping: no device capture at {CAPTURE}");
            return;
        };
        let out = build_groups_from_raw_json(&json);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let input: serde_json::Value = serde_json::from_str(&json).unwrap();
        let names: Vec<String> = input["channels"].as_array().unwrap().iter()
            .map(|c| c["name"].as_str().unwrap().to_string()).collect();
        let groups = parsed["groups"].as_array().unwrap();
        println!("=== {} channels -> {} groups", names.len(), groups.len());
        for g in groups {
            let idx: Vec<usize> = g["memberIndices"].as_array().unwrap().iter()
                .map(|v| v.as_u64().unwrap() as usize).collect();
            println!("  [{}] {} :: {:?}", idx.len(), g["stableKey"].as_str().unwrap(),
                idx.iter().map(|i| &names[*i]).collect::<Vec<_>>());
        }
    }
}

#[cfg(test)]
mod region_alias_check {
    #[test]
    fn abbreviations_resolve() {
        for n in ["BBC One Scot","BBC One NI","BBC One NW","BBC One WM","BBC One EMid","BBC One Yks","BBC One Wales","BBC One London","BBC One"] {
            println!("{:>20} -> {:?}", n, regional_channel_filter::detect_region_in_name(n));
        }
    }
}

#[cfg(test)]
mod brand_equivalence_tests {
    use super::*;

    fn gb() -> Option<String> { Some("GB".to_string()) }
    fn us() -> Option<String> { Some("US".to_string()) }

    /// On a British dial the channel branded CNN IS CNN International; the household's own
    /// line-up carried it three times from three providers.
    #[test]
    fn uk_cnn_and_cnn_international_are_one_channel() {
        assert!(same_channel_brand("cnn", "cnn international", &gb(), &gb()));
        assert!(same_channel_brand("cnn international", "cnn", &gb(), &gb()));
    }

    #[test]
    fn uk_cnbc_and_cnbc_europe_are_one_channel() {
        assert!(same_channel_brand("cnbc", "cnbc europe", &gb(), &gb()));
    }

    /// The reason this is country-scoped: on an American dial these are two real channels.
    #[test]
    fn us_cnn_and_cnn_international_stay_apart() {
        assert!(!same_channel_brand("cnn", "cnn international", &us(), &us()));
    }

    /// CNN and CNBC are unrelated channels and must never be brought together.
    #[test]
    fn cnn_is_not_cnbc() {
        assert!(!same_channel_brand("cnn", "cnbc", &gb(), &gb()));
        assert!(!same_channel_brand("cnn international", "cnbc europe", &gb(), &gb()));
    }

    #[test]
    fn an_unknown_country_is_never_a_match() {
        assert!(!same_channel_brand("cnn", "cnn international", &None, &gb()));
        assert!(!same_channel_brand("cnn", "cnn international", &gb(), &None));
        assert!(!same_channel_brand("cnn", "cnn international", &None, &None));
    }

    #[test]
    fn different_countries_never_match() {
        assert!(!same_channel_brand("cnn", "cnn international", &gb(), &us()));
    }

    /// Nothing outside the list is affected.
    #[test]
    fn unlisted_names_are_untouched() {
        assert!(!same_channel_brand("bbc one", "bbc one wales", &gb(), &gb()));
        assert!(!same_channel_brand("sky sports", "sky sports plus", &gb(), &gb()));
    }

    #[test]
    fn spacing_and_case_do_not_matter() {
        assert!(same_channel_brand("  CNN  ", "CNN   International", &gb(), &gb()));
    }
}
