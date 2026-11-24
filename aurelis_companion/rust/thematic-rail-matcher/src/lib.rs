//! Native scoring engine for Aurelis's `core/domain` `ThematicRailMatcher` (movies)
//! and `SeriesThematicRailMatcher` (series).
//!
//! Ports `scoreMovie`/`scoreSeries`/`scoreForConcept`/`countMatches`/`reasonFor`
//! line-for-line behind a single generic [`score_item`]/[`reason_for`] pair --
//! `SeriesThematicRailMatcher.ConceptSpec` is a strict subset of
//! `ThematicRailMatcher.ConceptSpec` (no year bounds, no `preferOutsideBoost`,
//! no `signalFromEra`/`signalFromQuality`), so passing those fields at their
//! Kotlin defaults (`None`/`false`) from the series call site makes the shared
//! algorithm degrade to the exact series behaviour.
//!
//! `MovieLandingCurator.isAllowedForLanding`/`SeriesThematicRailMatcher`'s adult-category
//! regex guard and `reservedIds` filtering stay Kotlin-side -- callers pass only the
//! already-eligible pool, mirroring `rust/channel-merge`'s "caller-precomputed
//! primitives" convention. `currentYear` (used for the movie-only "recent release"
//! bonus) is also a caller-supplied primitive since this crate has no date/time
//! dependency.
//!
//! Owns no long-lived state: one call receives the whole pool and spec and returns
//! the scored/ranked/sliced rail. Not wired in as the app's active scoring logic
//! anywhere -- `ThematicRailMatcher`/`SeriesThematicRailMatcher` remain the only
//! implementations actually selected, mirroring `rust/channel-merge`'s dormant status.
//!
//! The shuffle step (`sortedByDescending{score}.shuffled(random).sortedByDescending{score}`)
//! is reimplemented with a self-contained splitmix64 PRNG rather than reproducing the
//! JVM's `kotlin.random.Random` bit-for-bit -- since the final stable sort re-orders by
//! score afterwards, the shuffle only ever decides tie-break order among same-score
//! items, not which items are selected, so exact RNG parity isn't required for a
//! dormant "future parity comparison" engine.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

const ITEMS_PER_RAIL: usize = 12;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConceptSpecIn {
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub genre_hints: Vec<String>,
    #[serde(default)]
    pub category_hints: Vec<String>,
    #[serde(default)]
    pub min_year: Option<i32>,
    #[serde(default)]
    pub max_year: Option<i32>,
    #[serde(default)]
    pub min_rating: f64,
    #[serde(default)]
    pub prefer_outside_boost: bool,
    #[serde(default)]
    pub boost_pools: Vec<String>,
    #[serde(default = "default_min_match_score")]
    pub min_match_score: i32,
    #[serde(default)]
    pub signal_from_era: bool,
    #[serde(default)]
    pub signal_from_quality: bool,
}

fn default_min_match_score() -> i32 {
    80
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemIn {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub rating: f64,
    #[serde(default)]
    pub has_artwork: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoreInput {
    pub items: Vec<ItemIn>,
    pub spec: ConceptSpecIn,
    #[serde(default)]
    pub boost_keys: HashMap<String, Vec<String>>,
    /// Precomputed `daySeed + concept.id.hashCode()` from the Kotlin caller.
    #[serde(default)]
    pub seed: i32,
    /// `null` for series pools (no "recent release" bonus applies).
    #[serde(default)]
    pub current_year: Option<i32>,
    #[serde(default)]
    pub is_series: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoredOut {
    pub key: String,
    pub score: i32,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoreOutput {
    pub scored: Vec<ScoredOut>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountInput {
    pub items: Vec<ItemIn>,
    pub spec: ConceptSpecIn,
    #[serde(default)]
    pub boost_keys: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub current_year: Option<i32>,
}

fn boost_keys_flat(boost_keys: &HashMap<String, Vec<String>>) -> std::collections::HashSet<&str> {
    boost_keys.values().flatten().map(|s| s.as_str()).collect()
}

fn in_boost(key: &str, spec: &ConceptSpecIn, boost_keys: &HashMap<String, Vec<String>>) -> bool {
    spec.boost_pools.iter().any(|pool| {
        boost_keys
            .get(pool)
            .map(|keys| keys.iter().any(|k| k == key))
            .unwrap_or(false)
    })
}

/// Byte-for-byte port of `ThematicRailMatcher.scoreMovie`/`SeriesThematicRailMatcher.scoreSeries`.
/// Passing `spec.min_year = None`, `spec.max_year = None`, `spec.prefer_outside_boost = false`,
/// `spec.signal_from_era = false`, `spec.signal_from_quality = false`, and `current_year = None`
/// (the series call site's defaults) reduces this exactly to the series algorithm.
fn score_item(
    item: &ItemIn,
    spec: &ConceptSpecIn,
    boost_keys: &HashMap<String, Vec<String>>,
    flat: &std::collections::HashSet<&str>,
    current_year: Option<i32>,
) -> Option<i32> {
    if !item.has_artwork {
        return None;
    }
    let title = item.title.to_lowercase();
    let category = item.category.to_lowercase();
    let year = item.year;
    let rating = item.rating;

    if let Some(min_year) = spec.min_year {
        if year.map_or(true, |y| y < min_year) {
            return None;
        }
    }
    if let Some(max_year) = spec.max_year {
        if year.map_or(true, |y| y > max_year) {
            return None;
        }
    }
    if rating < spec.min_rating && spec.min_rating > 0.0 {
        let keyword_hit = spec.keywords.iter().any(|kw| title.contains(kw.as_str()));
        if !keyword_hit {
            return None;
        }
    }

    let mut score: i32 = 0;
    for kw in &spec.keywords {
        if title.contains(kw.as_str()) {
            score += 120;
        }
    }
    for hint in &spec.genre_hints {
        if title.contains(hint.as_str()) || category.contains(hint.as_str()) {
            score += 80;
        }
    }
    for hint in &spec.category_hints {
        if category.contains(hint.as_str()) {
            score += 90;
        }
    }
    if rating > 0.0 {
        score += (((rating - 6.0) * 35.0) as i32).max(0);
    }
    if let (Some(y), Some(cy)) = (year, current_year) {
        if y >= cy - 2 {
            score += 25;
        }
    }

    let is_in_boost = in_boost(&item.key, spec, boost_keys);
    if is_in_boost {
        score += 150;
    }
    if spec.prefer_outside_boost {
        if flat.contains(item.key.as_str()) {
            score -= 80;
        } else {
            score += 60;
        }
    }

    let has_thematic_signal = spec.keywords.iter().any(|kw| title.contains(kw.as_str()))
        || spec
            .genre_hints
            .iter()
            .any(|hint| title.contains(hint.as_str()) || category.contains(hint.as_str()))
        || spec.category_hints.iter().any(|hint| category.contains(hint.as_str()))
        || spec.signal_from_era
        || (spec.signal_from_quality
            && rating >= spec.min_rating
            && (!spec.prefer_outside_boost || !flat.contains(item.key.as_str())));
    if !has_thematic_signal && !is_in_boost {
        return None;
    }

    if score <= 0 {
        return None;
    }
    Some(score)
}

fn titlecase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Byte-for-byte port of `ThematicRailMatcher.reasonFor`/`SeriesThematicRailMatcher.reasonFor`.
fn reason_for(
    item: &ItemIn,
    spec: &ConceptSpecIn,
    boost_keys: &HashMap<String, Vec<String>>,
    flat: &std::collections::HashSet<&str>,
    is_series: bool,
) -> String {
    let title = item.title.to_lowercase();
    let in_pool = |pool: &str| -> bool {
        boost_keys
            .get(pool)
            .map(|keys| keys.iter().any(|k| k == &item.key))
            .unwrap_or(false)
    };

    if !is_series && spec.prefer_outside_boost && !flat.contains(item.key.as_str()) {
        return "Hidden gem".to_string();
    }
    if in_pool("topRated") {
        return "Critically acclaimed".to_string();
    }
    if in_pool("trending") {
        return "Trending pick".to_string();
    }
    if is_series && in_pool("popular") {
        return "Popular now".to_string();
    }
    if let Some(hint) = spec.genre_hints.iter().find(|h| title.contains(h.as_str())) {
        return titlecase_first(hint);
    }
    if item.rating >= 8.0 {
        return "Highly rated".to_string();
    }
    "Worth a look".to_string()
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: i64) -> Self {
        SplitMix64(seed as u64)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_bound(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }
}

/// Fisher-Yates shuffle from the last index down to 1, matching Kotlin's
/// `List.shuffled()` traversal order (see doc comment on tie-break parity above).
fn shuffle<T>(items: &mut [T], seed: i64) {
    let n = items.len();
    if n < 2 {
        return;
    }
    let mut rng = SplitMix64::new(seed);
    for i in (1..n).rev() {
        let j = rng.next_bound(i + 1);
        items.swap(i, j);
    }
}

/// Core of `scoreForConcept`: score every item, keep only those clearing
/// `minMatchScore`, dedupe by key (first occurrence wins), then
/// sort-desc / shuffle / sort-desc / take(12).
pub fn score_for_concept(input: &ScoreInput) -> Vec<ScoredOut> {
    let flat = boost_keys_flat(&input.boost_keys);

    let mut scored: Vec<(usize, i32)> = Vec::new();
    let mut seen_keys: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (idx, item) in input.items.iter().enumerate() {
        let Some(score) = score_item(item, &input.spec, &input.boost_keys, &flat, input.current_year) else {
            continue;
        };
        if score < input.spec.min_match_score {
            continue;
        }
        if !seen_keys.insert(item.key.as_str()) {
            continue;
        }
        scored.push((idx, score));
    }

    scored.sort_by(|a, b| b.1.cmp(&a.1));
    shuffle(&mut scored, input.seed as i64);
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.truncate(ITEMS_PER_RAIL);

    scored
        .into_iter()
        .map(|(idx, score)| {
            let item = &input.items[idx];
            ScoredOut {
                key: item.key.clone(),
                score,
                reason: reason_for(item, &input.spec, &input.boost_keys, &flat, input.is_series),
            }
        })
        .collect()
}

/// Byte-for-byte port of `countMatches`.
pub fn count_matches(input: &CountInput) -> i32 {
    let flat = boost_keys_flat(&input.boost_keys);
    input
        .items
        .iter()
        .filter(|item| {
            score_item(item, &input.spec, &input.boost_keys, &flat, input.current_year)
                .map(|s| s >= input.spec.min_match_score)
                .unwrap_or(false)
        })
        .count() as i32
}

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ThematicRailNativeBridge_nativeScoreForConcept<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let raw = jstring_to_string(env, &input_json);
        let output = match serde_json::from_str::<ScoreInput>(&raw) {
            Ok(input) => ScoreOutput {
                scored: score_for_concept(&input),
            },
            Err(_) => ScoreOutput { scored: Vec::new() },
        };
        let json = serde_json::to_string(&output).unwrap_or_else(|_| "{\"scored\":[]}".to_string());
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ThematicRailNativeBridge_nativeCountMatches<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input_json: JString<'local>,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
        let raw = jstring_to_string(env, &input_json);
        match serde_json::from_str::<CountInput>(&raw) {
            Ok(input) => count_matches(&input),
            Err(_) => 0,
        }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_ThematicRailNativeBridge_nativeEngineVersion<
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

    fn item(key: &str, title: &str, rating: f64, year: Option<i32>, category: &str) -> ItemIn {
        ItemIn {
            key: key.to_string(),
            title: title.to_string(),
            category: category.to_string(),
            year,
            rating,
            has_artwork: true,
        }
    }

    fn halloween_spec() -> ConceptSpecIn {
        ConceptSpecIn {
            keywords: vec![
                "halloween", "horror", "haunted", "vampire", "zombie", "witch",
                "nightmare", "frankenstein", "dracula", "exorcist", "scream", "terror",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            genre_hints: vec!["horror", "thriller", "mystery"].into_iter().map(String::from).collect(),
            category_hints: vec!["horror", "thriller"].into_iter().map(String::from).collect(),
            min_year: None,
            max_year: None,
            min_rating: 0.0,
            prefer_outside_boost: false,
            boost_pools: vec![],
            min_match_score: 80,
            signal_from_era: false,
            signal_from_quality: false,
        }
    }

    // Mirrors ThematicRailMatcherTest.scoreForConcept_halloweenRail_prefersHorrorTitles.
    #[test]
    fn halloween_rail_prefers_horror_titles() {
        let items = vec![
            item("1", "Halloween Night Terror", 7.0, Some(2020), ""),
            item("2", "The Godfather", 9.0, Some(2020), ""),
            item("3", "Dracula Returns", 6.8, Some(2020), ""),
            item("4", "Romantic Comedy Date", 7.2, Some(2020), "romance"),
            item("5", "Zombie Apocalypse", 7.1, Some(2020), ""),
            item("6", "Quiet Drama", 8.5, Some(2020), ""),
            item("7", "Haunted House", 6.9, Some(2020), ""),
            item("8", "Horror in the Woods", 7.0, Some(2020), "horror"),
            item("9", "Vampire Chronicles", 7.2, Some(2020), ""),
            item("10", "Nightmare on Elm", 7.4, Some(2020), ""),
        ];
        let input = ScoreInput {
            items,
            spec: halloween_spec(),
            boost_keys: HashMap::new(),
            seed: 42,
            current_year: Some(2026),
            is_series: false,
        };
        let scored = score_for_concept(&input);
        assert!(scored.len() >= 6);
        assert!(scored.iter().all(|s| s.key != "2"));
    }

    // Mirrors ThematicRailMatcherTest.scoreForConcept_summerBlockbusters_boostsTrendingPool.
    #[test]
    fn summer_blockbusters_boosts_trending_pool() {
        let mut items = vec![
            item("t1", "Mission Impossible Fallout", 8.0, Some(2018), ""),
            item("r1", "Obscure Indie Drama", 6.0, Some(2015), ""),
        ];
        for i in 1..=10 {
            items.push(item(&format!("a{i}"), &format!("Action Movie {i}"), 7.0, Some(2020), "action"));
        }
        let spec = ConceptSpecIn {
            keywords: vec!["mission impossible", "blockbuster"].into_iter().map(String::from).collect(),
            genre_hints: vec!["action", "adventure"].into_iter().map(String::from).collect(),
            category_hints: vec!["action"].into_iter().map(String::from).collect(),
            min_year: Some(2011),
            max_year: None,
            min_rating: 0.0,
            prefer_outside_boost: false,
            boost_pools: vec!["trending".to_string()],
            min_match_score: 90,
            signal_from_era: false,
            signal_from_quality: false,
        };
        let mut boost_keys = HashMap::new();
        boost_keys.insert("trending".to_string(), vec!["t1".to_string()]);
        let input = ScoreInput {
            items,
            spec,
            boost_keys,
            seed: 7,
            current_year: Some(2026),
            is_series: false,
        };
        let scored = score_for_concept(&input);
        assert_eq!(scored.first().map(|s| s.key.as_str()), Some("t1"));
    }

    // Mirrors SeriesThematicRailMatcherTest.comedyConcept_matchesComedyTitles.
    #[test]
    fn series_comedy_concept_matches_comedy_titles() {
        let items = vec![
            item("src1:1", "Parks and Recreation", 8.6, None, "comedy"),
            item("src1:2", "The Dark Knight", 9.0, None, "action"),
        ];
        let spec = ConceptSpecIn {
            keywords: vec!["comedy", "funny", "sitcom", "laugh", "humor", "humour"]
                .into_iter()
                .map(String::from)
                .collect(),
            genre_hints: vec!["comedy", "sitcom"].into_iter().map(String::from).collect(),
            category_hints: vec!["comedy", "sitcom"].into_iter().map(String::from).collect(),
            min_year: None,
            max_year: None,
            min_rating: 6.0,
            prefer_outside_boost: false,
            boost_pools: vec!["trending".to_string(), "popular".to_string()],
            min_match_score: 90,
            signal_from_era: false,
            signal_from_quality: false,
        };
        let input = ScoreInput {
            items,
            spec,
            boost_keys: HashMap::new(),
            seed: 42,
            current_year: None,
            is_series: true,
        };
        let scored = score_for_concept(&input);
        assert!(!scored.is_empty());
        assert_eq!(scored.first().map(|s| s.key.as_str()), Some("src1:1"));
    }

    // Mirrors SeriesThematicRailMatcherTest.countMatches_respectsReservedIds (reservedIds
    // filtering itself is caller-side here, so this only checks the raw count).
    #[test]
    fn count_matches_counts_qualifying_series() {
        let items = vec![
            item("src1:1", "Breaking Bad", 9.5, None, "drama"),
            item("src1:2", "Better Call Saul", 8.8, None, "drama"),
        ];
        let spec = ConceptSpecIn {
            keywords: vec!["drama", "prestige", "limited series", "miniseries"]
                .into_iter()
                .map(String::from)
                .collect(),
            genre_hints: vec!["drama"].into_iter().map(String::from).collect(),
            category_hints: vec!["drama"].into_iter().map(String::from).collect(),
            min_year: None,
            max_year: None,
            min_rating: 6.5,
            prefer_outside_boost: false,
            boost_pools: vec!["topRated".to_string(), "trending".to_string()],
            min_match_score: 100,
            signal_from_era: false,
            signal_from_quality: false,
        };
        let input = CountInput {
            items,
            spec,
            boost_keys: HashMap::new(),
            current_year: None,
        };
        assert!(count_matches(&input) >= 1);
    }

    #[test]
    fn no_artwork_items_are_excluded() {
        let mut no_art = item("1", "Halloween Night", 7.0, Some(2020), "horror");
        no_art.has_artwork = false;
        let input = ScoreInput {
            items: vec![no_art],
            spec: halloween_spec(),
            boost_keys: HashMap::new(),
            seed: 1,
            current_year: Some(2026),
            is_series: false,
        };
        assert!(score_for_concept(&input).is_empty());
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
