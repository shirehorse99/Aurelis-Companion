//! Native scoring primitives for Aurelis's `core/domain` `SmartCollectionInteractor`.
//!
//! Ports the three reusable pure-compute pieces shared by `buildRememberingItems`,
//! `buildOnThisDayItems`, and `buildFamousScenesItems`: the `stableScore` hash,
//! `parseMonthDay`'s three date-format regexes, and the
//! `distinctBy(id) -> map(stableScore) -> sortedBy(score) -> filter(usedIds.add) ->
//! take(limit)` dedupe/rank pipeline all three functions share.
//!
//! `HomeCollectionItem`/`HomeCollectionKind` construction, the virtual-channel
//! keyword pools, the rotating-genre-rail category grouping, and `buildMasterpieces`'s
//! rating-string sort stay Kotlin-side -- they're app-specific DTO shaping rather than
//! CPU-bound compute, mirroring how `ChannelNameKeys` stays Kotlin-side for
//! `rust/channel-merge`.
//!
//! Owns no long-lived state: one call receives a batch of (id, title) pairs and
//! returns the ranked/deduped index order. Not wired in as the app's active scoring
//! logic anywhere -- `SmartCollectionInteractor` remains the only implementation
//! actually selected, mirroring `rust/channel-merge`'s dormant status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::Env;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::OnceLock;

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

/// Byte-for-byte port of `stableScore` (SmartCollectionInteractor.kt): Java/Kotlin
/// `Int` arithmetic wraps silently on overflow, so every step uses `wrapping_*`.
/// Iterates UTF-16 code units (`Char.code` in Kotlin), not Unicode scalars, to match
/// Kotlin's `String.forEach` exactly for non-BMP titles.
pub fn stable_score(title: &str, seed: i32) -> i32 {
    let mut hash = seed.wrapping_mul(31);
    for unit in title.encode_utf16() {
        hash = 31i32.wrapping_mul(hash).wrapping_add(unit as i32);
    }
    hash
}

fn month_day_regexes() -> &'static (Regex, Regex, Regex) {
    static REGEXES: OnceLock<(Regex, Regex, Regex)> = OnceLock::new();
    REGEXES.get_or_init(|| {
        (
            Regex::new(r"(\d{4})-(\d{2})-(\d{2})").unwrap(),
            Regex::new(r"(\d{2})/(\d{2})/(\d{4})").unwrap(),
            Regex::new(r"(\d{2})-(\d{2})-(\d{4})").unwrap(),
        )
    })
}

/// Byte-for-byte port of `parseMonthDay` (SmartCollectionInteractor.kt). Returns
/// `(month, day)`. The third pattern (`DD-MM-YYYY`) mirrors the existing Kotlin
/// variable-swap exactly: it's tested, shipped behaviour, not a bug to "fix" here.
pub fn parse_month_day(raw: &str) -> Option<(i32, i32)> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    let (iso, slash, dashed) = month_day_regexes();

    if let Some(m) = iso.captures(value) {
        let month: i32 = m.get(2)?.as_str().parse().ok()?;
        let day: i32 = m.get(3)?.as_str().parse().ok()?;
        return Some((month, day));
    }
    if let Some(m) = slash.captures(value) {
        let month: i32 = m.get(1)?.as_str().parse().ok()?;
        let day: i32 = m.get(2)?.as_str().parse().ok()?;
        return Some((month, day));
    }
    if let Some(m) = dashed.captures(value) {
        let day: i32 = m.get(1)?.as_str().parse().ok()?;
        let month: i32 = m.get(2)?.as_str().parse().ok()?;
        return Some((month, day));
    }
    None
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankItemIn {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankInput {
    pub items: Vec<RankItemIn>,
    pub seed: i32,
    #[serde(default)]
    pub used_ids: Vec<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RankOutput {
    /// Indices into the input `items` list (first occurrence per id), in final
    /// dedupe -> rank -> usedIds-filter -> take(limit) order.
    pub indices: Vec<usize>,
}

/// Byte-for-byte port of the `distinctBy(id).map(stableScore).sortedBy(score)
/// .filter(usedIds.add).take(limit)` pipeline shared by `buildRememberingItems`,
/// `buildOnThisDayItems`, and `buildFamousScenesItems`.
pub fn rank_and_dedupe(input: &RankInput) -> Vec<usize> {
    let mut seen_ids: HashSet<&str> = HashSet::new();
    let mut deduped: Vec<usize> = Vec::new();
    for (i, item) in input.items.iter().enumerate() {
        if seen_ids.insert(item.id.as_str()) {
            deduped.push(i);
        }
    }

    let mut scored: Vec<(usize, i32)> = deduped
        .into_iter()
        .map(|i| (i, stable_score(&input.items[i].title, input.seed)))
        .collect();
    scored.sort_by_key(|&(_, score)| score);

    let mut used: HashSet<&str> = input.used_ids.iter().map(|s| s.as_str()).collect();
    let mut result = Vec::new();
    for (i, _) in scored {
        if result.len() >= input.limit {
            break;
        }
        let id = input.items[i].id.as_str();
        if used.insert(id) {
            result.push(i);
        }
    }
    result
}

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_SmartCollectionNativeBridge_nativeRankAndDedupe<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let raw = jstring_to_string(env, &input_json);
        let output = match serde_json::from_str::<RankInput>(&raw) {
            Ok(input) => RankOutput { indices: rank_and_dedupe(&input) },
            Err(_) => RankOutput { indices: Vec::new() },
        };
        let json = serde_json::to_string(&output).unwrap_or_else(|_| "{\"indices\":[]}".to_string());
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_SmartCollectionNativeBridge_nativeEngineVersion<
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

    #[test]
    fn stable_score_is_deterministic() {
        assert_eq!(stable_score("The Godfather", 1234), stable_score("The Godfather", 1234));
    }

    #[test]
    fn stable_score_differs_across_seeds() {
        assert_ne!(stable_score("The Godfather", 1), stable_score("The Godfather", 2));
    }

    #[test]
    fn stable_score_handles_overflow_without_panicking() {
        // Long title + large seed pushes the running hash well past i32::MAX repeatedly;
        // wrapping arithmetic must not panic (Kotlin's Int silently wraps too).
        let long_title = "A".repeat(500);
        let _ = stable_score(&long_title, i32::MAX);
    }

    #[test]
    fn parse_month_day_iso_format() {
        assert_eq!(parse_month_day("2011-01-15"), Some((1, 15)));
    }

    #[test]
    fn parse_month_day_slash_format() {
        assert_eq!(parse_month_day("03/25/2019"), Some((3, 25)));
    }

    #[test]
    fn parse_month_day_dashed_ddmmyyyy_format() {
        // "05-12-2020" is DD-MM-YYYY: day=05, month=12.
        assert_eq!(parse_month_day("05-12-2020"), Some((12, 5)));
    }

    #[test]
    fn parse_month_day_blank_or_unmatched_returns_none() {
        assert_eq!(parse_month_day(""), None);
        assert_eq!(parse_month_day("unknown"), None);
    }

    #[test]
    fn rank_and_dedupe_drops_duplicate_ids_and_respects_used_ids() {
        let input = RankInput {
            items: vec![
                RankItemIn { id: "1".into(), title: "Alpha".into() },
                RankItemIn { id: "1".into(), title: "Alpha Duplicate".into() },
                RankItemIn { id: "2".into(), title: "Beta".into() },
                RankItemIn { id: "3".into(), title: "Gamma".into() },
            ],
            seed: 42,
            used_ids: vec!["2".to_string()],
            limit: 10,
        };
        let result = rank_and_dedupe(&input);
        // "1" (first occurrence only) and "3" survive; "2" is pre-used, duplicate "1" dropped.
        assert_eq!(result.len(), 2);
        assert!(result.contains(&0));
        assert!(result.contains(&3));
        assert!(!result.contains(&1));
    }

    #[test]
    fn rank_and_dedupe_respects_limit() {
        let items: Vec<RankItemIn> = (0..20)
            .map(|i| RankItemIn { id: i.to_string(), title: format!("Title {i}") })
            .collect();
        let input = RankInput { items, seed: 7, used_ids: vec![], limit: 5 };
        assert_eq!(rank_and_dedupe(&input).len(), 5);
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
