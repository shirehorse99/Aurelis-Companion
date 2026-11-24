//! Native port of `EpgArtworkEnricher`'s two self-contained pure-compute steps
//! (`core/domain` `EpgArtworkEnricher.kt`).
//!
//! `EpgArtworkEnricher.resolve` is almost entirely network I/O: chained
//! TMDB/TVDB/Fanart HTTP calls, an in-memory LRU cache, a miss-TTL set, and a
//! search-rate-limiting mutex, none of which belong in a pure compute crate.
//! Only the two steps with zero network/cache dependency are ported:
//!
//! 1. `parse_season_episode_title` -- the `\bs(\d{1,2})e(\d{1,2})\b` regex parse
//!    that splits a programme title like `"Breaking Bad - S03E07"` into
//!    `(season, episode, series_name)` before the per-episode TMDB lookup.
//! 2. `pick_best_backdrop` -- the "prefer a textless (`iso_639_1` blank) backdrop,
//!    else highest `vote_average`" selection run over an already-fetched TMDB
//!    images response.
//!
//! Everything else (the HTTP calls themselves, `TitleMatcher.normalise`, the
//! cache/miss-set, Fanart clearlogo/clearart chains) stays Kotlin-side. Owns no
//! long-lived state. Not wired in as the app's active enricher anywhere --
//! `EpgArtworkEnricher`'s own Kotlin implementation remains the only one
//! actually selected, mirroring `rust/channel-merge`'s dormant status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::jstring;
use jni::Env;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Bump when the JSON snapshot/result shape changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

fn season_episode_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bs(\d{1,2})e(\d{1,2})\b").unwrap())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeasonEpisodeMatch {
    pub season: u32,
    pub episode: u32,
    pub series_name: String,
}

fn remove_suffix<'a>(s: &'a str, suffix: &str) -> &'a str {
    s.strip_suffix(suffix).unwrap_or(s)
}

/// Byte-for-byte port of the SxxExx parse block at the top of `resolve`: finds the
/// first `SxxExx` marker, then derives the series name from everything before it
/// (trim -> drop one trailing `-` -> drop one trailing `:` -> trim again). Returns
/// `None` when there's no match or the derived series name is blank (both cases
/// where Kotlin falls through to the regular TMDB/TVDB search instead).
pub fn parse_season_episode_title(programme_title: &str) -> Option<SeasonEpisodeMatch> {
    let mat = season_episode_regex().captures(programme_title)?;
    let season: u32 = mat.get(1)?.as_str().parse().ok()?;
    let episode: u32 = mat.get(2)?.as_str().parse().ok()?;
    let whole = mat.get(0)?;

    let trimmed = programme_title[..whole.start()].trim();
    let after_dash = remove_suffix(trimmed, "-");
    let after_colon = remove_suffix(after_dash, ":");
    let series_name = after_colon.trim();
    if series_name.is_empty() {
        return None;
    }
    Some(SeasonEpisodeMatch { season, episode, series_name: series_name.to_string() })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackdropCandidate {
    pub file_path: Option<String>,
    pub iso_639_1: Option<String>,
    pub vote_average: f64,
}

/// Byte-for-byte port of the clean-backdrop selection in `searchTmdb`: prefer a
/// textless backdrop (blank/missing `iso_639_1`), falling back to the full list;
/// within that pool pick the highest `vote_average`, keeping the first candidate
/// on ties (matches Kotlin's `maxByOrNull`, which only replaces on strictly
/// greater values); if the chosen pool has none with a `vote_average` (i.e. is
/// empty), fall back to the first candidate with a non-null `file_path`.
pub fn pick_best_backdrop(backdrops: &[BackdropCandidate]) -> Option<String> {
    let with_path: Vec<&BackdropCandidate> = backdrops.iter().filter(|b| b.file_path.is_some()).collect();
    let clean: Vec<&BackdropCandidate> = with_path
        .iter()
        .filter(|b| b.iso_639_1.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true))
        .copied()
        .collect();
    let pool: &[&BackdropCandidate] = if !clean.is_empty() { &clean } else { &with_path };

    let chosen = pool.iter().fold(None::<&&BackdropCandidate>, |acc, b| match acc {
        None => Some(b),
        Some(cur) => if b.vote_average > cur.vote_average { Some(b) } else { acc },
    });
    chosen
        .copied()
        .or_else(|| with_path.first().copied())
        .and_then(|b| b.file_path.clone())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackdropListInput {
    backdrops: Vec<BackdropCandidate>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackdropOutput {
    file_path: Option<String>,
}

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`. Returns
/// an empty string when there's no `SxxExx` match (Kotlin's `null` case).
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgArtworkNativeBridge_nativeParseSeasonEpisodeTitle<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    programme_title: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let title = jstring_to_string(env, &programme_title);
        let json = match parse_season_episode_title(&title) {
            Some(m) => serde_json::to_string(&m).unwrap_or_default(),
            None => String::new(),
        };
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgArtworkNativeBridge_nativePickBestBackdrop<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    input_json: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let raw = jstring_to_string(env, &input_json);
        let file_path = serde_json::from_str::<BackdropListInput>(&raw)
            .ok()
            .and_then(|input| pick_best_backdrop(&input.backdrops));
        let json = serde_json::to_string(&BackdropOutput { file_path }).unwrap_or_else(|_| "{\"filePath\":null}".to_string());
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_interactor_EpgArtworkNativeBridge_nativeEngineVersion<'local>(
    _env: EnvUnowned,
    _class: JClass<'local>,
) -> jni::sys::jint {
    ENGINE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_series_name_and_season_episode() {
        let result = parse_season_episode_title("Breaking Bad - S03E07").unwrap();
        assert_eq!(result.season, 3);
        assert_eq!(result.episode, 7);
        assert_eq!(result.series_name, "Breaking Bad");
    }

    #[test]
    fn strips_trailing_colon_after_dash_removal_only_when_exposed() {
        // "Show:" -> trim -> no trailing "-" to remove -> removeSuffix(":") DOES fire
        // since ":" is now the exposed trailing char.
        let result = parse_season_episode_title("Show: S01E01").unwrap();
        assert_eq!(result.series_name, "Show");
    }

    #[test]
    fn dash_removal_can_mask_a_colon_behind_a_space() {
        // "Show -" -> trim -> removeSuffix("-") -> "Show " (trailing space stays,
        // the "-" is gone but nothing after it is retrimmed) -> removeSuffix(":")
        // doesn't match "Show " -> final trim -> "Show". No colon was ever present
        // here, this just documents the exact multi-step order of operations.
        let result = parse_season_episode_title("Show - S01E01").unwrap();
        assert_eq!(result.series_name, "Show");
    }

    #[test]
    fn is_case_insensitive_and_requires_word_boundaries() {
        assert!(parse_season_episode_title("Some Show s1e2 Extra").is_some());
        assert!(parse_season_episode_title("Some Show S12E34").is_some());
        assert!(parse_season_episode_title("Nostalgia").is_none());
    }

    #[test]
    fn returns_none_when_series_name_is_blank() {
        assert!(parse_season_episode_title("S01E01").is_none());
    }

    #[test]
    fn picks_textless_backdrop_over_text_backdrop_by_vote_average() {
        let backdrops = vec![
            BackdropCandidate { file_path: Some("/text.jpg".into()), iso_639_1: Some("en".into()), vote_average: 9.0 },
            BackdropCandidate { file_path: Some("/clean-low.jpg".into()), iso_639_1: None, vote_average: 2.0 },
            BackdropCandidate { file_path: Some("/clean-high.jpg".into()), iso_639_1: Some("".into()), vote_average: 5.0 },
        ];
        assert_eq!(pick_best_backdrop(&backdrops), Some("/clean-high.jpg".to_string()));
    }

    #[test]
    fn falls_back_to_full_list_when_no_textless_backdrop_exists() {
        let backdrops = vec![
            BackdropCandidate { file_path: Some("/a.jpg".into()), iso_639_1: Some("en".into()), vote_average: 3.0 },
            BackdropCandidate { file_path: Some("/b.jpg".into()), iso_639_1: Some("fr".into()), vote_average: 7.0 },
        ];
        assert_eq!(pick_best_backdrop(&backdrops), Some("/b.jpg".to_string()));
    }

    #[test]
    fn keeps_first_candidate_on_vote_average_ties() {
        let backdrops = vec![
            BackdropCandidate { file_path: Some("/first.jpg".into()), iso_639_1: None, vote_average: 5.0 },
            BackdropCandidate { file_path: Some("/second.jpg".into()), iso_639_1: None, vote_average: 5.0 },
        ];
        assert_eq!(pick_best_backdrop(&backdrops), Some("/first.jpg".to_string()));
    }

    #[test]
    fn returns_none_when_no_candidate_has_a_file_path() {
        let backdrops = vec![BackdropCandidate { file_path: None, iso_639_1: None, vote_average: 9.0 }];
        assert_eq!(pick_best_backdrop(&backdrops), None);
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}


/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
