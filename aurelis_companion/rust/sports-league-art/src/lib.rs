//! Native sport-detection / league-search-query scoring helpers for Aurelis's
//! `core/data` `SportsLeagueArtResolver`.
//!
//! Ports the pure-compute pieces of `SportsLeagueArtResolver.kt` — the
//! `guessSportFromTitle` when-block, `titleImpliesSpecificEvent`,
//! `indexMatchConflicts`, `toV2SearchSlug`, `buildSearchQueries`,
//! `scoreLeagueQueryMatch`, `tokenize`, and the small boolean predicates
//! (`isMotorcycleMotorsportTitle`, `isDragRacingLeague`, `isPoolTitle`,
//! `isTennisGrandSlamTitle`, `extractCompetitionFromTitle`).
//!
//! Every network function (`resolve`, `resolveForTitle`, `resolveForSport`,
//! `searchLeagueByName`, `searchEventByName`, `lookupLeagueById`, `ensureIndex`,
//! `ensureSportBackdrops`, `searchLeagueV2`, `searchIndexByQuery`) stays
//! Kotlin-side — they're Retrofit/OkHttp I/O, not compute. `SportsLeagueArt`
//! DTO construction, the league index, and the `loadMutex` also stay Kotlin.
//!
//! Owns no long-lived state: each call is independent. Not wired in as the
//! app's active art-resolution logic anywhere — `SportsLeagueArtResolver`'s
//! own Kotlin implementations remain the only ones actually selected,
//! mirroring `rust/bitrate-calc`'s dormant status.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jstring};
use jni::Env;
use regex::Regex;
use std::sync::OnceLock;

/// Bump when the FFI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

/// Word-boundary regex matching "uci" as a whole word (Kotlin: `Regex("""\buci\b""")`).
fn uci_word() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\buci\b").unwrap())
}

/// Word-boundary regex matching "afc wimbledon" (Kotlin: `Regex("""\bafc\s+wimbledon\b""")`).
fn afc_wimbledon_football() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bafc\s+wimbledon\b").unwrap())
}

/// Word-boundary regex matching "wimbledon" (Kotlin: `Regex("""\bwimbledon\b""")`).
fn wimbledon_word() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bwimbledon\b").unwrap())
}

/// Prefix-noise regex for `extractCompetitionFromTitle`
/// (Kotlin: `Regex("""^(?:Live[:\s]+)?(?:\d{4}\s+)?(?:The\s+)?""", IGNORE_CASE)`).
fn title_prefix_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)^(?:Live[:\s]+)?(?:\d{4}\s+)?(?:The\s+)?").unwrap())
}

/// Case-insensitive "champions league" finder (used to locate the substring
/// in the original title without going through a lowercased copy, so byte
/// indices stay valid for the subsequent `lastIndexOfAny`-equivalent scan).
fn champions_league_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)champions league").unwrap())
}

/// Slug regex for `toV2SearchSlug` (Kotlin: `Regex("[^a-z0-9]+")`).
fn slug_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[^a-z0-9]+").unwrap())
}

/// Tokenize-split regex for `tokenize` (Kotlin: `Regex("[^a-z0-9]+")`).
fn tokenize_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[^a-z0-9]+").unwrap())
}

const POOL_TITLE_CUES: &[&str] = &["ultimate pool", "pool", "billiard", "snooker"];
const SOCCER_AMBIGUOUS_LEAGUES: &[&str] = &["champions league", "uefa"];

const TITLE_PREFIX_NOISE_WORDS: &[&str] = &[
    "live",
    "watch",
    "breaking",
    "now",
    "next",
    "coming up",
    "highlights",
    "replay",
    "on now",
];

/// Best-guess TheSportsDB sport name from a free-text programme title.
///
/// Byte-for-byte port of `guessSportFromTitle` — the big `when` block.
/// Branch order matters: earlier branches preempt later ones (e.g. "rugby"
/// is checked before "football", "evian" before generic "golf").
pub fn guess_sport_from_title(title: &str) -> Option<String> {
    let t = title.to_lowercase();

    if t.contains("rugby") {
        return Some("Rugby".into());
    }
    if t.contains("cricket") {
        return Some("Cricket".into());
    }
    if t.contains("evian") || (t.contains("lpga") && t.contains("golf")) {
        return Some("Golf".into());
    }
    if t.contains("golf") || t.contains("pga") || t.contains("ryder cup") {
        return Some("Golf".into());
    }
    if is_tennis_grand_slam_title(&t) {
        return Some("Tennis".into());
    }
    if t.contains("tennis")
        || t.contains(" atp ")
        || t.contains(" wta ")
        || t.contains("open tennis")
    {
        return Some("Tennis".into());
    }
    if is_motorcycle_motorsport_title(&t) {
        return Some("Motorsport".into());
    }
    // "formula one" spelled out, plus Formula E and the FIA prefix. Without them
    // "FIA Formula One World Championship" was not recognised as motorsport at all,
    // which left the index free to match it against another sport entirely.
    if t.contains("f1")
        || t.contains("formula 1")
        || t.contains("formula one")
        || t.contains("formula e")
        || t.contains("fia ")
        || t.contains("grand prix")
        || t.contains("motogp")
        || t.contains("nascar")
    {
        return Some("Motorsport".into());
    }
    if t.contains("ufc")
        || t.contains("boxing")
        || t.contains(" mma ")
        || t.contains("title fight")
    {
        return Some("Fighting".into());
    }
    if t.contains("nba") || t.contains("basketball") {
        return Some("Basketball".into());
    }
    if t.contains("nfl") || t.contains("american football") || t.contains("super bowl") {
        return Some("American Football".into());
    }
    if t.contains("nhl") || t.contains("ice hockey") {
        return Some("Ice Hockey".into());
    }
    if t.contains("baseball") || t.contains(" mlb ") {
        return Some("Baseball".into());
    }
    if t.contains("cycl")
        || t.contains("vuelta")
        || t.contains("giro")
        || t.contains("velodrome")
        || t.contains("tour de france")
        || t.contains("criterium")
        || uci_word().is_match(&t)
        || t.contains("mountain bik")
    {
        return Some("Cycling".into());
    }
    if t.contains("ultimate pool") || t.contains("pool") || t.contains("billiard") {
        return Some("Billiards".into());
    }
    if t.contains("snooker") || t.contains("world snooker") {
        return Some("Snooker".into());
    }
    if t.contains("darts") || t.contains("pdc") || t.contains("bdo") {
        return Some("Darts".into());
    }
    if t.contains("swimming") || t.contains("aquatics") {
        return Some("Swimming".into());
    }
    if t.contains("athletics") || t.contains("track and field") || t.contains("marathon") {
        return Some("Athletics".into());
    }
    if t.contains("volleyball") {
        return Some("Volleyball".into());
    }
    if t.contains("football")
        || t.contains("soccer")
        || t.contains("premier league")
        || (t.contains("champions league") && !is_pool_title(&t))
        || t.contains("uefa")
        || t.contains("fifa")
        || t.contains("world cup")
        || t.contains("la liga")
        || t.contains("bundesliga")
        || t.contains("serie a")
        || t.contains("matchday")
        || t.contains("match of the day")
        || t.contains("gillette")
        || t.contains("efl")
        || t.contains("fa cup")
        || t.contains("carabao")
        || t.contains("europa league")
        || t.contains(" vs ")
        || t.contains(" v ")
    {
        return Some("Soccer".into());
    }

    None
}

/// True when a title/competition names a specific event — generic sport
/// wallpaper must not win. Byte-for-byte port of `titleImpliesSpecificEvent`.
pub fn title_implies_specific_event(
    title: &str,
    competition: Option<&str>,
    sport_type: Option<&str>,
) -> bool {
    let t = title.trim();
    let c = competition.unwrap_or("").trim();
    if !c.is_empty() && !c.eq_ignore_ascii_case(sport_type.unwrap_or("")) {
        return true;
    }
    if t.is_empty() {
        return false;
    }
    if t.eq_ignore_ascii_case(sport_type.unwrap_or("")) {
        return false;
    }
    let tl = t.to_lowercase();
    is_pool_title(&tl)
        || is_tennis_grand_slam_title(&tl)
        || [
            "roland garros",
            "australian open",
            "us open",
            "ryder cup",
            "grand prix",
            "world cup",
            "wimbledon",
            "ultimate pool",
        ]
        .iter()
        .any(|s| tl.contains(s))
}

/// Byte-for-byte port of `indexMatchConflicts`. Inputs are already-lowercased
/// by the caller (same as Kotlin).
pub fn index_match_conflicts(
    title_lower: &str,
    league_name_lower: &str,
    sport: Option<&str>,
) -> bool {
    let soccer = sport.unwrap_or("").eq_ignore_ascii_case("Soccer");
    if is_pool_title(title_lower) {
        if soccer {
            return true;
        }
        if SOCCER_AMBIGUOUS_LEAGUES.iter().any(|s| league_name_lower.contains(s)) {
            return true;
        }
    }
    if is_tennis_grand_slam_title(title_lower) && soccer {
        return true;
    }
    if afc_wimbledon_football().is_match(title_lower)
        && league_name_lower.contains("wimbledon")
        && soccer
    {
        return true;
    }
    if is_motorcycle_motorsport_title(title_lower) && is_drag_racing_league(league_name_lower) {
        return true;
    }
    if title_lower.contains("evian") && is_drag_racing_league(league_name_lower) {
        return true;
    }
    if title_lower.contains("evian")
        && league_name_lower.contains("pga tour")
        && !league_name_lower.contains("lpga")
    {
        return true;
    }
    // The general rule, and the reason the four above should stop growing.
    //
    // Everything before this line is a list of PAST incidents, one entry per bug found. The
    // index match itself is a bare substring test, so a short name buried inside a word
    // qualifies as easily as a real one: "FIA Formula ONE" contains "one", and TheSportsDB
    // carries ONE Championship, which is kickboxing. That is how a Formula 1 hero came to
    // wear a kickboxing backdrop.
    //
    // So: when the title says plainly what sport it is, an index entry from a different sport
    // is a conflict whatever its name matched.
    if let Some(implied) = guess_sport_from_title(title_lower) {
        if let Some(indexed) = sport {
            if !indexed.trim().is_empty() && !sports_agree(&implied, indexed) {
                return true;
            }
        }
    }
    false
}

/// Whether a sport guessed from a title and a sport named by TheSportsDB are the same thing.
///
/// Deliberately narrow. Anything not recognised on both sides compares as EQUAL rather than
/// different, so an unfamiliar sport name can never cause a good match to be thrown away —
/// this guard exists to reject a confident mismatch, not to demand a confident match.
pub fn sports_agree(guessed: &str, indexed: &str) -> bool {
    fn canon(v: &str) -> String {
        match v.to_lowercase().trim() {
            "fighting" | "mma" | "boxing" | "kickboxing" | "martial arts" | "wrestling" => {
                "fighting".to_string()
            }
            "motorsport" | "motorsports" | "racing" | "auto racing" | "motorcycle" => {
                "motorsport".to_string()
            }
            "american football" | "americanfootball" => "american football".to_string(),
            "soccer" | "football" => "soccer".to_string(),
            "billiards" | "pool" | "snooker" => "cue".to_string(),
            other => other.to_string(),
        }
    }
    canon(guessed) == canon(indexed)
}

/// Byte-for-byte port of `toV2SearchSlug`: lowercase, replace non-alnum runs
/// with `_`, trim leading/trailing `_`.
pub fn to_v2_search_slug(query: &str) -> String {
    slug_re()
        .replace_all(&query.to_lowercase(), "_")
        .trim_matches('_')
        .to_string()
}

/// Byte-for-byte port of `buildSearchQueries`: linkedSetOf of competition,
/// title, extracted competition parts, and special-case additions for
/// Evian / motorcycle-motorsport titles.
pub fn build_search_queries(competition: Option<&str>, title: &str) -> Vec<String> {
    // linkedSetOf preserves insertion order and deduplicates.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();

    if let Some(c) = competition {
        let ct = c.trim();
        if ct.chars().count() >= 3 && seen.insert(ct.to_string()) {
            out.push(ct.to_string());
        }
    }
    let tt = title.trim();
    if tt.chars().count() >= 3 && seen.insert(tt.to_string()) {
        out.push(tt.to_string());
    }
    for part in extract_competition_from_title(title) {
        if seen.insert(part.clone()) {
            out.push(part);
        }
    }
    let tl = title.to_lowercase();
    if tl.contains("evian") {
        for q in [
            "Amundi Evian Championship",
            "LPGA Evian Championship",
            "The Evian Championship",
        ] {
            if seen.insert(q.to_string()) {
                out.push(q.to_string());
            }
        }
    }
    if tl.contains("premier league") {
        for q in ["Premier League", "English Premier League"] {
            if seen.insert(q.to_string()) {
                out.push(q.to_string());
            }
        }
    }
    if tl.contains("carabao") || tl.contains("efl cup") || tl.contains("league cup") {
        for q in ["EFL Cup", "Carabao Cup", "English League Cup"] {
            if seen.insert(q.to_string()) {
                out.push(q.to_string());
            }
        }
    }
    if tl.contains("indycar") {
        for q in ["NTT IndyCar Series", "IndyCar Series"] {
            if seen.insert(q.to_string()) {
                out.push(q.to_string());
            }
        }
    }
    if is_motorcycle_motorsport_title(&tl) {
        for q in ["FIM Speedway Grand Prix", "FIM MotoGP", "World Superbike"] {
            if seen.insert(q.to_string()) {
                out.push(q.to_string());
            }
        }
    }
    out
}

/// Byte-for-byte port of `scoreLeagueQueryMatch`.
pub fn score_league_query_match(query: &str, league_name: &str) -> i32 {
    let q_lower = query.to_lowercase();
    let name_lower = league_name.to_lowercase();
    let q = q_lower.trim();
    let name = name_lower.trim();
    if q.is_empty() || name.is_empty() {
        return 0;
    }
    // Kotlin's `String.length` is the UTF-16 code-unit count; for ASCII / BMP
    // league names `chars().count()` matches.
    let name_len = name.chars().count() as i32;
    if name == q {
        return 10_000;
    }
    if name.starts_with(q) || q.starts_with(name) {
        return 5_000 + name_len;
    }
    if name.contains(q) {
        return 2_000 + name_len;
    }
    if q.contains(name) {
        return 1_500 + name_len;
    }
    let count = tokenize(q)
        .iter()
        .filter(|tok| name.contains(tok.as_str()))
        .count() as i32;
    count * 100 + name_len
}

/// Byte-for-byte port of `tokenize`: lowercase, split on `[^a-z0-9]+`,
/// filter length >= 3.
pub fn tokenize(text: &str) -> Vec<String> {
    tokenize_re()
        .split(&text.to_lowercase())
        .filter(|s| s.chars().count() >= 3)
        .map(|s| s.to_string())
        .collect()
}

/// Byte-for-byte port of `isMotorcycleMotorsportTitle`. Input is already
/// lowercased by the caller.
pub fn is_motorcycle_motorsport_title(t: &str) -> bool {
    t.contains("fim")
        || t.contains("motogp")
        || t.contains("moto gp")
        || t.contains("motocross")
        || t.contains("mxgp")
        || t.contains("superbike")
        || t.contains("worldsbk")
        || t.contains("speedway")
}

/// Byte-for-byte port of `isDragRacingLeague`. Input is already lowercased.
pub fn is_drag_racing_league(name: &str) -> bool {
    name.contains("drag")
        || name.contains("nhra")
        || name.contains("top fuel")
        || name.contains("funny car")
}

/// Byte-for-byte port of `isPoolTitle`. Input is already lowercased.
pub fn is_pool_title(t: &str) -> bool {
    POOL_TITLE_CUES.iter().any(|cue| t.contains(cue))
}

/// Byte-for-byte port of `isTennisGrandSlamTitle`. Input is already lowercased.
///
/// Logic: (wimbledon AND NOT afc_wimbledon_football) OR roland garros OR
/// french open OR (us open AND NOT golf AND NOT swim).
pub fn is_tennis_grand_slam_title(t: &str) -> bool {
    (wimbledon_word().is_match(t) && !afc_wimbledon_football().is_match(t))
        || t.contains("roland garros")
        || t.contains("french open")
        || (t.contains("us open") && !t.contains("golf") && !t.contains("swim"))
}

/// Byte-for-byte port of `extractCompetitionFromTitle`.
///
/// Strips broadcast-style prefix noise (`Live:`, year, `The`), returns
/// competition-name candidates derived from the title (before-colon segment,
/// champions-league segment). Special-cases Evian.
pub fn extract_competition_from_title(title: &str) -> Vec<String> {
    let t = title.trim();
    if t.is_empty() {
        return Vec::new();
    }
    let stripped = title_prefix_re().replace(t, "");
    let stripped = stripped.trim();
    let sl = stripped.to_lowercase();
    if sl.contains("evian") {
        return vec![
            "Amundi Evian Championship".to_string(),
            "LPGA Evian Championship".to_string(),
        ];
    }
    if sl.contains("carabao") || sl.contains("efl cup") {
        return vec![
            "EFL Cup".to_string(),
            "Carabao Cup".to_string(),
            "English League Cup".to_string(),
        ];
    }

    // `t.substringBefore(':')` — the part before the first colon, or the
    // whole string if no colon.
    let before_colon = match t.find(':') {
        Some(idx) => t[..idx].trim(),
        None => t.trim(),
    };

    let mut out: Vec<String> = Vec::new();
    let before_colon_lower = before_colon.to_lowercase();
    if before_colon.chars().count() >= 3
        && before_colon != t
        && !TITLE_PREFIX_NOISE_WORDS.contains(&before_colon_lower.as_str())
    {
        out.push(before_colon.to_string());
    }

    if let Some(m) = champions_league_re().find(t) {
        let idx = m.start();
        // Kotlin: `t.lastIndexOfAny(listOf("-", "–", "|", ","), idx)` —
        // the last separator char at or before `idx`, or -1.
        let start_byte = t[..idx]
            .char_indices()
            .filter(|(_, c)| matches!(*c, '-' | '–' | '|' | ','))
            .last()
            .map(|(bi, c)| bi + c.len_utf8())
            .unwrap_or(0);
        out.push(t[start_byte..].trim().to_string());
    }

    out.into_iter().filter(|s| s.chars().count() >= 3).collect()
}

// ---------------------------------------------------------------------------
// JNI boundary
// ---------------------------------------------------------------------------

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

fn string_to_jstring(env: &mut Env, s: &str) -> jstring {
    env.new_string(s).unwrap_or_default().into_raw()
}

fn json_array_to_jstring(env: &mut Env, items: &[String]) -> jstring {
    let json = serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string());
    string_to_jstring(env, &json)
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeGuessSportFromTitle<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    title: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let t = jstring_to_string(env, &title);
        let result = guess_sport_from_title(&t).unwrap_or_default();
        string_to_jstring(env, &result)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeTitleImpliesSpecificEvent<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    title: JString<'local>,
    competition: JString<'local>,
    sport_type: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let t = jstring_to_string(env, &title);
        let c = jstring_to_string(env, &competition);
        let st = jstring_to_string(env, &sport_type);
        let result = title_implies_specific_event(&t, Some(&c), Some(&st));
        result
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeIndexMatchConflicts<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    title_lower: JString<'local>,
    league_name_lower: JString<'local>,
    sport: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let tl = jstring_to_string(env, &title_lower);
        let lnl = jstring_to_string(env, &league_name_lower);
        let sp = jstring_to_string(env, &sport);
        let result = index_match_conflicts(&tl, &lnl, Some(&sp));
        result
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeToV2SearchSlug<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    query: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let q = jstring_to_string(env, &query);
        let result = to_v2_search_slug(&q);
        string_to_jstring(env, &result)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeBuildSearchQueries<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    competition: JString<'local>,
    title: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let c = jstring_to_string(env, &competition);
        let t = jstring_to_string(env, &title);
        let queries = build_search_queries(Some(&c), &t);
        json_array_to_jstring(env, &queries)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeScoreLeagueQueryMatch<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    query: JString<'local>,
    league_name: JString<'local>,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
        let q = jstring_to_string(env, &query);
        let ln = jstring_to_string(env, &league_name);
        score_league_query_match(&q, &ln)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeTokenize<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    text: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let t = jstring_to_string(env, &text);
        let tokens = tokenize(&t);
        json_array_to_jstring(env, &tokens)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeIsMotorcycleMotorsportTitle<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    t: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let t = jstring_to_string(env, &t);
        let result = is_motorcycle_motorsport_title(&t);
        result
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeIsDragRacingLeague<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    name: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let n = jstring_to_string(env, &name);
        let result = is_drag_racing_league(&n);
        result
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeIsPoolTitle<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    t: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let t = jstring_to_string(env, &t);
        let result = is_pool_title(&t);
        result
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeIsTennisGrandSlamTitle<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    t: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let t = jstring_to_string(env, &t);
        let result = is_tennis_grand_slam_title(&t);
        result
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeExtractCompetitionFromTitle<
    'local,
>(
    mut env: EnvUnowned,
    _class: JClass<'local>,
    title: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let t = jstring_to_string(env, &title);
        let parts = extract_competition_from_title(&t);
        json_array_to_jstring(env, &parts)
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety: Called only by the JVM via JNI.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_remote_thesportsdb_SportsLeagueArtNativeBridge_nativeEngineVersion<
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

    // ---- guess_sport_from_title ----

    #[test]
    fn detects_rugby_before_football() {
        // "rugby" preempts the soccer branch.
        assert_eq!(
            guess_sport_from_title("Rugby World Cup"),
            Some("Rugby".into())
        );
    }

    #[test]
    fn detects_cricket() {
        assert_eq!(
            guess_sport_from_title("Live Cricket: England vs Australia"),
            Some("Cricket".into())
        );
    }

    #[test]
    fn detects_evian_as_golf_before_generic_golf() {
        assert_eq!(
            guess_sport_from_title("Amundi Evian Championship"),
            Some("Golf".into())
        );
    }

    #[test]
    fn detects_lpga_golf() {
        assert_eq!(
            guess_sport_from_title("LPGA Golf Tournament"),
            Some("Golf".into())
        );
    }

    #[test]
    fn detects_tennis_grand_slam_wimbledon() {
        assert_eq!(
            guess_sport_from_title("Wimbledon Quarter Finals"),
            Some("Tennis".into())
        );
    }

    #[test]
    fn detects_tennis_grand_slam_us_open() {
        assert_eq!(
            guess_sport_from_title("US Open Tennis"),
            Some("Tennis".into())
        );
    }

    #[test]
    fn us_open_with_golf_is_not_tennis() {
        // "us open" with "golf" → not tennis grand slam, falls through to golf branch.
        assert_eq!(
            guess_sport_from_title("US Open Golf"),
            Some("Golf".into())
        );
    }

    #[test]
    fn afc_wimbledon_is_not_tennis() {
        // "AFC Wimbledon" is football, not tennis.
        assert_eq!(
            guess_sport_from_title("AFC Wimbledon vs Plymouth"),
            Some("Soccer".into())
        );
    }

    #[test]
    fn detects_motorsport_f1() {
        assert_eq!(
            guess_sport_from_title("F1 Grand Prix Monaco"),
            Some("Motorsport".into())
        );
    }

    #[test]
    fn detects_motorsport_motogp() {
        assert_eq!(
            guess_sport_from_title("MotoGP Race"),
            Some("Motorsport".into())
        );
    }

    #[test]
    fn detects_fighting_ufc() {
        assert_eq!(
            guess_sport_from_title("UFC 300 Title Fight"),
            Some("Fighting".into())
        );
    }

    #[test]
    fn detects_basketball_nba() {
        assert_eq!(
            guess_sport_from_title("NBA Lakers vs Celtics"),
            Some("Basketball".into())
        );
    }

    #[test]
    fn detects_american_football_nfl() {
        assert_eq!(
            guess_sport_from_title("NFL Super Bowl LVIII"),
            Some("American Football".into())
        );
    }

    #[test]
    fn detects_soccer_premier_league() {
        assert_eq!(
            guess_sport_from_title("Premier League: Arsenal vs Chelsea"),
            Some("Soccer".into())
        );
    }

    #[test]
    fn detects_soccer_vs_format() {
        assert_eq!(
            guess_sport_from_title("Madrid vs Barcelona"),
            Some("Soccer".into())
        );
    }

    #[test]
    fn detects_cycling_tour_de_france() {
        assert_eq!(
            guess_sport_from_title("Tour de France Stage 15"),
            Some("Cycling".into())
        );
    }

    #[test]
    fn detects_cycling_uci_word() {
        assert_eq!(
            guess_sport_from_title("UCI World Championships"),
            Some("Cycling".into())
        );
    }

    #[test]
    fn detects_snooker() {
        assert_eq!(
            guess_sport_from_title("World Snooker Championship"),
            Some("Snooker".into())
        );
    }

    #[test]
    fn detects_darts() {
        assert_eq!(
            guess_sport_from_title("PDC World Darts Championship"),
            Some("Darts".into())
        );
    }

    #[test]
    fn returns_none_for_unrecognized_title() {
        assert_eq!(guess_sport_from_title("Cooking Show"), None);
    }

    // ---- title_implies_specific_event ----

    #[test]
    fn specific_event_with_competition() {
        assert!(title_implies_specific_event(
            "Finals",
            Some("World Cup"),
            Some("Soccer")
        ));
    }

    #[test]
    fn specific_event_competition_equals_sport_type_is_false() {
        assert!(!title_implies_specific_event(
            "Soccer",
            Some("Soccer"),
            Some("Soccer")
        ));
    }

    #[test]
    fn specific_event_wimbledon() {
        assert!(title_implies_specific_event(
            "Wimbledon 2026",
            None,
            None
        ));
    }

    #[test]
    fn specific_event_blank_title() {
        assert!(!title_implies_specific_event("   ", None, None));
    }

    // ---- index_match_conflicts ----

    #[test]
    fn pool_title_vs_soccer_league_conflicts() {
        assert!(index_match_conflicts(
            "ultimate pool championship",
            "uefa pool league",
            Some("Soccer")
        ));
    }

    #[test]
    fn tennis_grand_slam_vs_soccer_conflicts() {
        assert!(index_match_conflicts(
            "wimbledon finals",
            "some soccer league",
            Some("Soccer")
        ));
    }

    #[test]
    fn afc_wimbledon_vs_wimbledon_soccer_conflicts() {
        assert!(index_match_conflicts(
            "afc wimbledon match",
            "wimbledon fc",
            Some("Soccer")
        ));
    }

    #[test]
    fn motorcycle_vs_drag_racing_conflicts() {
        assert!(index_match_conflicts(
            "motogp race",
            "nhra drag racing",
            Some("Motorsport")
        ));
    }

    #[test]
    fn no_conflict_for_normal_match() {
        assert!(!index_match_conflicts(
            "premier league",
            "premier league",
            Some("Soccer")
        ));
    }

    // ---- to_v2_search_slug ----

    #[test]
    fn slug_replaces_non_alnum_with_underscore() {
        assert_eq!(to_v2_search_slug("Premier League 2024"), "premier_league_2024");
    }

    #[test]
    fn slug_trims_underscore_edges() {
        assert_eq!(to_v2_search_slug("  Hello World!  "), "hello_world");
    }

    #[test]
    fn slug_empty_for_all_symbols() {
        assert_eq!(to_v2_search_slug("---+++"), "");
    }

    // ---- build_search_queries ----

    #[test]
    fn build_queries_includes_competition_and_title() {
        let queries = build_search_queries(Some("Champions League"), "Real Madrid vs Liverpool");
        assert!(queries.contains(&"Champions League".to_string()));
        assert!(queries.contains(&"Real Madrid vs Liverpool".to_string()));
    }

    #[test]
    fn build_queries_adds_evian_special_cases() {
        let queries = build_search_queries(None, "Evian Championship Golf");
        assert!(queries.contains(&"Amundi Evian Championship".to_string()));
        assert!(queries.contains(&"LPGA Evian Championship".to_string()));
        assert!(queries.contains(&"The Evian Championship".to_string()));
    }

    #[test]
    fn build_queries_adds_motorcycle_special_cases() {
        let queries = build_search_queries(None, "MotoGP Race");
        assert!(queries.contains(&"FIM Speedway Grand Prix".to_string()));
        assert!(queries.contains(&"FIM MotoGP".to_string()));
        assert!(queries.contains(&"World Superbike".to_string()));
    }

    #[test]
    fn build_queries_deduplicates() {
        let queries = build_search_queries(Some("NBA"), "NBA");
        // "NBA" appears as both competition and title but should only be once.
        let nba_count = queries.iter().filter(|q| *q == "NBA").count();
        assert_eq!(nba_count, 1);
    }

    // ---- score_league_query_match ----

    #[test]
    fn score_exact_match() {
        assert_eq!(score_league_query_match("Premier League", "Premier League"), 10_000);
    }

    #[test]
    fn score_prefix_match() {
        let score = score_league_query_match("Premier", "Premier League");
        assert!(score >= 5_000 && score < 10_000);
    }

    #[test]
    fn score_substring_in_name() {
        let score = score_league_query_match("League", "Premier League");
        assert!(score >= 2_000 && score < 5_000);
    }

    #[test]
    fn score_empty_inputs() {
        assert_eq!(score_league_query_match("", "Premier League"), 0);
        assert_eq!(score_league_query_match("Premier", ""), 0);
    }

    #[test]
    fn score_token_overlap() {
        let score = score_league_query_match("World Cup Football", "World Football Association");
        assert!(score > 0);
    }

    // ---- tokenize ----

    #[test]
    fn tokenize_splits_on_non_alnum() {
        assert_eq!(
            tokenize("Hello World 2024"),
            vec!["hello", "world", "2024"]
        );
    }

    #[test]
    fn tokenize_filters_short_tokens() {
        assert_eq!(
            tokenize("A B CD EFG"),
            vec!["efg"]
        );
    }

    #[test]
    fn tokenize_empty_for_all_punctuation() {
        assert!(tokenize("!!! ... ???").is_empty());
    }

    // ---- is_motorcycle_motorsport_title ----

    #[test]
    fn motorcycle_detects_fim() {
        assert!(is_motorcycle_motorsport_title("fim speedway"));
    }

    #[test]
    fn motorcycle_detects_superbike() {
        assert!(is_motorcycle_motorsport_title("world superbike championship"));
    }

    #[test]
    fn motorcycle_rejects_f1() {
        assert!(!is_motorcycle_motorsport_title("f1 grand prix"));
    }

    // ---- is_drag_racing_league ----

    #[test]
    fn drag_racing_detects_nhra() {
        assert!(is_drag_racing_league("nhra top fuel"));
    }

    #[test]
    fn drag_racing_rejects_motogp() {
        assert!(!is_drag_racing_league("motogp championship"));
    }

    // ---- is_pool_title ----

    #[test]
    fn pool_detects_ultimate_pool() {
        assert!(is_pool_title("ultimate pool championship"));
    }

    #[test]
    fn pool_detects_snooker() {
        assert!(is_pool_title("world snooker tour"));
    }

    #[test]
    fn pool_rejects_football() {
        assert!(!is_pool_title("premier league football"));
    }

    // ---- is_tennis_grand_slam_title ----

    #[test]
    fn tennis_grand_slam_wimbledon() {
        assert!(is_tennis_grand_slam_title("wimbledon 2026"));
    }

    #[test]
    fn tennis_grand_slam_roland_garros() {
        assert!(is_tennis_grand_slam_title("roland garros final"));
    }

    #[test]
    fn tennis_grand_slam_french_open() {
        assert!(is_tennis_grand_slam_title("french open tennis"));
    }

    #[test]
    fn tennis_grand_slam_us_open_with_swim_is_false() {
        assert!(!is_tennis_grand_slam_title("us open swimming"));
    }

    #[test]
    fn tennis_grand_slam_afc_wimbledon_is_false() {
        assert!(!is_tennis_grand_slam_title("afc wimbledon football"));
    }

    // ---- extract_competition_from_title ----

    #[test]
    fn extract_strips_live_prefix() {
        let parts = extract_competition_from_title("Live: Champions League Final");
        assert!(parts.iter().any(|p| p.contains("Champions League")));
    }

    #[test]
    fn extract_strips_year_and_the_prefix() {
        let parts = extract_competition_from_title("2024 The Amundi Evian Championship");
        // Evian special case returns the two known names.
        assert!(parts.contains(&"Amundi Evian Championship".to_string()));
        assert!(parts.contains(&"LPGA Evian Championship".to_string()));
    }

    #[test]
    fn extract_before_colon_segment() {
        let parts = extract_competition_from_title("NBA: Lakers vs Celtics");
        assert!(parts.contains(&"NBA".to_string()));
    }

    #[test]
    fn extract_filters_noise_words() {
        let parts = extract_competition_from_title("Live: Match Highlights");
        // "Live" is a noise word, should not appear as a part.
        assert!(!parts.iter().any(|p| p.eq_ignore_ascii_case("live")));
    }

    #[test]
    fn extract_empty_for_blank_title() {
        assert!(extract_competition_from_title("   ").is_empty());
    }

    #[test]
    fn extract_champions_league_segment() {
        let parts = extract_competition_from_title("UEFA - Champions League Final");
        // The champions-league segment should start after the last separator before "champions league".
        assert!(parts.iter().any(|p| p.to_lowercase().contains("champions league")));
    }

    // ---- engine version ----

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}

#[cfg(test)]
mod formula_one_kickboxing_tests {
    use super::*;

    /// "FIA Formula One" contains "one", and TheSportsDB carries ONE Championship —
    /// kickboxing. The index match is a bare substring test, so the short name inside a word
    /// qualified as readily as a real league, and a Formula 1 hero wore a kickboxing backdrop.
    #[test]
    fn formula_one_is_motorsport() {
        assert_eq!(
            guess_sport_from_title("FIA Formula One World Championship"),
            Some("Motorsport".to_string()),
        );
        assert_eq!(
            guess_sport_from_title("Formula E: Berlin ePrix"),
            Some("Motorsport".to_string()),
        );
        // The spelling that already worked must keep working.
        assert_eq!(
            guess_sport_from_title("Formula 1: Italian Grand Prix"),
            Some("Motorsport".to_string()),
        );
    }

    #[test]
    fn a_fighting_league_cannot_claim_a_motorsport_title() {
        assert!(index_match_conflicts(
            "fia formula one world championship",
            "one championship",
            Some("Fighting"),
        ));
    }

    #[test]
    fn the_right_league_is_still_allowed() {
        assert!(!index_match_conflicts(
            "fia formula one world championship",
            "formula one",
            Some("Motorsport"),
        ));
        assert!(!index_match_conflicts("ufc 300 main card", "ufc", Some("MMA")));
    }

    /// The guard rejects a confident mismatch; it must never demand a confident match.
    #[test]
    fn an_unknown_sport_never_throws_away_a_match() {
        assert!(!index_match_conflicts("the saturday show", "one championship", Some("Fighting")));
        assert!(!index_match_conflicts("fia formula one", "one championship", None));
        assert!(!index_match_conflicts("fia formula one", "one championship", Some("")));
    }

    #[test]
    fn sports_agree_is_narrow_and_symmetric() {
        assert!(sports_agree("Motorsport", "Racing"));
        assert!(sports_agree("Fighting", "Kickboxing"));
        assert!(sports_agree("Soccer", "Football"));
        assert!(!sports_agree("Motorsport", "Fighting"));
        assert!(sports_agree("Kabaddi", "Kabaddi"));
        assert!(!sports_agree("Kabaddi", "Curling"));
    }
}
