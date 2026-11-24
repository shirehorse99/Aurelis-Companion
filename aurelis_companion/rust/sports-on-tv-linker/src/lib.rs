//! Native sports-on-TV EPG linker for Aurelis's `core/domain` sport logic.
//!
//! Ports four pure-compute pieces:
//!  * `SportsOnTvEpgLinker.parseTeams` — split event name on ` vs ` / ` v ` /
//!    ` @ ` / ` - ` / ` – ` into (home, away), lowercased.
//!  * `SportsOnTvEpgLinker.programCoversListing` — check if a listed start
//!    time falls within a programme's airing window (±45 min tolerance).
//!  * `SportsOnTvEpgLinker.programMatchesEvent` — if both teams parsed,
//!    delegate to fixture text matching; else match the longest ≥4-char token
//!    of the event name against the programme text blob.
//!  * `SportsFixtureEpgMatcher.programTextMatchesFixture` — check if both
//!    teams (or their aliases) appear in the programme title/description blob.
//!  * `SportsAliasRepository.matches` — true if the official name or any alias
//!    is contained in the query text.
//!
//! Owns no long-lived state: each call is independent. Not wired in as the
//! active linker anywhere — the Kotlin implementations remain the only ones
//! actually selected, mirroring `rust/bitrate-calc`'s dormant status.

use std::sync::OnceLock;

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jlong, jstring};
use jni::Env;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Bump when the FFI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

/// Listing times and EPG starts rarely line up exactly; accept a generous window.
const START_TOLERANCE_MS: i64 = 45 * 60 * 1000; // 45 minutes

static SPLIT_REGEX: OnceLock<Regex> = OnceLock::new();

fn split_regex() -> &'static Regex {
    // Kotlin: Regex("""\s+(?:vs?\.?|v|@|-|–)\s+""", RegexOption.IGNORE_CASE)
    // Note: the en-dash – (U+2013) is a literal in the pattern.
    SPLIT_REGEX.get_or_init(|| Regex::new(r"(?i)\s+(?:vs?\.?|v|@|-|–)\s+").unwrap())
}

/// JSON output struct for `parse_teams`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TeamsOut {
    home: String,
    away: String,
}

/// JSON input struct for fixture aliases.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureAliases {
    #[serde(default)]
    home_alias: Vec<String>,
    #[serde(default)]
    away_alias: Vec<String>,
}

// ---------------------------------------------------------------------------
// Pure-logic ports
// ---------------------------------------------------------------------------

/// Byte-for-byte port of `SportsOnTvEpgLinker.parseTeams`: split on the
/// `SPLIT_REGEX` (limit 2), return (home, away) lowercased. Returns a blank
/// pair when no split is found or either side is blank.
pub fn parse_teams(event_name: &str) -> (String, String) {
    let re = split_regex();
    let parts: Vec<&str> = re.splitn(event_name.trim(), 2).collect();
    if parts.len() != 2 {
        return (String::new(), String::new());
    }
    let home = parts[0].trim().to_lowercase();
    let away = parts[1].trim().to_lowercase();
    if home.is_empty() || away.is_empty() {
        return (String::new(), String::new());
    }
    (home, away)
}

/// Byte-for-byte port of `SportsOnTvEpgLinker.programCoversListing`: true
/// when `listed_start_ms` falls within `[start_ms - tol, end_ms + tol]`.
/// Returns true (no gating) when times are missing or zero.
pub fn program_covers_listing(start_ms: i64, end_ms: i64, listed_start_ms: i64) -> bool {
    if listed_start_ms <= 0 {
        return true;
    }
    if start_ms <= 0 || end_ms <= 0 {
        return true;
    }
    listed_start_ms >= (start_ms - START_TOLERANCE_MS) && listed_start_ms <= (end_ms + START_TOLERANCE_MS)
}

/// Byte-for-byte port of `SportsAliasRepository.matches`: true if the
/// lowercased official name or any (already-lowercased) alias is contained
/// in the lowercased query.
pub fn matches(official_name: &str, query: &str, aliases: &[String]) -> bool {
    let q = query.to_lowercase();
    let official = official_name.to_lowercase();
    if q.contains(&official) {
        return true;
    }
    aliases.iter().any(|alias| q.contains(alias))
}

/// Build the lowercased text blob from programme title and optional
/// description (description appended only when not blank). Mirrors the
/// `buildString { append(title); description?.let { append(' ').append(it) } }.lowercase()`
/// pattern used by both `SportsOnTvEpgLinker.programMatchesEvent` and
/// `SportsFixtureEpgMatcher.programTextMatchesFixture`.
fn build_blob(title: &str, description: Option<&str>) -> String {
    let mut blob = String::new();
    blob.push_str(title);
    if let Some(desc) = description {
        if !desc.trim().is_empty() {
            blob.push(' ');
            blob.push_str(desc);
        }
    }
    blob.to_lowercase()
}

/// Parse the aliases JSON `{"homeAlias":[...], "awayAlias":[...]}` (or empty
/// string) into separate alias vectors.
fn parse_fixture_aliases(json: &str) -> (Vec<String>, Vec<String>) {
    if json.is_empty() {
        return (Vec::new(), Vec::new());
    }
    match serde_json::from_str::<FixtureAliases>(json) {
        Ok(a) => (a.home_alias, a.away_alias),
        Err(_) => (Vec::new(), Vec::new()),
    }
}

/// Byte-for-byte port of `SportsFixtureEpgMatcher.programTextMatchesFixture`:
/// both home and away (or their aliases) must appear in the programme text blob.
pub fn program_text_matches_fixture(
    program_title: &str,
    program_description: Option<&str>,
    home: &str,
    away: &str,
    aliases_json: &str,
) -> bool {
    let (home_aliases, away_aliases) = parse_fixture_aliases(aliases_json);
    let blob = build_blob(program_title, program_description);
    let home_match = matches(home, &blob, &home_aliases);
    let away_match = matches(away, &blob, &away_aliases);
    home_match && away_match
}

/// Byte-for-byte port of `SportsOnTvEpgLinker.programMatchesEvent`: when both
/// home and away are non-blank, delegate to `program_text_matches_fixture`.
/// Otherwise, find the longest ≥4-char token of the event name and check if
/// it appears in the programme text blob.
pub fn program_matches_event(
    program_title: &str,
    program_description: Option<&str>,
    event_name: &str,
    home: &str,
    away: &str,
    aliases_json: &str,
) -> bool {
    if !home.trim().is_empty() && !away.trim().is_empty() {
        return program_text_matches_fixture(
            program_title,
            program_description,
            home,
            away,
            aliases_json,
        );
    }

    // Single-competitor events: match the longest >=4-char token.
    let blob = build_blob(program_title, program_description);
    let lowered = event_name.trim().to_lowercase();
    let anchor = lowered
        .split(' ')
        .filter(|t| t.len() >= 4)
        .max_by_key(|t| t.len());

    match anchor {
        Some(a) => blob.contains(a),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// JNI boundary
// ---------------------------------------------------------------------------

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

/// # Safety
/// Called only by the JVM via JNI. Returns JSON `{"home":"...","away":"..."}`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_sport_SportsOnTvLinkerNativeBridge_nativeParseTeams<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    event_name: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let name = jstring_to_string(env, &event_name);
        let (home, away) = parse_teams(&name);
        let out = TeamsOut { home, away };
        let json = serde_json::to_string(&out).unwrap_or_default();
        env.new_string(json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_sport_SportsOnTvLinkerNativeBridge_nativeProgramCoversListing<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
    start_ms: jlong,
    end_ms: jlong,
    listed_start_ms: jlong,
) -> jboolean {
    if program_covers_listing(start_ms, end_ms, listed_start_ms) {
        true
    } else {
        false
    }
}

/// # Safety
/// Called only by the JVM via JNI. `program_description` is an empty string
/// when the programme has no description (mapped to `None` internally).
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_sport_SportsOnTvLinkerNativeBridge_nativeProgramMatchesEvent<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    program_title: JString<'local>,
    program_description: JString<'local>,
    event_name: JString<'local>,
    home: JString<'local>,
    away: JString<'local>,
    aliases_json: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let title = jstring_to_string(env, &program_title);
        let desc_str = jstring_to_string(env, &program_description);
        let desc = if desc_str.is_empty() { None } else { Some(desc_str.as_str()) };
        let event = jstring_to_string(env, &event_name);
        let home_str = jstring_to_string(env, &home);
        let away_str = jstring_to_string(env, &away);
        let aliases = jstring_to_string(env, &aliases_json);

        if program_matches_event(&title, desc, &event, &home_str, &away_str, &aliases) {
            true
        } else {
            false
        }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI. `aliases_json` is a JSON array of alias
/// strings (already lowercased by the Kotlin side).
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_sport_SportsOnTvLinkerNativeBridge_nativeMatches<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    official_name: JString<'local>,
    query: JString<'local>,
    aliases_json: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let official = jstring_to_string(env, &official_name);
        let q = jstring_to_string(env, &query);
        let aliases_json_str = jstring_to_string(env, &aliases_json);
        let aliases: Vec<String> = if aliases_json_str.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&aliases_json_str).unwrap_or_default()
        };

        if matches(&official, &q, &aliases) {
            true
        } else {
            false
        }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_sport_SportsOnTvLinkerNativeBridge_nativeEngineVersion<
    'local,
>(
    _env: EnvUnowned,
    _class: JClass<'local>,
) -> jint {
    ENGINE_VERSION
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_teams ---

    #[test]
    fn parse_teams_splits_on_vs() {
        let (home, away) = parse_teams("Arsenal vs Aston Villa");
        assert_eq!(home, "arsenal");
        assert_eq!(away, "aston villa");
    }

    #[test]
    fn parse_teams_splits_on_v() {
        let (home, away) = parse_teams("TeamA v TeamB");
        assert_eq!(home, "teama");
        assert_eq!(away, "teamb");
    }

    #[test]
    fn parse_teams_splits_on_at_sign() {
        let (home, away) = parse_teams("Lakers @ Celtics");
        assert_eq!(home, "lakers");
        assert_eq!(away, "celtics");
    }

    #[test]
    fn parse_teams_splits_on_hyphen() {
        let (home, away) = parse_teams("TeamA - TeamB");
        assert_eq!(home, "teama");
        assert_eq!(away, "teamb");
    }

    #[test]
    fn parse_teams_splits_on_en_dash() {
        let (home, away) = parse_teams("TeamA \u{2013} TeamB");
        assert_eq!(home, "teama");
        assert_eq!(away, "teamb");
    }

    #[test]
    fn parse_teams_splits_on_vs_with_dot() {
        let (home, away) = parse_teams("TeamA vs. TeamB");
        assert_eq!(home, "teama");
        assert_eq!(away, "teamb");
    }

    #[test]
    fn parse_teams_returns_blank_for_no_separator() {
        let (home, away) = parse_teams("British Grand Prix");
        assert_eq!(home, "");
        assert_eq!(away, "");
    }

    #[test]
    fn parse_teams_returns_blank_when_either_side_blank() {
        let (home, away) = parse_teams(" vs TeamB");
        assert_eq!(home, "");
        assert_eq!(away, "");
    }

    #[test]
    fn parse_teams_case_insensitive_separator() {
        let (home, away) = parse_teams("TeamA VS TeamB");
        assert_eq!(home, "teama");
        assert_eq!(away, "teamb");
    }

    #[test]
    fn parse_teams_limit_2_keeps_remainder() {
        let (home, away) = parse_teams("A vs B vs C");
        assert_eq!(home, "a");
        assert_eq!(away, "b vs c");
    }

    // --- program_covers_listing ---

    #[test]
    fn covers_listing_within_tolerance() {
        let start = 1_000_000_000_000_i64;
        let end = start + 2 * 60 * 60 * 1000; // 2h programme
        let listed = start + 30 * 60 * 1000; // 30 min into programme
        assert!(program_covers_listing(start, end, listed));
    }

    #[test]
    fn covers_listing_outside_tolerance() {
        let start = 1_000_000_000_000_i64;
        let end = start + 2 * 60 * 60 * 1000;
        let listed = start - 2 * 60 * 60 * 1000; // 2h before — outside 45min tolerance
        assert!(!program_covers_listing(start, end, listed));
    }

    #[test]
    fn covers_listing_returns_true_when_listed_start_zero() {
        assert!(program_covers_listing(1000, 2000, 0));
    }

    #[test]
    fn covers_listing_returns_true_when_program_times_zero() {
        assert!(program_covers_listing(0, 0, 1000));
        assert!(program_covers_listing(1000, 0, 1000));
    }

    #[test]
    fn covers_listing_at_boundary_inclusive() {
        let start = 1_000_000_000_000_i64;
        let end = start + 60_000;
        let tol = START_TOLERANCE_MS;
        assert!(program_covers_listing(start, end, start - tol)); // exact lower bound
        assert!(program_covers_listing(start, end, end + tol)); // exact upper bound
        assert!(!program_covers_listing(start, end, start - tol - 1)); // just outside
    }

    // --- matches ---

    #[test]
    fn matches_official_name_in_query() {
        assert!(matches("arsenal", "live: arsenal vs chelsea", &[]));
    }

    #[test]
    fn matches_alias_in_query() {
        let aliases = vec!["gunners".to_string()];
        assert!(matches("arsenal", "the gunners play today", &aliases));
    }

    #[test]
    fn matches_returns_false_when_neither_found() {
        assert!(!matches("arsenal", "liverpool vs chelsea", &[]));
    }

    #[test]
    fn matches_is_case_insensitive() {
        assert!(matches("ARSENAL", "Arsenal vs Chelsea", &[]));
    }

    // --- program_text_matches_fixture ---

    #[test]
    fn fixture_matches_when_both_teams_in_title() {
        let aliases = r#"{"homeAlias":[],"awayAlias":[]}"#;
        assert!(program_text_matches_fixture(
            "Arsenal vs Chelsea",
            None,
            "arsenal",
            "chelsea",
            aliases,
        ));
    }

    #[test]
    fn fixture_matches_with_aliases() {
        let aliases = r#"{"homeAlias":["gunners"],"awayAlias":["blues"]}"#;
        assert!(program_text_matches_fixture(
            "The Gunners host The Blues",
            None,
            "arsenal",
            "chelsea",
            aliases,
        ));
    }

    #[test]
    fn fixture_does_not_match_when_one_team_missing() {
        let aliases = r#"{"homeAlias":[],"awayAlias":[]}"#;
        assert!(!program_text_matches_fixture(
            "Arsenal vs Liverpool",
            None,
            "arsenal",
            "chelsea",
            aliases,
        ));
    }

    #[test]
    fn fixture_matches_with_description() {
        let aliases = r#"{"homeAlias":[],"awayAlias":[]}"#;
        assert!(program_text_matches_fixture(
            "Live Football",
            Some("Arsenal take on Chelsea in today's big match"),
            "arsenal",
            "chelsea",
            aliases,
        ));
    }

    #[test]
    fn fixture_empty_aliases_json() {
        assert!(program_text_matches_fixture(
            "Arsenal vs Chelsea",
            None,
            "arsenal",
            "chelsea",
            "",
        ));
    }

    // --- program_matches_event ---

    #[test]
    fn event_match_delegates_to_fixture_when_both_teams_present() {
        let aliases = r#"{"homeAlias":[],"awayAlias":[]}"#;
        assert!(program_matches_event(
            "Arsenal vs Chelsea",
            None,
            "Arsenal vs Chelsea",
            "arsenal",
            "chelsea",
            aliases,
        ));
    }

    #[test]
    fn event_match_uses_longest_token_for_single_competitor() {
        // "British Grand Prix" has no team split; longest >=4 token is "british"
        // (7 chars) vs "grand" (5) vs "prix" (4) → "british"
        assert!(program_matches_event(
            "British Grand Prix Live",
            None,
            "British Grand Prix",
            "",
            "",
            "",
        ));
    }

    #[test]
    fn event_match_returns_false_when_no_long_token() {
        // "F1" → no token >= 4 chars
        assert!(!program_matches_event(
            "Some Programme",
            None,
            "F1",
            "",
            "",
            "",
        ));
    }

    #[test]
    fn event_match_single_competitor_in_description() {
        assert!(program_matches_event(
            "Sports Coverage",
            Some("Live from Wimbledon: British Grand Prix coverage"),
            "British Grand Prix",
            "",
            "",
            "",
        ));
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}
