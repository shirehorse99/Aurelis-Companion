//! Shared Aurelis channel-name heuristics, ported byte-for-byte from Kotlin.
//!
//! This crate ports the pure-compute heuristics that every playlist sync, EPG match,
//! channel sort, and genre categorisation depends on:
//!
//! - [`ContentCategory`] — enum mirroring `com.aurelis.tv.model.ContentCategory`
//! - [`title_metadata`] — `TitleMetadataParser.parse`
//! - [`channel_name_aliases`] — `ChannelNameAliases.expand`
//! - [`channel_name_keys`] — `ChannelNameKeys` (foldDecorations, normalizeForMatch,
//!   normalizeForPlanMatch, normalizeWordsForMatch, looseKeyMatch, isTimeshift,
//!   stripTimeshiftMarker, isSkySportsNews, compactForMatch)
//! - [`country_affinity`] — `CountryAffinity.detectForChannel` + country catalog
//! - [`locale_channel_plan`] — `LocaleChannelPlan.rankFor` / `guideRank`
//! - [`provider_genre_mapper`] — `ProviderGenreMapper.categorize`
//! - [`channel_sort_keys`] — `ChannelSortKeys` (extractLcn, lcnSortKey, etc.)
//! - [`channel_curation_keys`] — `ChannelCurationKeys` (familyKey, brandKey)
//!
//! All quirks documented in the Kotlin source are preserved, not "fixed".

use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::{Env, EnvUnowned};
use jni::sys::jboolean;
pub const ENGINE_VERSION: i32 = 1;

pub mod locale_channel_plan;
pub mod provider_genre_mapper;
pub mod country_affinity;
pub mod channel_sort_keys;
pub mod channel_curation_keys;

// ── ContentCategory enum ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentCategory {
    Local,
    Entertainment,
    Documentaries,
    Movies,
    Sport,
    Uhd,
    News,
    Children,
    Music,
    Shopping,
    Religious,
    Radio,
    International,
    Regional,
    Others,
    Adult,
    Hidden,
}

impl ContentCategory {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "LOCAL" => Some(Self::Local),
            "ENTERTAINMENT" => Some(Self::Entertainment),
            "DOCUMENTARIES" => Some(Self::Documentaries),
            "MOVIES" => Some(Self::Movies),
            "SPORT" => Some(Self::Sport),
            "UHD" => Some(Self::Uhd),
            "NEWS" => Some(Self::News),
            "CHILDREN" => Some(Self::Children),
            "MUSIC" => Some(Self::Music),
            "SHOPPING" => Some(Self::Shopping),
            "RELIGIOUS" => Some(Self::Religious),
            "RADIO" => Some(Self::Radio),
            "INTERNATIONAL" => Some(Self::International),
            "REGIONAL" => Some(Self::Regional),
            "OTHERS" => Some(Self::Others),
            "ADULT" => Some(Self::Adult),
            "HIDDEN" => Some(Self::Hidden),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Local => "LOCAL",
            Self::Entertainment => "ENTERTAINMENT",
            Self::Documentaries => "DOCUMENTARIES",
            Self::Movies => "MOVIES",
            Self::Sport => "SPORT",
            Self::Uhd => "UHD",
            Self::News => "NEWS",
            Self::Children => "CHILDREN",
            Self::Music => "MUSIC",
            Self::Shopping => "SHOPPING",
            Self::Religious => "RELIGIOUS",
            Self::Radio => "RADIO",
            Self::International => "INTERNATIONAL",
            Self::Regional => "REGIONAL",
            Self::Others => "OTHERS",
            Self::Adult => "ADULT",
            Self::Hidden => "HIDDEN",
        }
    }
}

// ── TitleMetadataParser ──────────────────────────────────────────────────────

pub mod title_metadata {
    use regex::Regex;
    use serde::{Deserialize, Serialize};
    use std::sync::LazyLock;

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    pub struct ParsedMetadata {
        pub year: Option<String>,
        pub quality: Option<String>,
        pub season_episode: Option<String>,
        pub language: Option<String>,
    }

    static YEAR_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b((?:19|20)\d{2})\b").unwrap());
    static SE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[Ss](\d{1,2})[Ee](\d{1,2})").unwrap());
    static SE_LONG_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[Ss]eason\s+(\d{1,2})\s+[Ee]pisode\s+(\d{1,2})").unwrap());
    static LANG_CODE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)[()\[\]]?\s*(ENG|FRA|GER|SPA|ITA|JPN|KOR|RUS|POL|NLD|SWE|NOR|DAN|FIN|POR|TUR|GRE|HUN|CZE|ROU|BUL|HRV|SLV)\s*[()\[\]]?").unwrap()
    });
    static LANG_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(English|French|German|Spanish|Italian|Japanese|Korean|Russian|Dutch|Swedish|Norwegian)\b").unwrap()
    });

    pub fn parse(title: &str) -> ParsedMetadata {
        let normalized = title.trim();
        ParsedMetadata {
            year: extract_year(normalized),
            quality: extract_quality(normalized),
            season_episode: extract_season_episode(normalized),
            language: extract_language(normalized),
        }
    }

    fn extract_year(title: &str) -> Option<String> {
        YEAR_REGEX.captures(title).map(|c| c[1].to_string())
    }

    static QUALITY_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
        [
            ("4K", "4K"), ("UHD", "4K"),
            ("1080p?", "1080p"), ("720p?", "720p"), ("480p?", "480p"),
            ("HDR", "HDR"), ("HEVC", "HEVC"),
            ("x265", "HEVC"), ("h\\.?265", "HEVC"),
            ("x264", "H.264"), ("h\\.?264", "H.264"),
            ("Dolby", "Dolby"),
        ]
        .into_iter()
        .map(|(pat, label)| (Regex::new(&format!("(?i){}", pat)).unwrap(), label))
        .collect()
    });

    fn extract_quality(title: &str) -> Option<String> {
        for (re, label) in QUALITY_PATTERNS.iter() {
            if re.is_match(title) {
                return Some((*label).to_string());
            }
        }
        None
    }

    fn extract_season_episode(title: &str) -> Option<String> {
        for re in [&*SE_REGEX, &*SE_LONG_REGEX] {
            if let Some(caps) = re.captures(title) {
                let s: u32 = caps[1].parse().unwrap_or(0);
                let e: u32 = caps[2].parse().unwrap_or(0);
                return Some(format!("S{:02}E{:02}", s, e));
            }
        }
        None
    }

    fn extract_language(title: &str) -> Option<String> {
        for re in [&*LANG_CODE_REGEX, &*LANG_NAME_REGEX] {
            if let Some(m) = re.find(title) {
                let code = m.as_str().trim_matches(|c| c == '(' || c == ')' || c == '[' || c == ']' || c == ' ');
                return Some(code[..code.len().min(3)].to_uppercase());
            }
        }
        None
    }
}

// ── ChannelNameAliases ───────────────────────────────────────────────────────

pub mod channel_name_aliases {
    use regex::Regex;
    use std::sync::LazyLock;

    struct Rule {
        re: Regex,
        replacement: String,
    }

    static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
        let ic = |p: &str| Regex::new(&format!("(?i){}", p)).unwrap();
        vec![
            // NZ Sky Sport+ / SkySP+ — before SkySp→Sky Sports so the + bank stays singular.
            Rule { re: ic(r"\bsky\s*sp\s*\+"), replacement: "sky sport plus".into() },
            Rule { re: ic(r"\bsky\s*sport\s*\+"), replacement: "sky sport plus".into() },
            Rule { re: ic(r"\bsky\s*sp\b"), replacement: "sky sports".into() },
            Rule { re: ic(r"\bsyfy\b"), replacement: "sky sci-fi".into() },
            // BT Sport became TNT Sports -- one channel (owner, 8 Oct 2026). Plural, to match
            // the brand and the plan's "tntsports1" tokens. These four were Kotlin-only from
            // 17 Aug to 8 Oct 2026, so boxes kept BT Sport and TNT Sports as two rows.
            Rule { re: ic(r"\bbt\s*sports?\b"), replacement: "tnt sports".into() },
            Rule { re: ic(r"\btnt\s*sports?\b"), replacement: "tnt sports".into() },
            Rule { re: ic(r"\beuro\s*sport\b"), replacement: "eurosport".into() },
            Rule { re: ic(r"\beurosport\s*1\b"), replacement: "eurosport 1".into() },
            Rule { re: ic(r"\bcomedy\s*cent\b"), replacement: "comedy central".into() },
            Rule { re: ic(r"\bcomedy\s*xtra\b"), replacement: "comedy central extra".into() },
            Rule { re: ic(r"\bnat\s*geo\b"), replacement: "national geographic".into() },
            Rule { re: ic(r"^\s*id\s*$"), replacement: "investigation discovery".into() },
            Rule { re: ic(r"^\s*5(\s+(?:hd|fhd|uhd|sd))?(\s+tshift1)?$"), replacement: "channel 5$1$2".into() },
            Rule { re: ic(r"^\s*turbo(\s+tshift1)?$"), replacement: "discovery turbo$1".into() },
            // "Discovery Channel" and "Discovery" are one channel (owner, 8 Oct 2026). Whole
            // name only; mirrors ChannelNameAliases.kt.
            Rule { re: ic(r"^\s*discovery\s+channel(\s+tshift1)?$"), replacement: "discovery$1".into() },
            // Talk TV / TalkTV → Talk. Not Talksport.
            Rule { re: ic(r"\btalktv\b"), replacement: "talk".into() },
            Rule { re: ic(r"\btalk\s+television\b"), replacement: "talk".into() },
            Rule { re: ic(r"\btalk\s+tv\b"), replacement: "talk".into() },
            Rule { re: ic(r"\bitv\s+cymru\b"), replacement: "itv1".into() },
            Rule { re: ic(r"\bprem\s*sports?\b"), replacement: "premier sports".into() },
            Rule { re: ic(r"\bsky\s+(action|thriller|premiere|greats|family|drama|animation|hits|select)\b"), replacement: "sky cinema $1".into() },
            Rule { re: ic(r"\bsky\s+(sci-?fi\s*(?:&|and|\+)\s*horror)\b"), replacement: "sky cinema $1".into() },
            Rule { re: ic(r"\bmain\s*ev\b"), replacement: "main event".into() },
            Rule { re: ic(r"\bprem\s*sp\b"), replacement: "premier league".into() },
            // Straight and curly apostrophe, as Kotlin's ['’] -- this read [''] (one char twice).
            Rule { re: ic(r"\bf['’]?\s*ball\b"), replacement: "football".into() },
            Rule { re: ic(r"\bcrime\s*\+?\s*inv\b"), replacement: "crime and investigation".into() },
            Rule { re: ic(r"\bdoc(?:'?s)?\b"), replacement: "documentaries".into() },
            Rule { re: ic(r"\bdocmntrs\b"), replacement: "documentaries".into() },
            Rule { re: ic(r"\bdisc\b"), replacement: "discovery".into() },
            Rule { re: ic(r"\bsky\s*sports?\s+nfl\b"), replacement: "sky sports action".into() },
            Rule { re: ic(r"\bsky\s*sport\s+action\b"), replacement: "sky sports action".into() },
            Rule { re: ic(r"\bsky sports\s+pl\b"), replacement: "sky sports premier league".into() },
            Rule { re: ic(r"\bparl\b"), replacement: "parliament".into() },
        ]
    });

    static ITV_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bitv\b").unwrap());

    pub fn expand(name: &str) -> String {
        expand_with_season(name, christmas_season_now())
    }

    /// Two spellings of one channel (owner, 6 Nov 2026), permanent. Mirrors
    /// ChannelNameAliases.lineupRules.
    static LINEUP_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
        let ic = |p: &str| Regex::new(&format!("(?i){}", p)).unwrap();
        vec![
            Rule { re: ic(r"\bsky\s*cinema\s+premier\b"), replacement: "sky cinema premiere".into() },
            Rule { re: ic(r"\bgreat!?\s*christma\b"), replacement: "great! christmas".into() },
        ]
    });

    /// Christmas renames, 1 Nov - 6 Jan by UTC date. Mirrors ChannelNameAliases.christmasRules.
    static CHRISTMAS_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
        let ic = |p: &str| Regex::new(&format!("(?i){}", p)).unwrap();
        vec![
            Rule { re: ic(r"\bsky\s*cinema\s+select\b"), replacement: "sky cinema christmas".into() },
            Rule { re: ic(r"\bmovies\s*24\b"), replacement: "christmas24".into() },
            Rule { re: ic(r"\bgreat!?\s*romance\b"), replacement: "great! christmas".into() },
        ]
    });

    /// Whether today (UTC) is in the Christmas naming season: November, December, 1-6 January.
    /// Mirrors ChristmasSeason.isActive.
    pub fn christmas_season_now() -> bool {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (month, day) = month_day_from_days(secs.div_euclid(86_400));
        christmas_season(month, day)
    }

    pub fn christmas_season(month: u32, day: u32) -> bool {
        month == 11 || month == 12 || (month == 1 && day <= 6)
    }

    /// Civil month and day from days since 1970-01-01 (Howard Hinnant's algorithm).
    pub(crate) fn month_day_from_days(z: i64) -> (u32, u32) {
        let z = z + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (m, d)
    }

    pub fn expand_with_season(name: &str, christmas: bool) -> String {
        let mut s = name.to_string();
        for rule in RULES.iter() {
            s = rule.re.replace_all(&s, &rule.replacement as &str).to_string();
        }
        // Standalone "ITV" -> "itv1" (excluding 1..9, be, news, sport)
        if ITV_RE.is_match(&s) {
            let mut result = String::with_capacity(s.len() + 8);
            let mut last_idx = 0;
            for mat in ITV_RE.find_iter(&s) {
                result.push_str(&s[last_idx..mat.start()]);
                let after = &s[mat.end()..];
                let after_trimmed = after.trim_start();
                let lower_after = after_trimmed.to_lowercase();
                let is_excluded = if lower_after.is_empty() {
                    false
                } else {
                    let first_char = lower_after.chars().next().unwrap();
                    if ('1'..='9').contains(&first_char) {
                        true
                    } else if lower_after.starts_with("be") && (lower_after.len() == 2 || !lower_after[2..].chars().next().unwrap().is_alphanumeric()) {
                        true
                    } else if lower_after.starts_with("news") && (lower_after.len() == 4 || !lower_after[4..].chars().next().unwrap().is_alphanumeric()) {
                        true
                    } else if lower_after.starts_with("sport") && (lower_after.len() == 5 || !lower_after[5..].chars().next().unwrap().is_alphanumeric()) {
                        true
                    } else {
                        false
                    }
                };
                if is_excluded {
                    result.push_str(&s[mat.start()..mat.end()]);
                } else {
                    result.push_str("itv1");
                }
                last_idx = mat.end();
            }
            result.push_str(&s[last_idx..]);
            s = result;
        }
        for rule in LINEUP_RULES.iter() {
            s = rule.re.replace_all(&s, &rule.replacement as &str).to_string();
        }
        if christmas {
            for rule in CHRISTMAS_RULES.iter() {
                s = rule.re.replace_all(&s, &rule.replacement as &str).to_string();
            }
        }
        s
    }
}

// ── ChannelNameKeys ──────────────────────────────────────────────────────────

pub mod channel_name_keys {
    use super::channel_name_aliases;
    use regex::Regex;
    use std::sync::LazyLock;
    use unicode_normalization::UnicodeNormalization;

    static NORMALIZE_PREFIX_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^\s*\[?[A-Z]{2,3}\]?\s*[:\|-]\s*").unwrap());
    static COMBINING_MARKS_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{Mn}+").unwrap());
    static DECORATION_SYMBOL_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\p{So}\p{Sk}\p{Cf}\p{Co}]+").unwrap());
    static RESOLUTION_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b\d{3,4}p\b").unwrap());
    static JUNK_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bgeo[\s-]?blocked\b").unwrap());
    static LCN_PREFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*#?\d{1,4}\s*[.:\-]\s*").unwrap());
    static NORMALIZE_QUALITY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(UHD|4K|5K|6K|8K|FHD|HD|SD|720P|1080P|2160P|4320P|ULTRA)\b").unwrap()
    });
    static NORMALIZE_FLAVOR_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(HEVC|H265|X265|50FPS|60FPS|P50|P60|VIP|PREMIUM|GOLD|BACKUP|SECONDARY|RAW|DIRECT|SOURCE)\b").unwrap()
    });
    static NORMALIZE_SEPARATORS_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\[\](){}|/_\-.]+").unwrap());
    static TRAILING_FEED_TAG_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\s+(?:uk|gb|ie|us|usa|ca|au|nz)\s*$").unwrap());
    /// "Racing UK" is the channel brand, not Racing + a UK feed tag — see Kotlin twin.
    static RACING_UK_BRAND_STEM_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bracing\s*$").unwrap());
    static NON_ALPHA_NUM_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
    static WHITESPACE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

    fn strip_trailing_feed_tag_keeping_racing_uk_brand(s: &str) -> String {
        let stripped = TRAILING_FEED_TAG_REGEX.replace_all(s, "").to_string();
        if stripped == s {
            return s.to_string();
        }
        if RACING_UK_BRAND_STEM_REGEX.is_match(stripped.trim()) {
            return s.to_string();
        }
        stripped
    }

    static ONE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bone\b").unwrap());
    static TWO_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\btwo\b").unwrap());
    static THREE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bthree\b").unwrap());
    static FOUR_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bfour\b").unwrap());
    static FIVE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bfive\b").unwrap());

    static REGIONAL_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\s+(?:north\s*west|north\s*east|south\s*west|south\s*east|east\s*midlands|west\s*midlands|channel\s*islands|channel\s*isles|tyne\s*tees|northern\s*ireland|yorks(?:hire)?|lincolnshire|midlands|anglia|granada|central|meridian|border|calendar|oxford|cambridge|hull|cumbria|scotland|wales|london|north|south|east|west)\s*$").unwrap()
    });

    static PARLMNT_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bparlmnt\b").unwrap());

    /// UKTV's 2024 rebrand spelled its channels "U&Dave", "U&Gold", "U&W"… — and providers
    /// space every which way ("U & Dave", "U &Dave", "U& Dave"). Freezing each brand into
    /// one word BEFORE the timeshift and flavour passes does three jobs at once: every
    /// spacing keys identically; "U&W" survives (the bare trailing "w" is the West
    /// timeshift marker, and unguarded "U&W" keyed as "uandtshift1" — the SAME key as
    /// "U&GOLD +1", merging two different channels); and "U&GOLD" survives (GOLD sits on
    /// the flavour-strip list, and unguarded "U&GOLD" decayed to bare "uand"). Atomic
    /// ("uandgold", one word) is what makes both survivals work: no word boundary exists
    /// inside the token. Mirrors Kotlin `ChannelNameKeys.uAndBrandRegex`.
    static U_AND_BRAND_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\bu\s*&\s*(gold|dave|drama|eden|alibi|yesterday|w)\b").unwrap()
    });

    /// The spaced "+ 1" spelling of a U& timeshift ("U&DRAMA + 1"). The timeshift pass
    /// deliberately requires the "1" tight after the "+" (a spaced "+ 1" is
    /// indistinguishable from "GAA+ 1"), so this re-tokens it, scoped to the U& brands.
    /// Runs after U_AND_BRAND_REGEX so the brand is already collapsed. Mirrors Kotlin
    /// `ChannelNameKeys.uAndSpacedPlusOneRegex`.
    static U_AND_SPACED_PLUS_ONE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(uand(?:gold|dave|drama|eden|alibi|yesterday|w))\s*\+\s*1\b").unwrap()
    });

    static LANG_REGION_BRACKET_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\s*[\(\[\{](?:uk|gb|eng|english|de|french|fr|es|spanish)[\)\]\}]\s*").unwrap()
    });

    // The Kotlin timeshiftRegex uses variable-length negative lookbehind
    // `(?<!\b(?:north|south|east)\s)` which Rust's regex crate doesn't support.
    // We split it into standard regex patterns and check the lookbehind condition in code.

    /// Matches the simple timeshift markers: +1, +1hr, +1hour, "plus 1"
    static TIMESHIFT_SIMPLE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:\+1(?:\s*hr|\s*hour)?|\bplus\s*1\b)").unwrap()
    });

    /// Matches regional timeshift words (west, pst, pacific, pac, w) at end of string.
    static TIMESHIFT_REGIONAL_END_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:west|pst|pacific|pac|w)\b$").unwrap()
    });

    /// Matches regional timeshift words before a closing parenthesis.
    /// The `)` is included in the match (Rust regex has no lookahead); we trim it in code.
    static TIMESHIFT_REGIONAL_PAREN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:west|pst|pacific|pac|w)\b\s*\)").unwrap()
    });

    /// Checks if text ends with `north|south|east` + one whitespace char (the lookbehind condition).
    static REGIONAL_PREFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:north|south|east)\s$").unwrap()
    });

    /// Returns true if the text contains any timeshift marker.
    fn timeshift_is_match(text: &str) -> bool {
        if TIMESHIFT_SIMPLE_REGEX.is_match(text) {
            return true;
        }
        for re in [&*TIMESHIFT_REGIONAL_END_REGEX, &*TIMESHIFT_REGIONAL_PAREN_REGEX] {
            for mat in re.find_iter(text) {
                let prefix = &text[..mat.start()];
                if !REGIONAL_PREFIX_REGEX.is_match(prefix) {
                    return true;
                }
            }
        }
        false
    }

    /// Replaces all timeshift markers in text with replacement, respecting the
    /// north/south/east lookbehind exclusion.
    fn timeshift_replace_all(text: &str, replacement: &str) -> String {
        let mut matches: Vec<(usize, usize)> = Vec::new();
        for mat in TIMESHIFT_SIMPLE_REGEX.find_iter(text) {
            matches.push((mat.start(), mat.end()));
        }
        for re in [&*TIMESHIFT_REGIONAL_END_REGEX, &*TIMESHIFT_REGIONAL_PAREN_REGEX] {
            for mat in re.find_iter(text) {
                let prefix = &text[..mat.start()];
                if !REGIONAL_PREFIX_REGEX.is_match(prefix) {
                    // For the paren regex, the match includes trailing " )"; trim it.
                    let word = mat.as_str();
                    let word_len = word
                        .find(|c: char| c.is_whitespace() || c == ')')
                        .unwrap_or(word.len());
                    matches.push((mat.start(), mat.start() + word_len));
                }
            }
        }
        matches.sort_by_key(|&(s, _)| s);
        let mut result = String::new();
        let mut last_end = 0;
        for &(start, end) in &matches {
            if start < last_end {
                continue;
            }
            result.push_str(&text[last_end..start]);
            result.push_str(replacement);
            last_end = end;
        }
        result.push_str(&text[last_end..]);
        result
    }

    /// Mirrors `ChannelNameKeys.foldDecorations`.
    pub fn fold_decorations(name: &str) -> String {
        let nfkd: String = name.nfkd().collect();
        let stripped = COMBINING_MARKS_REGEX.replace_all(&nfkd, "");
        DECORATION_SYMBOL_REGEX.replace_all(&stripped, " ").to_string()
    }

    /// Mirrors `ChannelNameKeys.TIMESHIFT_TOKEN`: the word substituted for a timeshift
    /// marker, so "ITV +1" keys as "itvtshift1" and never collides with plain "ITV".
    /// Public because callers that compare *stripped* names (channel-merge's sweep) need
    /// to tell a +1 from its parent, and reading the flag off the already-computed
    /// normalized key is far cheaper than re-running the timeshift regex per channel.
    pub const TIMESHIFT_TOKEN: &str = "tshift1";

    /// Mirrors `ChannelNameKeys.normalizeForMatch`.
    pub fn normalize_for_match(name: &str) -> String {
        normalize_words_for_match(name).join("")
    }

    /// Mirrors `ChannelNameKeys.normalizeWordsForMatch`.
    pub fn normalize_words_for_match(name: &str) -> Vec<String> {
        let folded = fold_decorations(name).trim().to_lowercase();
        let mut s = U_AND_BRAND_REGEX
            .replace_all(&folded, "uand${1}")
            .into_owned();
        s = U_AND_SPACED_PLUS_ONE_REGEX
            .replace_all(&s, format!("${{1}} {TIMESHIFT_TOKEN}").as_str())
            .into_owned();
        let mut s = timeshift_replace_all(&s, &format!(" {TIMESHIFT_TOKEN} "));
        s = s.replace('&', " and ");
        s = NORMALIZE_PREFIX_REGEX.replace_all(&s, "").to_string();
        s = LCN_PREFIX_REGEX.replace_all(&s, "").to_string();
        s = ONE_REGEX.replace_all(&s, "1").to_string();
        s = TWO_REGEX.replace_all(&s, "2").to_string();
        s = THREE_REGEX.replace_all(&s, "3").to_string();
        s = FOUR_REGEX.replace_all(&s, "4").to_string();
        s = FIVE_REGEX.replace_all(&s, "5").to_string();
        s = NORMALIZE_QUALITY_REGEX.replace_all(&s, "").to_string();
        s = RESOLUTION_REGEX.replace_all(&s, " ").to_string();
        s = JUNK_TAG_REGEX.replace_all(&s, " ").to_string();
        s = NORMALIZE_FLAVOR_REGEX.replace_all(&s, "").to_string();
        s = NORMALIZE_SEPARATORS_REGEX.replace_all(&s, " ").to_string();
        s = WHITESPACE_REGEX.replace_all(&s, " ").to_string();
        s = s.trim().to_string();

        s = strip_trailing_feed_tag_keeping_racing_uk_brand(&s);
        s = REGIONAL_SUFFIX.replace_all(&s, "").trim().to_string();

        s = LANG_REGION_BRACKET_REGEX.replace_all(&s, " ").to_string();
        s = WHITESPACE_REGEX.replace_all(&s, " ").trim().to_string();

        s = PARLMNT_REGEX.replace_all(&s, "parliament").to_string();
        s = channel_name_aliases::expand(&s);

        s.split_whitespace()
            .map(|w| NON_ALPHA_NUM_REGEX.replace_all(w, "").to_string())
            .filter(|w| !w.is_empty())
            .collect()
    }

    /// Mirrors `ChannelNameKeys.normalizeForPlanMatch`.
    pub fn normalize_for_plan_match(name: &str) -> String {
        let folded = fold_decorations(name).trim().to_lowercase();
        let mut s = timeshift_replace_all(&folded, " ");
        s = s.replace('&', " and ");
        s = NORMALIZE_PREFIX_REGEX.replace_all(&s, "").to_string();
        s = LCN_PREFIX_REGEX.replace_all(&s, "").to_string();
        s = ONE_REGEX.replace_all(&s, "1").to_string();
        s = TWO_REGEX.replace_all(&s, "2").to_string();
        s = THREE_REGEX.replace_all(&s, "3").to_string();
        s = FOUR_REGEX.replace_all(&s, "4").to_string();
        s = FIVE_REGEX.replace_all(&s, "5").to_string();
        s = NORMALIZE_QUALITY_REGEX.replace_all(&s, "").to_string();
        s = RESOLUTION_REGEX.replace_all(&s, " ").to_string();
        s = JUNK_TAG_REGEX.replace_all(&s, " ").to_string();
        // Deliberately NO normalizeFlavorRegex (eats "GOLD" the channel) or regionalSuffix
        // (eats "Central"/"Scotland" from "Comedy Central") — see Kotlin doc comment.
        s = NORMALIZE_SEPARATORS_REGEX.replace_all(&s, " ").to_string();
        s = WHITESPACE_REGEX.replace_all(&s, " ").to_string();
        s = s.trim().to_string();

        s = strip_trailing_feed_tag_keeping_racing_uk_brand(&s);
        s = PARLMNT_REGEX.replace_all(&s, "parliament").to_string();
        s = channel_name_aliases::expand(&s);

        s.split_whitespace()
            .map(|w| NON_ALPHA_NUM_REGEX.replace_all(w, "").to_string())
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join("")
    }

    /// Mirrors `ChannelNameKeys.looseKeyMatch`.
    pub fn loose_key_match(a: &str, b: &str) -> bool {
        if a.is_empty() || b.is_empty() { return false; }
        if a == b { return true; }
        let (shorter, longer) = if a.len() <= b.len() { (a, b) } else { (b, a) };
        if shorter.len() >= 5 && longer.contains(shorter) { return true; }
        if longer.len() - shorter.len() > 1 || shorter.len() < 6 { return false; }
        let s: Vec<char> = shorter.chars().collect();
        let l: Vec<char> = longer.chars().collect();
        let mut i = 0; let mut j = 0; let mut edits = 0;
        while i < s.len() && j < l.len() {
            if s[i] == l[j] { i += 1; j += 1; continue; }
            edits += 1;
            if edits > 1 { return false; }
            if s.len() == l.len() { i += 1; j += 1; } else { j += 1; }
        }
        edits + (l.len() - j) <= 1
    }

    /// Mirrors `ChannelNameKeys.compactForMatch`.
    pub fn compact_for_match(parts: &[&str]) -> String {
        parts.iter().map(|p| normalize_for_match(p)).collect::<Vec<_>>().join("")
    }

    /// Mirrors `ChannelNameKeys.isTimeshift`.
    pub fn is_timeshift(channel_name: &str) -> bool {
        let folded = fold_decorations(channel_name);
        timeshift_is_match(&folded)
    }

    /// Mirrors `ChannelNameKeys.stripTimeshiftMarker`.
    pub fn strip_timeshift_marker(channel_name: &str) -> String {
        let folded = fold_decorations(channel_name);
        timeshift_replace_all(&folded, " ")
    }

    /// Mirrors `ChannelNameKeys.isSkySportsNews`.
    pub fn is_sky_sports_news(channel_name: &str, genre: &str) -> bool {
        let compact = compact_for_match(&[channel_name, genre]);
        compact.contains("skysportsnews") || compact.contains("skyspnews")
            || (compact.contains("skysport") && compact.contains("news"))
    }

    // ── ChannelArbiter.stripQualityAndRegional ──────────────────────────────

    static DISPLAY_PREFIX_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^\s*\[?([A-Z]{2,4})\]?\s*[:\|\-]\s*").unwrap());
    static DISPLAY_QUALITY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\s*[\[(\{]?\b(UHD|4K|5K|6K|8K|FHD|HD|SD|720P?|1080P?|2160P?|4320P?)\b[\])\}]?\s*")
            .unwrap()
    });
    // Deliberately case-sensitive (no (?i)), matching Kotlin's flavorCleanRegex --
    // harmless in practice since the input is already lowercased before this runs.
    static FLAVOR_CLEAN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(premium|gold|plus|extra|backup|secondary|vip|hd|fhd|4k|5k|6k|8k|uhd|50fps|60fps)\b")
            .unwrap()
    });
    static REGIONAL_CLEAN_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\s*(uk|gb)\s*$").unwrap());
    static BRACKETED_CONTENT_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\[\(\{].*?[\]\)\}]").unwrap());
    static SPACES_CLEAN_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

    /// Mirrors `ChannelArbiter.stripQualityAndRegional`: strip quality tags
    /// (FHD/HD/etc) and regional markers (UK/GB) for the arbiter's fuzzy-merge
    /// comparison. Kotlin's `Regex.replace(input, replacement)` replaces EVERY
    /// match, not just the first -- mirrored here with `replace_all` throughout
    /// (Rust's plain `replace()` only replaces the first match).
    pub fn strip_quality_and_regional(name: &str) -> String {
        let mut s = name.to_lowercase().trim().to_string();
        s = DISPLAY_PREFIX_REGEX.replace_all(&s, "").to_string();
        s = DISPLAY_QUALITY_REGEX.replace_all(&s, " ").to_string();
        s = FLAVOR_CLEAN_REGEX.replace_all(&s, " ").to_string();
        s = REGIONAL_CLEAN_REGEX.replace_all(&s, " ").to_string();
        s = BRACKETED_CONTENT_REGEX.replace_all(&s, " ").to_string();
        SPACES_CLEAN_REGEX.replace_all(&s, " ").trim().to_string()
    }
}

// ── JNI exports ──────────────────────────────────────────────────────────────
// (Foundation classes — will be expanded with CountryAffinity, LocaleChannelPlan,
//  ProviderGenreMapper, ChannelSortKeys, ChannelCurationKeys in subsequent edits)

fn read_jstring(env: &mut Env, s: &JString) -> Option<String> {
    env.get_string(s).ok().map(|s| s.into())
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeEngineVersion(
    mut env: EnvUnowned,
    _class: JClass,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
    ENGINE_VERSION
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeNormalizeForMatch(
    mut env: EnvUnowned,
    _class: JClass,
    input: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let s = read_jstring(env, &input).unwrap_or_default();
    let result = channel_name_keys::normalize_for_match(&s);
    env.new_string(result).unwrap_or_else(|_| env.new_string("").unwrap()).into_raw()
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeNormalizeForPlanMatch(
    mut env: EnvUnowned,
    _class: JClass,
    input: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let s = read_jstring(env, &input).unwrap_or_default();
    let result = channel_name_keys::normalize_for_plan_match(&s);
    env.new_string(result).unwrap_or_else(|_| env.new_string("").unwrap()).into_raw()
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeFoldDecorations(
    mut env: EnvUnowned,
    _class: JClass,
    input: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let s = read_jstring(env, &input).unwrap_or_default();
    let result = channel_name_keys::fold_decorations(&s);
    env.new_string(result).unwrap_or_else(|_| env.new_string("").unwrap()).into_raw()
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeIsTimeshift(
    mut env: EnvUnowned,
    _class: JClass,
    input: JString,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
    let s = read_jstring(env, &input).unwrap_or_default();
    channel_name_keys::is_timeshift(&s)
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeStripTimeshiftMarker(
    mut env: EnvUnowned,
    _class: JClass,
    input: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let s = read_jstring(env, &input).unwrap_or_default();
    let result = channel_name_keys::strip_timeshift_marker(&s);
    env.new_string(result).unwrap_or_else(|_| env.new_string("").unwrap()).into_raw()
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeLooseKeyMatch(
    mut env: EnvUnowned,
    _class: JClass,
    a: JString,
    b: JString,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
    let sa = read_jstring(env, &a).unwrap_or_default();
    let sb = read_jstring(env, &b).unwrap_or_default();
    channel_name_keys::loose_key_match(&sa, &sb)
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeIsSkySportsNews(
    mut env: EnvUnowned,
    _class: JClass,
    channel_name: JString,
    genre: JString,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
    let cn = read_jstring(env, &channel_name).unwrap_or_default();
    let g = read_jstring(env, &genre).unwrap_or_default();
    channel_name_keys::is_sky_sports_news(&cn, &g)
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeParseTitleMetadata(
    mut env: EnvUnowned,
    _class: JClass,
    input: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let s = read_jstring(env, &input).unwrap_or_default();
    let result = title_metadata::parse(&s);
    let json = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string());
    env.new_string(json).unwrap_or_else(|_| env.new_string("{}").unwrap()).into_raw()
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// ── JNI: CountryAffinity ─────────────────────────────────────────────────────

fn jstr(env: &mut Env, s: &str) -> jstring {
    env.new_string(s).unwrap_or_else(|_| env.new_string("").unwrap()).into_raw()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeDetectForChannel(
    mut env: EnvUnowned,
    _class: JClass,
    channel_name: JString,
    genre: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let cn = read_jstring(env, &channel_name).unwrap_or_default();
    let g = read_jstring(env, &genre).unwrap_or_default();
    let result = country_affinity::detect_for_channel(&cn, &g);
    jstr(env, result.map(|c| c.code).unwrap_or(""))
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeAffinityTier(
    mut env: EnvUnowned,
    _class: JClass,
    user_country_code: JString,
    channel_country_code: JString,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
    let uc = read_jstring(env, &user_country_code);
    let cc = read_jstring(env, &channel_country_code);
    let channel = cc.as_ref().and_then(|c| country_affinity::info_for_code(Some(c)));
    country_affinity::affinity_tier(uc.as_deref(), channel.as_ref())
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeCountryForText(
    mut env: EnvUnowned,
    _class: JClass,
    input: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let s = read_jstring(env, &input).unwrap_or_default();
    let result = country_affinity::country_for_text(&s);
    jstr(env, result.map(|c| c.code).unwrap_or(""))
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeDisplayName(
    mut env: EnvUnowned,
    _class: JClass,
    country_code: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let s = read_jstring(env, &country_code).unwrap_or_default();
    jstr(env, &country_affinity::display_name(&s))
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// ── JNI: ProviderGenreMapper ─────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeCategorize(
    mut env: EnvUnowned,
    _class: JClass,
    country_code: JString,
    genre: JString,
    channel_name: JString,
    user_region: JString,
    detected_region: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let cc = read_jstring(env, &country_code).filter(|s| !s.is_empty());
    let g = read_jstring(env, &genre).unwrap_or_default();
    let cn = read_jstring(env, &channel_name).unwrap_or_default();
    let ur = read_jstring(env, &user_region).filter(|s| !s.is_empty());
    let dr = read_jstring(env, &detected_region).filter(|s| !s.is_empty());
    let result = provider_genre_mapper::categorize(cc.as_deref(), &g, &cn, ur.as_deref(), dr.as_deref());
    jstr(env, result.name())
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// ── JNI: LocaleChannelPlan ───────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeRankFor(
    mut env: EnvUnowned,
    _class: JClass,
    country_code: JString,
    channel_name: JString,
    category: JString,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
    let cc = read_jstring(env, &country_code).filter(|s| !s.is_empty());
    let cn = read_jstring(env, &channel_name).unwrap_or_default();
    let cat = read_jstring(env, &category)
        .and_then(|s| ContentCategory::from_str(&s));
    locale_channel_plan::rank_for(cc.as_deref(), &cn, cat).unwrap_or(-1)
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeGuideRank(
    mut env: EnvUnowned,
    _class: JClass,
    country_code: JString,
    category: JString,
    channel_name: JString,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
    let cc = read_jstring(env, &country_code).filter(|s| !s.is_empty());
    let cat = read_jstring(env, &category)
        .and_then(|s| ContentCategory::from_str(&s));
    let cn = read_jstring(env, &channel_name).unwrap_or_default();
    locale_channel_plan::guide_rank(cc.as_deref(), cat, &cn).unwrap_or(-1)
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// ── JNI: ChannelSortKeys ─────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeDialNumberFromPlaylistOrName(
    mut env: EnvUnowned,
    _class: JClass,
    channel_name: JString,
    playlist_channel_id: JString,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
    let cn = read_jstring(env, &channel_name).unwrap_or_default();
    let pid = read_jstring(env, &playlist_channel_id).filter(|s| !s.is_empty());
    channel_sort_keys::dial_number_from_playlist_or_name(&cn, pid.as_deref()).unwrap_or(-1)
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeLcnSortKey(
    mut env: EnvUnowned,
    _class: JClass,
    channel_name: JString,
    playlist_channel_id: JString,
    country_code: JString,
) -> jint {
    env.with_env(|env| -> std::result::Result<jint, jni::errors::Error> {
        Ok({
    let cn = read_jstring(env, &channel_name).unwrap_or_default();
    let pid = read_jstring(env, &playlist_channel_id).filter(|s| !s.is_empty());
    let cc = read_jstring(env, &country_code).filter(|s| !s.is_empty());
    channel_sort_keys::lcn_sort_key(&cn, pid.as_deref(), cc.as_deref())
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeUsesNumericPrimaryOrdering(
    mut env: EnvUnowned,
    _class: JClass,
    country_code: JString,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
    let cc = read_jstring(env, &country_code).filter(|s| !s.is_empty());
    channel_sort_keys::uses_numeric_primary_ordering(cc.as_deref())
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// ── JNI: ChannelCurationKeys ─────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeFamilyKey(
    mut env: EnvUnowned,
    _class: JClass,
    input: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let s = read_jstring(env, &input).unwrap_or_default();
    jstr(env, &channel_curation_keys::family_key(&s))
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_AurelisHeuristicsNativeBridge_nativeBrandKey(
    mut env: EnvUnowned,
    _class: JClass,
    input: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
    let s = read_jstring(env, &input).unwrap_or_default();
    jstr(env, &channel_curation_keys::brand_key(&s))
})
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_decorations_strips_unicode_symbols() {
        assert_eq!(channel_name_keys::fold_decorations("BBC One Wales HD ◉"), "BBC One Wales HD  ");
        assert_eq!(channel_name_keys::fold_decorations("RTÉ One"), "RTE One");
    }

    #[test]
    fn normalize_for_match_basic() {
        assert_eq!(channel_name_keys::normalize_for_match("BBC One HD"), "bbc1");
        assert_eq!(channel_name_keys::normalize_for_match("ITV 1 FHD"), "itv1");
        assert_eq!(channel_name_keys::normalize_for_match("UK: ITV CYMRU WALES FHD"), "itv1");
        assert_eq!(channel_name_keys::normalize_for_match("ITV +1"), "itv1tshift1");
        assert_eq!(channel_name_keys::normalize_for_match("ITV1 +1"), "itv1tshift1");
        assert_eq!(channel_name_keys::normalize_for_match("5 +1"), "channel5tshift1");
        assert_eq!(channel_name_keys::normalize_for_match("Channel 5 +1"), "channel5tshift1");
        assert_eq!(channel_name_keys::normalize_for_match("Turbo"), "discoveryturbo");
        assert_eq!(channel_name_keys::normalize_for_match("Discovery Turbo"), "discoveryturbo");
        assert_eq!(channel_name_keys::normalize_for_match("UK: TURBO HD"), "discoveryturbo");
        assert_eq!(channel_name_keys::normalize_for_match("UK: DISCOVERY TURBO SD"), "discoveryturbo");
        assert_eq!(channel_name_keys::normalize_for_match("Discovery Channel HD"), "discovery");
        assert_eq!(channel_name_keys::normalize_for_match("UK: DISCOVERY"), "discovery");
        assert_eq!(channel_name_keys::normalize_for_match("Discovery Channel +1"), "discoverytshift1");
        assert_eq!(channel_name_keys::normalize_for_match("Investigation Discovery"), "investigationdiscovery");
        assert_eq!(
            channel_name_keys::normalize_for_match("Crime & Investigation"),
            channel_name_keys::normalize_for_match("Crime and Investigation HD"),
        );
        assert_eq!(channel_name_keys::normalize_for_match("[UK] BBC One"), "ukbbc1");
    }

    #[test]
    fn racing_uk_keeps_brand_uk_and_stays_apart_from_generic_racing() {
        assert_eq!(channel_name_keys::normalize_for_match("Racing UK"), "racinguk");
        assert_eq!(channel_name_keys::normalize_for_match("UK: RACING UK"), "racinguk");
        assert_eq!(channel_name_keys::normalize_for_match("Racing.UK"), "racinguk");
        // SPORT: is not a 2–3 letter country prefix, so it stays in the key.
        assert_eq!(channel_name_keys::normalize_for_match("SPORT: RACING"), "sportracing");
        assert_ne!(
            channel_name_keys::normalize_for_match("UK: RACING UK"),
            channel_name_keys::normalize_for_match("SPORT: RACING")
        );
        assert_eq!(channel_name_keys::normalize_for_plan_match("UK: RACING UK"), "racinguk");
        assert_eq!(channel_name_keys::normalize_for_match("Discovery UK"), "discovery");
    }

    #[test]
    fn u_and_brand_spellings_collapse() {
        // Every provider spacing of the UKTV rebrand names must key identically.
        assert_eq!(channel_name_keys::normalize_for_match("U&Dave"), "uanddave");
        assert_eq!(
            channel_name_keys::normalize_for_match("U&Dave"),
            channel_name_keys::normalize_for_match("U & Dave")
        );
        assert_eq!(
            channel_name_keys::normalize_for_match("U&Dave"),
            channel_name_keys::normalize_for_match("UK: U&DAVE FHD")
        );
        // U&GOLD must not decay to bare "uand" (GOLD is on the flavour-strip list).
        assert_eq!(channel_name_keys::normalize_for_match("U&Gold"), "uandgold");
        assert_eq!(
            channel_name_keys::normalize_for_match("UK: U&GOLD FHD"),
            channel_name_keys::normalize_for_match("U&Gold")
        );
        // U&W must not become "uandtshift1" (the West marker) — that key belonged to
        // U&GOLD +1, and the two channels merged.
        assert_eq!(channel_name_keys::normalize_for_match("U&W"), "uandw");
        assert_ne!(
            channel_name_keys::normalize_for_match("U&W"),
            channel_name_keys::normalize_for_match("U&GOLD +1")
        );
        // Spaced "+ 1" and tight "+1" spellings of the same timeshift converge.
        assert_eq!(
            channel_name_keys::normalize_for_match("UK: U&DRAMA + 1"),
            channel_name_keys::normalize_for_match("U&DRAMA +1")
        );
        // The West timeshift marker still works where it belongs.
        assert_eq!(channel_name_keys::normalize_for_match("HBO West"), "hbotshift1");
    }

    #[test]
    fn normalize_for_plan_match_removes_timeshift() {
        assert_eq!(channel_name_keys::normalize_for_plan_match("ITV +1"), "itv1");
        assert_eq!(channel_name_keys::normalize_for_plan_match("Comedy Central"), "comedycentral");
        assert_eq!(channel_name_keys::normalize_for_plan_match("5 +1"), "channel5");
    }

    #[test]
    fn is_timeshift_detection() {
        assert!(channel_name_keys::is_timeshift("ITV +1"));
        assert!(channel_name_keys::is_timeshift("Channel 4 +1hr"));
        assert!(!channel_name_keys::is_timeshift("BBC One"));
    }

    #[test]
    fn strip_timeshift_marker() {
        assert_eq!(channel_name_keys::strip_timeshift_marker("ITV +1"), "ITV  ");
        assert_eq!(channel_name_keys::strip_timeshift_marker("BBC One"), "BBC One");
    }

    // Twin: ChannelNameKeysTest `BT Sport and TNT Sports are one channel`.
    #[test]
    fn bt_sport_and_tnt_sports_are_one_key() {
        use channel_name_keys::{normalize_for_match as m, normalize_for_plan_match as p};
        assert_eq!(m("BT Sport 1"), "tntsports1");
        assert_eq!(m("UK: TNT Sports 1 FHD"), "tntsports1");
        assert_eq!(m("TNT Sport 2"), m("BT Sports 2"));
        assert_eq!(p("BT Sport 1"), "tntsports1");
        assert_eq!(m("BT Sport Box Office"), "tntsportsboxoffice");
    }

    // Twin: ChannelNameKeysTest `Eurosport and Prem Sp and Fball spellings fold`.
    #[test]
    fn eurosport_prem_sp_and_fball_spellings_fold() {
        use channel_name_keys::normalize_for_match as m;
        assert_eq!(m("Euro Sport 1"), m("Eurosport 1"));
        assert_eq!(m("Sky Sports Prem Sp"), m("Sky Sports Premier League"));
        assert_eq!(m("Prem Sports 1"), "premiersports1");
        assert_eq!(channel_name_aliases::expand("F’ball"), "football");
        assert_eq!(channel_name_aliases::expand("F'ball"), "football");
    }

    #[test]
    fn loose_key_match_edit_distance() {
        assert!(channel_name_keys::loose_key_match("skysportsmainevent", "skysprtsmainevent"));
        assert!(!channel_name_keys::loose_key_match("abc", "xyz"));
    }

    #[test]
    fn title_metadata_parse() {
        let m = title_metadata::parse("Movie.2024.1080p.HEVC.AAC-SWEET");
        assert_eq!(m.year, Some("2024".to_string()));
        assert_eq!(m.quality, Some("1080p".to_string()));
    }

    #[test]
    fn title_metadata_season_episode() {
        let m = title_metadata::parse("Show S01E05");
        assert_eq!(m.season_episode, Some("S01E05".to_string()));
    }

    #[test]
    fn christmas_names_and_lineup_merges() {
        use channel_name_aliases::*;
        assert_eq!(expand_with_season("UK: SKY CINEMA SELECT HD", true), "UK: sky cinema christmas HD");
        assert_eq!(expand_with_season("UK: SKY CINEMA SELECT HD", false), "UK: SKY CINEMA SELECT HD");
        assert_eq!(expand_with_season("Sky Select", true), "sky cinema christmas");
        assert_eq!(expand_with_season("UK: MOVIES 24 SD", true), "UK: christmas24 SD");
        assert_eq!(expand_with_season("Movies24 +1", true), "christmas24 +1");
        assert_eq!(expand_with_season("UK: GREAT ROMANCE +1 SD", true), "UK: great! christmas +1 SD");
        assert_eq!(expand_with_season("GREAT! Romance", true), "great! christmas");
        assert_eq!(expand_with_season("Great! Christma", false), "great! christmas");
        assert_eq!(expand_with_season("UK: SKY CINEMA PREMIER  HD", false), "UK: sky cinema premiere  HD");
        assert_eq!(expand_with_season("UK: SKY CINEMA PREMIERE HD", false), "UK: SKY CINEMA PREMIERE HD");
        assert!(christmas_season(11, 1) && christmas_season(12, 31) && christmas_season(1, 6));
        assert!(!christmas_season(1, 7) && !christmas_season(10, 31));
        assert_eq!(month_day_from_days(20_398), (11, 6)); // 2025-11-06
        assert_eq!(month_day_from_days(0), (1, 1));
    }

    #[test]
    fn channel_name_aliases_expand() {
        assert_eq!(channel_name_aliases::expand("SkySp MainEv"), "sky sports main event");
        assert_eq!(channel_name_aliases::expand("Nat Geo Wild"), "national geographic Wild");
        assert_eq!(channel_name_aliases::expand("SkySP+"), "sky sport plus");
        assert_eq!(channel_name_aliases::expand("Sky Sport+"), "sky sport plus");
        assert_eq!(
            channel_name_keys::normalize_for_match("SkySP+"),
            channel_name_keys::normalize_for_match("Sky Sport+ HD"),
        );
        assert_eq!(
            channel_name_keys::normalize_for_match("Talk"),
            channel_name_keys::normalize_for_match("Talk TV HD"),
        );
        assert_eq!(channel_name_keys::normalize_for_match("TalkTV"), "talk");
        assert_ne!(
            channel_name_keys::normalize_for_match("Talksport"),
            channel_name_keys::normalize_for_match("Talk"),
        );
        assert_eq!(
            channel_name_aliases::expand("Sky Sports NFL"),
            "sky sports action"
        );
        assert_eq!(
            channel_name_aliases::expand("SkySp NFL"),
            "sky sports action"
        );
        assert_eq!(
            channel_name_keys::normalize_for_match("UK: SKY SPORTS NFL HD"),
            channel_name_keys::normalize_for_match("UK: SKY SPORTS ACTION FHD"),
        );
        assert_eq!(
            channel_name_keys::normalize_for_match("Sky Sports NFL"),
            "skysportsaction"
        );
    }

    #[test]
    fn strip_quality_and_regional_basic() {
        assert_eq!(channel_name_keys::strip_quality_and_regional("BBC One Wales HD"), "bbc one wales");
        assert_eq!(channel_name_keys::strip_quality_and_regional("UK: BBC One"), "bbc one");
        assert_eq!(channel_name_keys::strip_quality_and_regional("Sky Sports 1 UK"), "sky sports 1");
    }

    #[test]
    fn is_sky_sports_news_detection() {
        assert!(channel_name_keys::is_sky_sports_news("Sky Sports News", "Sports"));
        assert!(!channel_name_keys::is_sky_sports_news("Sky News", "News"));
    }

    #[test]
    fn engine_version_stable() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
