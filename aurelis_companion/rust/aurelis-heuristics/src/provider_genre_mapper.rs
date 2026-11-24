//! Port of `ProviderGenreMapper` — maps provider-specific genre strings to
//! `ContentCategory` per country.
//!
//! All regex patterns, helper functions, country mappers, and the `categorize`
//! orchestration are ported from the Kotlin `object ProviderGenreMapper`.
//!
//! Key deviation from Kotlin: `looks_like_regional_name` and `categorize`
//! accept a `detected_region: Option<&str>` parameter instead of calling
//! `RegionalChannelFilter.detectRegionInName` directly (that class lives in
//! another crate). The Kotlin bridge calls `detectRegionInName` and passes
//! the result.
//!
//! Rust's `regex` crate doesn't support lookahead. The Kotlin patterns that
//! use negative lookahead (`localNameRegex`'s ITV guard, `documentaryNameRegex`'s
//! Quest guard, `entertainmentNameRegex`'s Legend guard, `caCtvLocalRegex`'s
//! sub-channel guard) are split: the regex matches without the lookahead, then
//! a helper function checks the lookahead condition in code.

use super::*;
use regex::Regex;
use std::sync::LazyLock;

// ── Static regexes ───────────────────────────────────────────────────────────

static ENT_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bent\b").unwrap());

static REG_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\breg\b").unwrap());

static ITV_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bitv\b").unwrap());

static CHANNEL_45_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bchannel\s*[45]\b").unwrap());

static RADIO_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(^|[^a-z])(radio|fm|dab\+?|talksport|talk radio|absolute radio|bbc sounds|planet rock|kerrang radio|virgin radio|capital fm|heart fm|kiss fm|smooth radio|classic fm|magic radio|jazz fm|radio caroline|greatest hits radio|bbc\s*(?:6\s*music|r1x|r5l|r5sx|asian\s*net(?:work)?|world\s*s(?:v|ervice)\.?)|6\s*music|capital|heart|lbc(?:\s*news)?|rnib\s*connect)([^a-z]|$)",
    )
    .unwrap()
});

// localNameRegex without the ITV negative lookahead — the lookahead is
// checked in code by `local_name_regex_is_match`.
static LOCAL_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(bbc\s*(?:one|1|two|2|scotland|alba|cymru|wales|ni)|itv\s*1|itv|stv|utv|s4c|that'?s\s*tv|thats\s*tv|channel\s*4|channel\s*5)\b",
    )
    .unwrap()
});

// Checks the Kotlin lookahead `(?!\s*[2-4]|\s*be\b|be\b|x\b)` on the text
// immediately after a bare "itv" match.  Each alternative is independent
// (the `\s*` is NOT shared across alternatives — only `\s*[2-4]` and
// `\s*be\b` consume leading whitespace; `be\b` and `x\b` do not).
static ITV_LOOKAHEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:\s*[2-4]|\s*be\b|be\b|x\b)").unwrap()
});

static NEWS_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(bbc\s*news|itv\s*news|channel\s*4\s*news|channel\s*5\s*news|sky\s*news|gb\s*news|cbc\s*news|ctv\s*news|(?:bnn\s*)?bloomberg|cp\s*24|cpac)\b",
    )
    .unwrap()
});

// documentaryNameRegex without the Quest negative lookahead — checked in
// code by `documentary_name_regex_is_match`. TLC is excluded for the same reason as
// Quest Red: Discovery-family name, reality/lifestyle content — and this rescue
// overrides an ENTERTAINMENT genre, so listing it here put TLC in Documentaries no
// matter what the provider's group-title said.
static DOCUMENTARY_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(sky\s*documentaries|sky\s*docs|sky\s*docmntrs|sky\s*nature|sky\s*history|sky\s*crime|discovery|national\s*geographic|nat\s*geo|animal\s*planet|crime\s*(?:(?:and|&|\+)\s*)?inv(?:estigation)?|investigation\s*discovery|u(?:and|&)?\s*yesterday|yesterday|u(?:and|&)?\s*eden|eden|pbs\s*america|blaze|court\s*tv|travel\s*xp|bbc\s*earth|smithsonian|history|discovery\s*turbo|turbo|id|american\s*heroes|dmax|true\s*crime)\b",
    )
    .unwrap()
});

static TLC_LOOKAHEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*tlc\b").unwrap());

static MOVIES_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(amc|crave\s*[0-9]?|sky\s*cinema|disney\+?\s*cinema|disney\s*plus\s*cinema|film\s*4|film4|movies\s*24|legend\s*xtra|horror\s*xtra|horror\s*channel|talking\s*pictures|great[!\s]*(?:movies|action|mystery|romance|christmas)|sony\s*movies|that'?s\s*movies|tcm|turner\s*classic\s*movies)\b",
    )
    .unwrap()
});

// entertainmentNameRegex without the Legend negative lookahead — checked in
// code by `entertainment_name_regex_is_match`.
static ENTERTAINMENT_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(dave(?:\s*ja\s*vu)?|itv\s*[2-4]|itv\s*be|itvbe|itv\s*x|itvx|together(?:\s*tv)?|e4(?:\s*extra)?|more\s*4|more4|4\s*seven|4seven|5\s*star|5star|5\s*usa|5usa|5\s*action|5action|5\s*select|5select|5\s*drama|5drama|legend|great[!\s]*tv|quest|quest\s*red|tlc|food\s*network|bravo|ctv\s*comedy|ctv\s*drama|hgtv|lifetime|peachtree|slice|fx\s*canada|fxx\s*canada|e!\s*canada)\b",
    )
    .unwrap()
});
static UK_SKY_COMEDY_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bsky\s*comedy\b").unwrap());
static UK_ITV_ENTERTAINMENT_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bitv\s*[2-4]\b").unwrap());

/// Kotlin parity: `ProviderGenreMapper.isLegendItself`. Legend however a provider dresses it --
/// "Legend", "[UK] LEGEND", "316 UK - LEGEND HD" -- and not Legend Xtra.
fn is_legend_itself(name: &str) -> bool {
    let lower = name.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| {
            !w.is_empty()
                && !w.chars().all(|c| c.is_ascii_digit())
                && !matches!(*w, "uk" | "gb" | "hd" | "fhd" | "sd" | "uhd" | "4k")
        })
        .collect();
    words == ["legend"]
}

static LEGEND_LOOKAHEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*xtra").unwrap());

static CA_LOCAL_BRAND_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(cbc|global|city\s*tv)\b").unwrap());

// caCtvLocalRegex without the negative lookahead — checked in code by
// `ca_ctv_local_regex_is_match`.
static CA_CTV_LOCAL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bctv\s*2?\b").unwrap());

static CTV_LOOKAHEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:comedy|drama|news|sci-?fi)").unwrap()
});

static KIDS_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(cbbc|cbeebies|cartoon\s*network|cartoon\s*net|cartoonito|boomerang|nickelodeon|nick\s*jr|nick\s*junior|nicktoons|teen\s*nick|sky\s*kids|disney\s*jr|disney\s*junior|disney\s*channel|babytv|baby\s*tv|ducktv|rte\s*kidsjr|rte\s*kids|rtejr|rte\s*junior)\b",
    )
    .unwrap()
});

static SHOPPING_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(qvc|tjc|the\s*jewellery\s*channel|must\s*have\s*ideas|gemporia|gems\s*tv|high\s*street\s*tv|hstv|ideal\s*world|best\s*direct|hobby\s*maker|hobbymaker|shop\s*unlimited|tv\s*warehouse|jewellery\s*maker|jewelry\s*maker|cruise\s*1st|cruise1st|hochanda|create\s*and\s*craft|craft\s*extra|jml\s*direct)\b",
    )
    .unwrap()
});

static INTERNATIONAL_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(atn)\b").unwrap());

static RELIGIOUS_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(god\s*(?:tv|channel)|tbn|daystar|revelation(?:\s*tv)?|ewtn|loveworld|faith\s*world|dunamis|son\s*life|sonlife|sbn|islam\s*channel|sikh\s*channel|sangat\s*tv|mta\s*international)\b",
    )
    .unwrap()
});

static SPORT_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(sport|sports|bein|espn|eurosport|premier\s*sports|dazn|mutv|lfctv|laliga|clubber\s*tv|clubber|fifa\s*world\s*cup|tsn[0-9]?|sportsnet|fubo\s*sports|racing[\s._\-]*tv\+?|racing[\s._\-]*uk|racingtv|racinguk|at[\s._\-]*the[\s._\-]*races)\b",
    )
    .unwrap()
});

/// `rte[0-9]?` rather than a bare `rte`: the channel is called `RTÉ2` as often as `RTÉ 2`,
/// and with the digit glued on there is no word boundary for `\brte\b` to find. The
/// run-together junior spellings need naming for the same reason. See `irish_psb_category`.
static IRISH_BRAND_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(rte[0-9]?|rte\s?(?:jr|junior|kids)|tg4|virgin)\b").unwrap()
});

static RTE_JUNIOR_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\brte\s*(?:jr|junior|kids)\b").unwrap());

static RTE_RADIO_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(rte\s*(?:radio|2fm|lyric|gold)|virgin\s*radio)\b").unwrap()
});

// ── Static data ──────────────────────────────────────────────────────────────

fn music_name_tokens_by_country(cc: &str) -> Option<&'static [&'static str]> {
    match cc {
        "GB" => Some(&[
            "4music", "boxhits", "boxplus", "boxnation", "kisstv", "mtvmusic", "mtvhits",
            "mtvbase", "mtvclassic", "vevo", "now70s", "now80s", "now90s", "now00s",
            "kerrangtv", "magictv", "traceurban", "tracevault", "clubbertv",
        ]),
        "US" => Some(&[
            "mtv", "mtv2", "vh1", "cmt", "fuse", "revolt", "musicchoice",
            "greatamericancountry", "gactv", "juice",
        ]),
        "DE" => Some(&["mtv", "viva", "deluxemusic", "rockantennetv", "goldstartv"]),
        "FR" => Some(&["mcm", "mtv", "trace", "mezzo", "m6music"]),
        "IT" => Some(&["mtvitalia", "radioitaliatv", "deejaytv"]),
        "ES" => Some(&["mtvespana", "solmusica", "kisstv"]),
        "PT" => Some(&["mtvportugal"]),
        "NL" => Some(&["mtvnl", "tmf", "538tv"]),
        "CA" => Some(&["muchmusic", "juicebox", "cmt"]),
        "HK" => Some(&["channelv", "mtvmandarin", "mtvasia"]),
        _ => None,
    }
}

fn regional_name_tokens_by_country(cc: &str) -> Option<&'static [&'static str]> {
    match cc {
        "DE" => Some(&["ndr", "wdr", "br", "swr", "mdr", "hr", "rbb", "sr"]),
        "FR" => Some(&[
            "france3bretagne",
            "france3normandie",
            "france3alsace",
            "france3aquitaine",
            "france3provence",
            "france3paca",
            "france3occitanie",
            "france3grandest",
        ]),
        _ => None,
    }
}

// ── Lookahead-aware regex helpers ────────────────────────────────────────────

/// `localNameRegex` with the ITV negative lookahead handled in code.
/// Matches the regex, but when the matched text is a bare "itv" (not "itv1"),
/// checks that the text after the match doesn't start with `[2-4]`, `be`, or `x`.
fn local_name_regex_is_match(text: &str) -> bool {
    LOCAL_NAME_REGEX.find_iter(text).any(|m| {
        let matched = m.as_str().to_lowercase();
        if matched == "itv" {
            let after = &text[m.end()..];
            !ITV_LOOKAHEAD.is_match(after)
        } else {
            true
        }
    })
}

/// `documentaryNameRegex` with the TLC negative lookahead handled in code.
/// When the matched text is "discovery", checks that the text after doesn't start
/// with "tlc".
fn documentary_name_regex_is_match(text: &str) -> bool {
    DOCUMENTARY_NAME_REGEX.find_iter(text).any(|m| {
        let matched = m.as_str().to_lowercase();
        if matched == "discovery" {
            let after = &text[m.end()..];
            !TLC_LOOKAHEAD.is_match(after)
        } else {
            true
        }
    })
}

/// `entertainmentNameRegex` with the Legend negative lookahead handled in code.
/// When the matched text is "legend", checks that the text after doesn't start
/// with "xtra".
fn entertainment_name_regex_is_match(text: &str) -> bool {
    ENTERTAINMENT_NAME_REGEX.find_iter(text).any(|m| {
        let matched = m.as_str().to_lowercase();
        if matched == "legend" {
            let after = &text[m.end()..];
            !LEGEND_LOOKAHEAD.is_match(after)
        } else {
            true
        }
    })
}

/// `caCtvLocalRegex` with the negative lookahead handled in code.
/// Every CTV match is checked: if the text after starts with one of the
/// excluded sub-channel names (comedy/drama/news/sci-fi), the match is skipped.
fn ca_ctv_local_regex_is_match(text: &str) -> bool {
    CA_CTV_LOCAL_REGEX.find_iter(text).any(|m| {
        let after = &text[m.end()..];
        !CTV_LOOKAHEAD.is_match(after)
    })
}

// ── Helper ───────────────────────────────────────────────────────────────────

/// True when `by_genre` is `None` or equals one of `cats`.
#[inline]
fn matches_or_none(by_genre: Option<ContentCategory>, cats: &[ContentCategory]) -> bool {
    by_genre.is_none() || cats.iter().any(|c| by_genre == Some(*c))
}

// ── looksLike* functions ─────────────────────────────────────────────────────

fn looks_like_radio_name(name: &str) -> bool {
    !name.trim().is_empty() && RADIO_NAME_REGEX.is_match(name)
}

fn looks_like_news_name(name: &str) -> bool {
    !name.trim().is_empty() && NEWS_NAME_REGEX.is_match(name)
}

fn looks_like_documentary_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let folded = channel_name_keys::fold_decorations(name);
    documentary_name_regex_is_match(&folded)
}

fn looks_like_movies_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let folded = channel_name_keys::fold_decorations(name);
    MOVIES_NAME_REGEX.is_match(&folded)
}

fn looks_like_entertainment_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let folded = channel_name_keys::fold_decorations(name);
    if entertainment_name_regex_is_match(&folded) {
        return true;
    }
    // Channel 4 +1 / Channel 5 +1 — Freeview entertainment timeshifts (not Local PSB).
    if channel_name_keys::is_timeshift(name) && CHANNEL_45_REGEX.is_match(&folded) {
        return true;
    }
    false
}

fn looks_like_local_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let expanded = channel_name_aliases::expand(name);
    // Channel 4/5 +1 are Freeview entertainment timeshifts, not Local PSB feeds —
    // leave them for the entertainment rescue.  BBC/ITV +1 stay Local (flagship
    // timeshifts).
    if channel_name_keys::is_timeshift(name) && CHANNEL_45_REGEX.is_match(&expanded) {
        return false;
    }
    local_name_regex_is_match(&expanded)
}

fn looks_like_canadian_local_name(country_code: Option<&str>, name: &str) -> bool {
    !name.trim().is_empty()
        && country_code.map_or(false, |cc| cc.eq_ignore_ascii_case("CA"))
        && (CA_LOCAL_BRAND_REGEX.is_match(name) || ca_ctv_local_regex_is_match(name))
}

fn looks_like_kids_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    // Fold accents/superscripts first so "RTÉ KIDSjr" matches the ASCII "rte kidsjr"
    // token (same decoration pipeline as plan matching / dedup).
    let n_lower = channel_name_keys::fold_decorations(name).to_lowercase();
    let n = n_lower.trim();
    KIDS_NAME_REGEX.is_match(n)
}

fn looks_like_shopping_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let folded = channel_name_keys::fold_decorations(name);
    SHOPPING_NAME_REGEX.is_match(&folded)
}

fn looks_like_international_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let folded = channel_name_keys::fold_decorations(name);
    INTERNATIONAL_NAME_REGEX.is_match(&folded)
}

fn looks_like_religious_name(name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let folded = channel_name_keys::fold_decorations(name);
    RELIGIOUS_NAME_REGEX.is_match(&folded)
}

fn looks_like_sport_name(name: &str) -> bool {
    !name.trim().is_empty() && SPORT_NAME_REGEX.is_match(name)
}

fn looks_like_music_name(country_code: Option<&str>, name: &str) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let cc_upper = country_code.map(|s| s.to_uppercase());
    let tokens = match cc_upper.as_deref() {
        Some(cc) => music_name_tokens_by_country(cc),
        None => None,
    };
    let tokens = match tokens {
        Some(t) => t,
        None => return false,
    };
    let n = channel_name_keys::normalize_for_match(name);
    tokens
        .iter()
        .any(|&token| n == token || n.starts_with(token) || (token.len() >= 4 && n.contains(token)))
}

/// Port of `looksLikeRegionalName`.
///
/// `detected_region` replaces the Kotlin call to
/// `RegionalChannelFilter.detectRegionInName(name)`.  The Kotlin bridge
/// calls that function and passes the result here.
fn looks_like_regional_name(
    country_code: Option<&str>,
    name: &str,
    user_region: Option<&str>,
    detected_region: Option<&str>,
) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    let cc = country_code.map(|s| s.to_uppercase());
    let cc_ref = cc.as_deref();
    if cc_ref == Some("GB") || cc_ref.is_none() {
        if looks_like_local_name(name) {
            if let Some(detected) = detected_region {
                if !detected.is_empty() {
                    return match user_region {
                        None => true,
                        Some(ur) => !detected.eq_ignore_ascii_case(ur),
                    };
                }
            }
        }
        if cc_ref.is_some() {
            return false;
        }
    }
    let tokens = match cc_ref {
        Some(cc) => regional_name_tokens_by_country(cc),
        None => return false,
    };
    let tokens = match tokens {
        Some(t) => t,
        None => return false,
    };
    let n = channel_name_keys::normalize_for_match(name);
    tokens
        .iter()
        .any(|&token| n == token || n.starts_with(token) || (token.len() >= 4 && n.contains(token)))
}

// ── Irish PSB ────────────────────────────────────────────────────────────────

/// Read off the accent-folded name, because half these channels are spelled with one.
///
/// `\brte\b` cannot match `RTÉ` — the third letter is not an `e` — so one provider's
/// `UK: RTE ONE HD` came out Entertainment and another's `RTÉ One` came out Others, and that
/// disagreement was the only thing keeping two rows with the same key and the same country from
/// merging. TG4 merged perfectly throughout: it is the one Irish brand with no accent in it.
fn irish_psb_category(channel_name: &str) -> Option<ContentCategory> {
    let folded = channel_name_keys::fold_decorations(channel_name);
    if !IRISH_BRAND_REGEX.is_match(&folded) {
        return None;
    }
    if RTE_JUNIOR_REGEX.is_match(&folded) {
        return Some(ContentCategory::Children);
    }
    if RTE_RADIO_REGEX.is_match(&folded) {
        return Some(ContentCategory::Radio);
    }
    if folded.to_lowercase().contains("news") {
        return Some(ContentCategory::News);
    }
    Some(ContentCategory::Entertainment)
}

// ── Country-specific genre mappers ───────────────────────────────────────────

/// "Factual" on its own means documentaries, but providers also ship combined buckets —
/// "US: Lifestyle & Factual" (TLC, HGTV, Food Network), "Reality & Factual" — whose content
/// is the lifestyle half. Because every mapper checks Documentaries before Entertainment,
/// the factual half of a combined label won and dragged the whole group into the
/// Documentaries tab. So: "factual" counts as documentary evidence only when the label
/// doesn't also claim the lifestyle/reality side.
fn is_documentary_factual(g: &str) -> bool {
    g.contains("factual") && !g.contains("lifestyle") && !g.contains("reality")
}

fn map_uk_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    // Adult — explicit, checked first
    if g.contains("adult") || g.contains("xxx") || g.contains("18+") {
        return Some(ContentCategory::Adult);
    }
    // Sport — brands + generic "sport(s)"
    if g.contains("sport")
        || g.contains("espn")
        || g.contains("dazn")
        || g.contains("eurosport")
        || g.contains("racing")
        || g.contains("premier league")
        || g.contains("efl")
        || g.contains("viaplay")
    {
        return Some(ContentCategory::Sport);
    }
    // News — generic
    if g.contains("news") {
        return Some(ContentCategory::News);
    }
    // Children — before Entertainment so "kids" doesn't fall through
    if g.contains("kids")
        || g.contains("child")
        || g.contains("cartoon")
        || g.contains("cbbc")
        || g.contains("cbeebies")
        || g.contains("nick")
        || g.contains("junior")
        || g.contains("boomerang")
    {
        return Some(ContentCategory::Children);
    }
    // Documentaries
    if g.contains("document")
        || g.contains("docu")
        || g.contains("discovery")
        || g.contains("history")
        || g.contains("nat geo")
        || g.contains("national geographic")
        || g.contains("nature")
        || is_documentary_factual(g)
        || g.contains("science")
        || g.contains("wildlife")
    {
        return Some(ContentCategory::Documentaries);
    }
    // Movies — film/cinema variants
    if g.contains("movie") || g.contains("cinema") || g.contains("film") || g.contains("cine") {
        return Some(ContentCategory::Movies);
    }
    // Music
    if g.contains("music") || g.contains("mtv") || g.contains("kerrang") || g.contains("vevo")
        || g.contains("hits")
    {
        return Some(ContentCategory::Music);
    }
    // Radio
    if g.contains("radio") {
        return Some(ContentCategory::Radio);
    }
    // Shopping
    if g.contains("shop") || g.contains("qvc") || g.contains("ideal world") {
        return Some(ContentCategory::Shopping);
    }
    // Religion
    if g.contains("religi")
        || g.contains("christian")
        || g.contains("islam")
        || g.contains("gospel")
        || g.contains("faith")
    {
        return Some(ContentCategory::Religious);
    }
    // Regional
    if g.contains("regional") {
        return Some(ContentCategory::Regional);
    }
    // Terrestrial / Local (UK public-service broadcasters)
    if g.contains("bbc")
        || g.contains("itv")
        || g.contains("channel 4")
        || g.contains("channel4")
        || g.contains("channel 5")
        || g.contains("channel5")
        || g.contains("s4c")
        || g.contains("terrestrial")
        || g.contains("freeview")
        || g.contains("freesat")
        || g.contains("local")
    {
        return Some(ContentCategory::Local);
    }
    // Entertainment — broad bucket + brands, near the end
    if g.contains("entertainment")
        || g.contains("general")
        || g.contains("drama")
        || g.contains("comedy")
        || g.contains("lifestyle")
        || g.contains("reality")
        || g.contains("variety")
        || g.contains("arts")
        || g.contains("sky atlantic")
        || g.contains("sky max")
        || g.contains("sky witness")
        || g.contains("sky showcase")
    {
        return Some(ContentCategory::Entertainment);
    }
    None
}

fn map_ireland_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("sport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("news") {
        return Some(ContentCategory::News);
    }
    if g.contains("film") || g.contains("movie") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("documentar") {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("entertainment") {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("rte") || g.contains("terrestrial") || g.contains("freeview") {
        return Some(ContentCategory::Local);
    }
    if g.contains("radio") {
        return Some(ContentCategory::Radio);
    }
    if g.contains("children") || g.contains("kids") {
        return Some(ContentCategory::Children);
    }
    if g.contains("music") {
        return Some(ContentCategory::Music);
    }
    None
}

fn map_us_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("sport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("news") {
        return Some(ContentCategory::News);
    }
    if g.contains("movie") || g.contains("hbo") || g.contains("showtime") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("documentar") || g.contains("history") || g.contains("discovery") {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("entertainment") || g.contains("comedy") || g.contains("drama") {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("abc")
        || g.contains("cbs")
        || g.contains("nbc")
        || g.contains("fox")
        || g.contains("pbs")
        || g.contains("network")
    {
        return Some(ContentCategory::Local);
    }
    if g.contains("radio") {
        return Some(ContentCategory::Radio);
    }
    if g.contains("kids") || g.contains("disney") || g.contains("nick") {
        return Some(ContentCategory::Children);
    }
    if g.contains("music") {
        return Some(ContentCategory::Music);
    }
    if g.contains("adult") || g.contains("xxx") {
        return Some(ContentCategory::Adult);
    }
    None
}

fn map_ca_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("adult") || g.contains("xxx") || g.contains("18+") {
        return Some(ContentCategory::Adult);
    }
    if g.contains("sport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("news") || g.contains("nouvelles") || g.contains("actualit") {
        return Some(ContentCategory::News);
    }
    // Kids before Entertainment so "jeunesse"/"famille" never fall through
    if g.contains("kids")
        || g.contains("child")
        || g.contains("enfant")
        || g.contains("jeunesse")
        || g.contains("famille")
        || g.contains("family")
    {
        return Some(ContentCategory::Children);
    }
    if g.contains("document")
        || g.contains("docu")
        || g.contains("discovery")
        || is_documentary_factual(g)
        || g.contains("nature")
    {
        return Some(ContentCategory::Documentaries);
    }
    // "ciné" (not "cine") so the accented "Cinéma" matches without
    // false-positives from e.g. "racing" containing "cin".
    if g.contains("movie")
        || g.contains("film")
        || g.contains("cinema")
        || g.contains("ciné")
        || g.contains("crave")
    {
        return Some(ContentCategory::Movies);
    }
    if g.contains("music") || g.contains("musique") {
        return Some(ContentCategory::Music);
    }
    if g.contains("radio") {
        return Some(ContentCategory::Radio);
    }
    if g.contains("shop") || g.contains("achat") {
        return Some(ContentCategory::Shopping);
    }
    if g.contains("religi") || g.contains("faith") || g.contains("gospel") {
        return Some(ContentCategory::Religious);
    }
    if g.contains("entertainment")
        || g.contains("divertissement")
        || g.contains("general")
        || g.contains("variety")
        || g.contains("lifestyle")
    {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("local") || g.contains("network") {
        return Some(ContentCategory::Local);
    }
    None
}

fn map_germany_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("sport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("nachrichten") || g.contains("news") {
        return Some(ContentCategory::News);
    }
    if g.contains("film") || g.contains("kino") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("dokumentation") || g.contains("dokuformat") {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("unterhaltung") || g.contains("entertainment") {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("ard") || g.contains("zdf") || g.contains("terrestrisch") {
        return Some(ContentCategory::Local);
    }
    if g.contains("funk") || g.contains("radio") {
        return Some(ContentCategory::Radio);
    }
    if g.contains("kinder") || g.contains("kids") {
        return Some(ContentCategory::Children);
    }
    if g.contains("musik") || g.contains("music") {
        return Some(ContentCategory::Music);
    }
    None
}

fn map_france_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("sport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("information") || g.contains("news") || g.contains("actualités") {
        return Some(ContentCategory::News);
    }
    if g.contains("film") || g.contains("cinéma") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("documentair") {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("divertissement") || g.contains("entertainment") {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("france") || g.contains("terrestr") {
        return Some(ContentCategory::Local);
    }
    if g.contains("radio") || g.contains("musique") {
        return Some(ContentCategory::Music);
    }
    if g.contains("jeunesse") || g.contains("enfants") {
        return Some(ContentCategory::Children);
    }
    None
}

fn map_italy_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("sport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("news") || g.contains("cronaca") {
        return Some(ContentCategory::News);
    }
    if g.contains("film") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("documentario") {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("intrattenimento") {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("rai") || g.contains("mediaset") || g.contains("nazionale") {
        return Some(ContentCategory::Local);
    }
    if g.contains("radio") {
        return Some(ContentCategory::Radio);
    }
    if g.contains("bambini") || g.contains("ragazzi") {
        return Some(ContentCategory::Children);
    }
    if g.contains("musica") {
        return Some(ContentCategory::Music);
    }
    None
}

fn map_spain_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("deport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("noticias") || g.contains("news") {
        return Some(ContentCategory::News);
    }
    if g.contains("película") || g.contains("cine") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("documentales") {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("entretenimiento") || g.contains("entertainment") {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("tvt") || g.contains("televisión") || g.contains("nacional") {
        return Some(ContentCategory::Local);
    }
    if g.contains("radio") {
        return Some(ContentCategory::Radio);
    }
    if g.contains("infantil") || g.contains("niños") {
        return Some(ContentCategory::Children);
    }
    if g.contains("música") {
        return Some(ContentCategory::Music);
    }
    None
}

fn map_netherlands_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("sport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("nieuws") || g.contains("news") {
        return Some(ContentCategory::News);
    }
    if g.contains("film") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("documentaire") {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("entertainment") {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("npo") || g.contains("rtl") || g.contains("sbs") {
        return Some(ContentCategory::Local);
    }
    if g.contains("radio") {
        return Some(ContentCategory::Radio);
    }
    if g.contains("kinderen") || g.contains("kids") {
        return Some(ContentCategory::Children);
    }
    if g.contains("muziek") || g.contains("music") {
        return Some(ContentCategory::Music);
    }
    None
}

fn map_belgium_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("sport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("actualité") || g.contains("nieuws") || g.contains("news") {
        return Some(ContentCategory::News);
    }
    if g.contains("film") || g.contains("cinema") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("documentaire") {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("divertissement") || g.contains("entertainment") {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("vrt") || g.contains("rtbf") || g.contains("één") || g.contains("deux") {
        return Some(ContentCategory::Local);
    }
    if g.contains("radio") || g.contains("musique") {
        return Some(ContentCategory::Music);
    }
    if g.contains("jeunesse") || g.contains("jeugd") {
        return Some(ContentCategory::Children);
    }
    None
}

fn map_austria_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("sport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("nachrichten") || g.contains("news") {
        return Some(ContentCategory::News);
    }
    if g.contains("film") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("dokumentation") {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("unterhaltung") {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("orf") || g.contains("terrestrisch") {
        return Some(ContentCategory::Local);
    }
    if g.contains("radio") || g.contains("musik") {
        return Some(ContentCategory::Music);
    }
    if g.contains("kinder") {
        return Some(ContentCategory::Children);
    }
    None
}

fn map_switzerland_genre(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    if g.contains("sport") {
        return Some(ContentCategory::Sport);
    }
    if g.contains("nachrichten")
        || g.contains("actualité")
        || g.contains("notízia")
        || g.contains("news")
    {
        return Some(ContentCategory::News);
    }
    if g.contains("film") || g.contains("cinema") || g.contains("kino") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("dokumentation") || g.contains("documentaire") {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("unterhaltung")
        || g.contains("divertissement")
        || g.contains("entertainment")
    {
        return Some(ContentCategory::Entertainment);
    }
    if g.contains("srf") || g.contains("rts") || g.contains("rsi") || g.contains("terrestr") {
        return Some(ContentCategory::Local);
    }
    if g.contains("radio") || g.contains("musik") || g.contains("musique") {
        return Some(ContentCategory::Music);
    }
    if g.contains("kinder") || g.contains("enfants") || g.contains("bambini") {
        return Some(ContentCategory::Children);
    }
    None
}

fn map_unknown_genre(_genre: &str) -> Option<ContentCategory> {
    None
}

// ── Generic group mapping ────────────────────────────────────────────────────

/// Country-agnostic mapping of common (mostly English) provider group-titles,
/// including abbreviations and league names.  Ordered most-specific →
/// most-generic so a broad bucket never swallows a sharper one.
/// Returns `None` when nothing matches (caller → Others).
pub fn map_generic_group(genre: &str) -> Option<ContentCategory> {
    let g_lower = genre.to_lowercase();
    let g = g_lower.trim();
    // Adult
    if g.contains("adult") || g.contains("xxx") || g.contains("18+") {
        return Some(ContentCategory::Adult);
    }
    // Sport — generic + brands + league names/abbreviations
    if g.contains("sport")
        || g.contains("dazn")
        || g.contains("espn")
        || g.contains("eurosport")
        || g.contains("league")
        || g.contains("epl")
        || g.contains("efl")
        || g.contains("uefa")
        || g.contains("spfl")
        || g.contains("nfl")
        || g.contains("nba")
        || g.contains("mlb")
        || g.contains("nhl")
        || g.contains("gaa")
        || g.contains("rugby")
        || g.contains("darts")
        || g.contains("golf")
        || g.contains("tennis")
        || g.contains("cricket")
        || g.contains("ufc")
        || g.contains("wwe")
        || g.contains("boxing")
        // Combat sports arrive under provider genres that never say "sport". Measured on
        // `.246`, 2026-09-04: `Glory Kickboxing` and `One Championship TV` both carry the
        // provider genre "Fight Club", so the channel was not SPORT and their events were
        // filed by the TV Times as OUT OF THE BOX. `UFC 24/7` shares that genre and escaped
        // only because "ufc" is also a title hint. Mirrors the Kotlin twin exactly.
        || g.contains("fight")
        || g.contains("mma")
        || g.contains("kickbox")
        || g.contains("muay thai")
        || g.contains("bellator")
        || g.contains("grappl")
        || g.contains("jiu-jitsu")
        || g.contains("jiu jitsu")
        || g.contains("one championship")
        || g.contains("motogp")
        || g.contains("racing")
        || g.contains("skysp")
        || g.contains("ppv")
        || g.contains("live events")
        || g.contains("champ")
    {
        return Some(ContentCategory::Sport);
    }
    if g.contains("news") {
        return Some(ContentCategory::News);
    }
    if g.contains("kids")
        || g.contains("child")
        || g.contains("cartoon")
        || g.contains("junior")
        || g.contains("cbbc")
        || g.contains("cbeebies")
        || g.contains("boomerang")
        || g.contains("nick")
    {
        return Some(ContentCategory::Children);
    }
    if g.contains("doc")
        || g.contains("discovery")
        || g.contains("history")
        || g.contains("nat geo")
        || g.contains("nature")
        || is_documentary_factual(g)
        || g.contains("science")
        || g.contains("wildlife")
    {
        return Some(ContentCategory::Documentaries);
    }
    if g.contains("movie") || g.contains("cinema") || g.contains("film") || g.contains("cine") {
        return Some(ContentCategory::Movies);
    }
    if g.contains("music")
        || g.contains("mtv")
        || g.contains("vevo")
        || g.contains("hits")
        || g.contains("kerrang")
    {
        return Some(ContentCategory::Music);
    }
    if g.contains("radio") {
        return Some(ContentCategory::Radio);
    }
    if g.contains("international") || g.contains("foreign") {
        return Some(ContentCategory::International);
    }
    if g.contains("shop") || g.contains("qvc") || g.contains("ideal world") {
        return Some(ContentCategory::Shopping);
    }
    if g.contains("religi")
        || g.contains("christian")
        || g.contains("islam")
        || g.contains("gospel")
        || g.contains("faith")
        || g.contains("church")
    {
        return Some(ContentCategory::Religious);
    }
    // Regional
    if g.contains("regional") || REG_TOKEN.is_match(&g) {
        return Some(ContentCategory::Regional);
    }
    // Local / terrestrial — UK PSBs
    if g.contains("local")
        || g.contains("terrestrial")
        || g.contains("freeview")
        || g.contains("freesat")
        || g.contains("bbc")
        || ITV_TOKEN.is_match(&g)
        || g.contains("channel 4")
        || g.contains("channel 5")
        || g.contains("s4c")
    {
        return Some(ContentCategory::Local);
    }
    // Entertainment — broad bucket + the "ent" abbreviation (whole word)
    if g.contains("entertainment")
        || ENT_TOKEN.is_match(&g)
        || g.contains("general")
        || g.contains("drama")
        || g.contains("comedy")
        || g.contains("lifestyle")
        || g.contains("reality")
        || g.contains("variety")
        || g.contains("arts")
    {
        return Some(ContentCategory::Entertainment);
    }
    None
}

// ── Country mapper dispatch ──────────────────────────────────────────────────

pub type GenreMapper = fn(&str) -> Option<ContentCategory>;

/// Get the category mapping function for a specific country.
pub fn get_mapper_for_country(country_code: &str) -> GenreMapper {
    match country_code.to_uppercase().as_str() {
        "GB" => map_uk_genre,
        "IE" => map_ireland_genre,
        "US" => map_us_genre,
        "CA" => map_ca_genre,
        "DE" => map_germany_genre,
        "FR" => map_france_genre,
        "IT" => map_italy_genre,
        "ES" => map_spain_genre,
        "NL" => map_netherlands_genre,
        "BE" => map_belgium_genre,
        "AT" => map_austria_genre,
        "CH" => map_switzerland_genre,
        _ => map_unknown_genre,
    }
}

// ── categorize ───────────────────────────────────────────────────────────────

/// Resolve a channel's category from its provider group-title.
///
/// `detected_region` is the output of `RegionalChannelFilter.detectRegionInName`
/// (called by the Kotlin bridge and passed in here, since this crate can't
/// depend on the regional filter).
pub fn categorize(
    country_code: Option<&str>,
    genre: &str,
    channel_name: &str,
    user_region: Option<&str>,
    detected_region: Option<&str>,
) -> ContentCategory {
    // A timeshift belongs wherever its parent belongs — answered by asking the same
    // question of the parent's name. Without this, Channel 4 +1 and Channel 4 split
    // across tabs and no amount of curation sort can reunite them (Kotlin parity).
    let timeshift_base = {
        let stripped = channel_name_keys::strip_timeshift_marker(channel_name);
        let base = stripped.trim();
        if channel_name_keys::is_timeshift(channel_name)
            && !base.is_empty()
            && !base.eq_ignore_ascii_case(channel_name.trim())
        {
            Some(base.to_string())
        } else {
            None
        }
    };
    if let Some(base) = timeshift_base {
        let parent = categorize(country_code, genre, &base, user_region, detected_region);
        // Parent under a useless folder ("UK: Plus 1") lands in Others — keep the +1 in
        // Entertainment rather than inventing a Plus 1 guide tab (Kotlin parity).
        return if parent != ContentCategory::Others {
            parent
        } else {
            ContentCategory::Entertainment
        };
    }

    let cc: Option<&str> = country_code.filter(|s| !s.trim().is_empty());
    let by_genre: Option<ContentCategory> = if let Some(cc) = cc {
        let mapper = get_mapper_for_country(cc);
        mapper(genre).or_else(|| map_generic_group(genre))
    } else {
        map_generic_group(genre)
    };
    // UK brand rules (Kotlin parity): over a soft filing only, never a specialist genre, so
    // "ITV4 Racing Extra" under Sports stays Sport.
    if cc.is_some_and(|code| code.eq_ignore_ascii_case("GB"))
        && matches_or_none(
            by_genre,
            &[
                ContentCategory::Others,
                ContentCategory::Entertainment,
                ContentCategory::Movies,
                ContentCategory::Local,
                ContentCategory::Regional,
            ],
        )
        && (UK_SKY_COMEDY_NAME_REGEX.is_match(channel_name)
            || UK_ITV_ENTERTAINMENT_NAME_REGEX.is_match(channel_name))
    {
        return ContentCategory::Entertainment;
    }
    // Legend is a film channel in every spelling a provider uses (Kotlin parity).
    if cc.is_some_and(|code| code.eq_ignore_ascii_case("GB")) && is_legend_itself(channel_name) {
        return ContentCategory::Movies;
    }

    // Radio rescue
    if looks_like_radio_name(channel_name)
        && matches_or_none(
            by_genre,
            &[
                ContentCategory::Entertainment,
                ContentCategory::Music,
                ContentCategory::Others,
            ],
        )
    {
        return ContentCategory::Radio;
    }

    // News rescue — checked BEFORE Local so "BBC News" doesn't fall into Local
    if looks_like_news_name(channel_name)
        && matches_or_none(
            by_genre,
            &[
                ContentCategory::Entertainment,
                ContentCategory::Local,
                ContentCategory::Others,
                ContentCategory::Documentaries,
            ],
        )
    {
        return ContentCategory::News;
    }

    // Regional rescue — checked BEFORE Local so a regional variant isn't
    // absorbed into Local just because it also matches the PSB brand
    if looks_like_regional_name(cc, channel_name, user_region, detected_region)
        && matches_or_none(
            by_genre,
            &[
                ContentCategory::Entertainment,
                ContentCategory::Local,
                ContentCategory::Others,
            ],
        )
    {
        return ContentCategory::Regional;
    }

    // Local rescue — UK PSB/terrestrial channels
    if looks_like_local_name(channel_name)
        && matches_or_none(
            by_genre,
            &[ContentCategory::Entertainment, ContentCategory::Others],
        )
    {
        return ContentCategory::Local;
    }

    // Canadian local rescue — CBC/CTV/Global/CityTV
    if looks_like_canadian_local_name(cc, channel_name)
        && matches_or_none(
            by_genre,
            &[ContentCategory::Entertainment, ContentCategory::Others],
        )
    {
        return ContentCategory::Local;
    }

    // Music rescue — real major-provider TV music channels
    if looks_like_music_name(cc, channel_name)
        && matches_or_none(
            by_genre,
            &[ContentCategory::Entertainment, ContentCategory::Others],
        )
    {
        return ContentCategory::Music;
    }

    // Documentary rescue
    if looks_like_documentary_name(channel_name)
        && matches_or_none(
            by_genre,
            &[ContentCategory::Entertainment, ContentCategory::Others],
        )
    {
        return ContentCategory::Documentaries;
    }

    // Movies rescue
    if looks_like_movies_name(channel_name)
        && matches_or_none(
            by_genre,
            &[ContentCategory::Entertainment, ContentCategory::Others],
        )
    {
        return ContentCategory::Movies;
    }

    // Kids rescue
    if looks_like_kids_name(channel_name)
        && matches_or_none(
            by_genre,
            &[ContentCategory::Entertainment, ContentCategory::Others],
        )
    {
        return ContentCategory::Children;
    }

    // Shopping rescue
    if looks_like_shopping_name(channel_name)
        && matches_or_none(
            by_genre,
            &[ContentCategory::Entertainment, ContentCategory::Others],
        )
    {
        return ContentCategory::Shopping;
    }

    // International rescue — ATN etc. filed under UK Entertainment
    if looks_like_international_name(channel_name)
        && matches_or_none(
            by_genre,
            &[ContentCategory::Entertainment, ContentCategory::Others],
        )
    {
        return ContentCategory::International;
    }

    // Religious rescue
    if looks_like_religious_name(channel_name)
        && matches_or_none(
            by_genre,
            &[ContentCategory::Entertainment, ContentCategory::Others],
        )
    {
        return ContentCategory::Religious;
    }

    // Sport rescue
    if looks_like_sport_name(channel_name)
        && matches_or_none(
            by_genre,
            &[ContentCategory::Entertainment, ContentCategory::Others],
        )
    {
        return ContentCategory::Sport;
    }

    // Entertainment brand rescue — catches brands like "Dave"/"Together"/etc.
    // Also covers C4/C5 diginet +1s under "UK: Plus 1", and ITV2/3/4/Be under "UK: Freeview"/"UK: ITV".
    if looks_like_entertainment_name(channel_name)
        && matches_or_none(
            by_genre,
            &[
                ContentCategory::Others,
                ContentCategory::Documentaries,
                ContentCategory::Local,
            ],
        )
    {
        return ContentCategory::Entertainment;
    }

    // Irish PSB rescue — RTE/TG4/Virgin under region-ish group names that map
    // to nothing.
    if matches_or_none(by_genre, &[ContentCategory::Others]) {
        if let Some(cat) = irish_psb_category(channel_name) {
            return cat;
        }
    }

    // A timeshift is never a category of its own. Providers give the +1 feeds a folder
    // ("UK: Plus 1") that names no genre, so any brand no rescue above recognises fell to
    // Others — and Others fans out per provider folder in the guide, which turned that one
    // folder into a "Plus 1" section sitting alongside Sport and Movies. Whatever a +1 is, it
    // is a second airing of a channel that already has a home, so the broad bucket beats a
    // section of its own.
    let fallback = by_genre.unwrap_or(ContentCategory::Others);
    if fallback == ContentCategory::Others
        && (channel_name_keys::is_timeshift(channel_name)
            || genre.to_lowercase().contains("hdhomerun"))
    {
        return ContentCategory::Entertainment;
    }
    fallback
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {

    // 5Select became 5Drama (Oct 2026); both names stay Entertainment. Twin of ProviderGenreMapperTest.
    #[test]
    fn five_drama_and_five_select_are_entertainment() {
        for (genre, name) in [
            ("UK: Entertainment", "UK: 5DRAMA SD"),
            ("Live TV", "[UK] 5 DRAMA"),
            ("UK: Entertainment", "UK: 5SELECT SD"),
        ] {
            assert_eq!(
                categorize(Some("GB"), genre, name, None, None),
                ContentCategory::Entertainment,
                "{name:?} must categorise as ENTERTAINMENT"
            );
        }
    }

    // Measured on `.246`, 2026-09-04: Glory Kickboxing and One Championship TV carry the
    // provider genre "Fight Club", so the channel was never SPORT and the TV Times filed their
    // events under OUT OF THE BOX. UFC 24/7 shares the genre and escaped only because "ufc" is
    // also a title hint — the container was wrong for all three, one was rescued by accident.
    #[test]
    fn fight_club_is_a_sports_genre() {
        for (genre, name) in [
            ("Fight Club", "Glory Kickboxing"),
            ("Fight Club", "One Championship TV"),
            ("Fight Club", "UFC 24/7"),
            ("MMA", "Some Fight Channel"),
            ("Kickboxing", "Another Fight Channel"),
        ] {
            assert_eq!(
                categorize(None, genre, name, None, None),
                ContentCategory::Sport,
                "provider genre {genre:?} on {name:?} must categorise as SPORT"
            );
        }
    }

    use super::*;

    #[test]
    fn categorize_sport_genre() {
        assert_eq!(
            categorize(None, "Sports", "", None, None),
            ContentCategory::Sport
        );
    }

    #[test]
    fn categorize_movies_genre() {
        assert_eq!(
            categorize(None, "Movies", "", None, None),
            ContentCategory::Movies
        );
    }

    #[test]
    fn categorize_news_with_bbc_news_channel_name() {
        assert_eq!(
            categorize(None, "News", "BBC News", None, None),
            ContentCategory::News
        );
    }

    #[test]
    fn categorize_entertainment_with_bbc_one_channel_name() {
        assert_eq!(
            categorize(None, "Entertainment", "BBC One", None, None),
            ContentCategory::Local
        );
    }

    #[test]
    fn map_generic_group_adult() {
        assert_eq!(
            map_generic_group("adult"),
            Some(ContentCategory::Adult)
        );
    }

    #[test]
    fn map_generic_group_kids() {
        assert_eq!(
            map_generic_group("kids"),
            Some(ContentCategory::Children)
        );
    }

    // ── Additional coverage ─────────────────────────────────────────────────

    #[test]
    fn categorize_radio_name_rescue() {
        // "Absolute Radio" under a generic Entertainment group → Radio
        assert_eq!(
            categorize(None, "Entertainment", "Absolute Radio", None, None),
            ContentCategory::Radio
        );
    }

    #[test]
    fn categorize_sport_name_rescue() {
        // "ESPN" channel with a generic genre → Sport
        assert_eq!(
            categorize(None, "Live TV", "ESPN", None, None),
            ContentCategory::Sport
        );
        assert_eq!(
            categorize(Some("GB"), "Entertainment", "Racing.UK", None, None),
            ContentCategory::Sport
        );
        assert_eq!(
            categorize(Some("GB"), "Entertainment", "Racing-UK", None, None),
            ContentCategory::Sport
        );
        assert_eq!(
            categorize(Some("GB"), "Entertainment", "RacingUK", None, None),
            ContentCategory::Sport
        );
    }

    #[test]
    fn categorize_itv2_is_entertainment_not_local() {
        // ITV2 is an entertainment diginet, NOT local PSB — the ITV lookahead
        // in localNameRegex must reject it.
        assert_eq!(
            categorize(Some("GB"), "UK: Plus 1", "ITV2", None, None),
            ContentCategory::Entertainment
        );
    }

    #[test]
    fn categorize_itv1_is_local() {
        // ITV1 is the flagship PSB feed — Local.
        assert_eq!(
            categorize(Some("GB"), "UK: General", "ITV1 London", None, None),
            ContentCategory::Local
        );
    }

    #[test]
    fn categorize_quest_is_entertainment() {
        // "Quest" and "Quest Red" are both Entertainment.
        assert_eq!(
            categorize(None, "Live TV", "Quest", None, None),
            ContentCategory::Entertainment
        );
        assert_eq!(
            categorize(None, "Live TV", "Quest Red", None, None),
            ContentCategory::Entertainment
        );
        assert_eq!(
            categorize(Some("GB"), "UK Docs", "Bloomberg", None, None),
            ContentCategory::News
        );
        assert_eq!(
            categorize(Some("GB"), "Live TV", "Turbo", None, None),
            ContentCategory::Documentaries
        );
        assert_eq!(
            categorize(Some("GB"), "Live TV", "Discovery Turbo", None, None),
            ContentCategory::Documentaries
        );
        assert_eq!(
            categorize(Some("GB"), "HDHomeRun", "BBC 6 Music", None, None),
            ContentCategory::Radio
        );
        assert_eq!(
            categorize(Some("GB"), "HDHomeRun", "Sky Mix", None, None),
            ContentCategory::Entertainment
        );
    }

    /// The provider's "UK: Plus 1" folder names no genre, so a brand no rescue recognises used
    /// to fall to Others — and Others fans out per folder in the guide, giving the lineup a
    /// "Plus 1" section of its own alongside Sport and Movies.
    #[test]
    fn categorize_puts_a_plus_one_in_a_real_section_never_others() {
        for name in [
            "UK: U&ALIBI +1 SD",
            "UK: U&GOLD +1 SD",
            "UK: COMEDY CENTRAL +1 SD",
            "UK: FOOD NETWORK +1 SD",
            "UK: TLC +1 SD",
        ] {
            assert_eq!(
                categorize(Some("GB"), "UK: Plus 1", name, None, None),
                ContentCategory::Entertainment,
                "{name} should follow its base channel into Entertainment"
            );
        }
        // Brands a rescue does recognise still keep their own section.
        assert_eq!(
            categorize(Some("GB"), "UK: Plus 1", "UK: DISCOVERY +1 SD", None, None),
            ContentCategory::Documentaries
        );
        assert_eq!(
            categorize(Some("GB"), "UK: Plus 1", "UK: FILM4 +1 SD", None, None),
            ContentCategory::Movies
        );
        assert_eq!(
            categorize(Some("GB"), "UK: Plus 1", "UK: HORROR XTRA +1 SD", None, None),
            ContentCategory::Movies
        );
    }

    #[test]
    fn categorize_legend_xtra_is_movies_not_entertainment() {
        // "Legend Xtra" is Movies; with no country, bare "Legend" stays Entertainment. In the UK
        // it is Movies in every spelling -- see the test below.
        assert_eq!(
            categorize(None, "Live TV", "Legend Xtra", None, None),
            ContentCategory::Movies
        );
        assert_eq!(
            categorize(None, "Live TV", "Legend", None, None),
            ContentCategory::Entertainment
        );
    }

    /// The owner's sorting pass, 11 Sep 2026 (Kotlin twin in ProviderGenreMapperTest).
    #[test]
    fn categorize_uk_brand_rules_respect_a_specialist_genre() {
        assert_eq!(categorize(Some("GB"), "UK Movies", "ITV2 HD", None, None), ContentCategory::Entertainment);
        assert_eq!(categorize(Some("GB"), "UK Movies", "Sky Comedy", None, None), ContentCategory::Entertainment);
        assert_eq!(categorize(Some("GB"), "Sports", "ITV4 Racing Extra", None, None), ContentCategory::Sport);
    }

    #[test]
    fn categorize_legend_is_movies_in_every_uk_spelling() {
        assert_eq!(categorize(Some("GB"), "UK Ent", "Legend", None, None), ContentCategory::Movies);
        assert_eq!(categorize(Some("GB"), "Live TV", "316 UK - LEGEND HD", None, None), ContentCategory::Movies);
        assert_eq!(categorize(Some("GB"), "UK: Plus 1", "Legend +1", None, None), ContentCategory::Movies);
    }

    #[test]
    fn categorize_canadian_local_rescue() {
        assert_eq!(
            categorize(Some("CA"), "CA: All", "CBC Toronto", None, None),
            ContentCategory::Local
        );
    }

    #[test]
    fn categorize_ctv_comedy_is_not_canadian_local() {
        // CTV Comedy should NOT be pulled into Local by the CTV lookahead guard.
        let result = categorize(Some("CA"), "CA: All", "CTV Comedy", None, None);
        assert_ne!(result, ContentCategory::Local);
    }

    #[test]
    fn categorize_irish_psb_rescue() {
        assert_eq!(
            categorize(None, "UK/EIR: Regions", "RTE One", None, None),
            ContentCategory::Entertainment
        );
        assert_eq!(
            categorize(None, "UK/EIR: Regions", "RTE Junior", None, None),
            ContentCategory::Children
        );
    }

    #[test]
    fn categorize_kids_name_rescue() {
        assert_eq!(
            categorize(None, "Live TV", "CBeebies", None, None),
            ContentCategory::Children
        );
    }

    #[test]
    fn map_generic_group_news() {
        assert_eq!(map_generic_group("news"), Some(ContentCategory::News));
    }

    #[test]
    fn map_generic_group_sport_league() {
        assert_eq!(map_generic_group("EPL"), Some(ContentCategory::Sport));
        assert_eq!(map_generic_group("UEFA"), Some(ContentCategory::Sport));
    }

    #[test]
    fn map_uk_genre_sport() {
        assert_eq!(map_uk_genre("Sky Sports"), Some(ContentCategory::Sport));
    }

    #[test]
    fn map_uk_genre_local() {
        assert_eq!(map_uk_genre("BBC"), Some(ContentCategory::Local));
    }

    #[test]
    fn map_ca_genre_bilingual_news() {
        assert_eq!(map_ca_genre("Nouvelles"), Some(ContentCategory::News));
    }

    #[test]
    fn map_germany_genre_local() {
        assert_eq!(map_germany_genre("ARD"), Some(ContentCategory::Local));
    }

    #[test]
    fn map_france_genre_movies() {
        assert_eq!(map_france_genre("Cinéma"), Some(ContentCategory::Movies));
    }

    #[test]
    fn get_mapper_for_country_unknown() {
        let mapper = get_mapper_for_country("XX");
        assert_eq!(mapper("anything"), None);
    }

    #[test]
    fn get_mapper_for_country_gb() {
        let mapper = get_mapper_for_country("gb");
        assert_eq!(mapper("Sky Sports"), Some(ContentCategory::Sport));
    }
}
