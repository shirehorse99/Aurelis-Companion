//! Port of `CountryAffinity.kt` — broadcaster-level country metadata.
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    English, French, German, Spanish, Portuguese, Italian, Dutch,
    Nordic, EasternEuropean, Greek, Turkish, Arabic, Hindi,
    Chinese, Japanese, Korean, Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    WesternEurope, EasternEurope, NorthAmerica, LatinAmerica,
    MiddleEast, Africa, SouthAsia, EastAsia, Oceania, Other,
}

#[derive(Debug, Clone)]
pub struct CountryInfo {
    pub code: &'static str,
    pub display_name: &'static str,
    pub language: Language,
    pub region: Region,
    pub tokens: &'static [&'static str],
}

pub const TIER_SAME_COUNTRY: i32 = 0;
pub const TIER_SAME_LANGUAGE: i32 = 1;
pub const TIER_SAME_REGION: i32 = 2;
pub const TIER_REST: i32 = 3;

static PLAYLIST_PREFIX_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:\[)?([A-Za-z]{2,3})(?:\])?\s*[-|:|]\s*.+").unwrap()
});
static PLAYLIST_PREFIX_PIPE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*\|([A-Za-z]{2,3})\|\s*.+").unwrap()
});
static PLAYLIST_PREFIX_PAREN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*\(([A-Za-z]{2,3})\)\s*.+").unwrap()
});
static PLAYLIST_PREFIX_SPACE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\[)?([A-Z]{2,3})(?:\])?\s+[A-Za-z]").unwrap()
});
/// Genre/category only (`UK_SPORTS`, `US/News`). Twin of
/// `CountryAffinity.playlistPrefixLooseSepPattern`.
static PLAYLIST_PREFIX_LOOSE_SEP_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\[)?([A-Z]{2,3})(?:\])?[_/]+[A-Za-z]").unwrap()
});

static US_TIMEZONE_FEED_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\((east|west)\)").unwrap()
});

/// ISO/alias feed tag in a channel name, not glued to another letter or digit
/// (`5USA` must not become US). Longer aliases first. Twin of
/// `CountryAffinity.nameFeedTagRegex`.
static NAME_FEED_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^|[^A-Za-z0-9])(NZL|AUS|ZAF|RSA|USA|CAN|HKG|NZ|AU|ZA|US|CA|UK|GB|IE|HK|DEU|GER|DE|FRA|FR|ESP|ES|ITA|IT|NED|NL|PT|POR|AT|AUT|CH|SUI|BE|PL|SE|NO|DK|FI|IN|JPN|JP|KOR|KR|BRA|BR|MEX|MX|ARG|AR)([^A-Za-z]|$)").unwrap()
});
/// "Sci-Fi", "Sci Fi", "SciFi": the genre, never Finland. Read as "FI", it put "Sky Sci-Fi" in
/// Finland while a copy named "UK: Sky Sci-Fi" stayed British, so the arbiter kept one channel as
/// three (26 Nov 2026). Mirrors `CountryAffinity.SCI_FI` in Kotlin.
static SCI_FI_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bsci[\s\-_./]*fi\b").unwrap()
});

/// `text` with every "Sci-Fi" spelling made the one word "scifi".
pub fn without_sci_fi(text: &str) -> std::borrow::Cow<'_, str> {
    SCI_FI_REGEX.replace_all(text, "scifi")
}

static FIVE_USA_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\d\s*USA\b").unwrap()
});

/// Broadcasters that are Irish wherever a playlist files them — the twin of
/// `CountryAffinity.irishBroadcasterRegex`. A provider selling into these islands ships RTÉ,
/// TG4 and the Virgin Media channels as `UK: RTE ONE HD` inside a `UK/EIR: Regions` folder, so
/// every country signal on the row reads GB and Irish viewers find their own television in the
/// British section. Bounded by non-alphanumerics rather than `\b` so the accented `RTÉ`
/// spelling matches too.
static IRISH_BROADCASTER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(^|[^a-z0-9])(rte|rté|rte2|rté2|rtejr|rtéjr|tg4|virgin\s?media\s?(one|two|three|four|1|2|3|4))([^a-z0-9]|$)",
    )
    .unwrap()
});

/// UK broadcasters filed under a foreign group-title. Twin of `CountryAffinity.ukBroadcasterRegex`.
/// Runs after `country_from_name_hint` so `Sky Sports NZ` keeps its feed tag.
static UK_BROADCASTER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[^a-z0-9])(?:bbc|itv|itvx|channel\s*4|ch4|channel\s*5|ch5|s4c|cymru|sky\s*sports|skysp|sky\s*news|sky\s*uk|sky\s*atlantic|sky\s*cinema|tnt\s*sport|bt\s*sport|freeview|freesat|talk\s*(?:tv|sport)?|racing[\s._\-]*uk|racing[\s._\-]*tv|racinguk|racingtv|at\s*the\s*races)(?:[^a-z0-9]|$)",
    )
    .unwrap()
});

static SKY_SPORTS_PL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bsky\s*(?:sp(?:orts?)?)\s+pl\b").unwrap()
});

/// US networks whose name is the only country evidence they carry. Twin of
/// `CountryAffinity.usBroadcasterRegex`. Runs after the prefix / feed-tag checks and after
/// the UK check, so "CNN US", an explicit "CA:" prefix and "TNT Sport" all still win.
static US_BROADCASTER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[^a-z0-9])(?:oann?|one\s*american?\s*news|newsmax|msnbc|fox\s*news|fox\s*business|cnbc|c\s*-?\s*span|cbs\s*news|nbc\s*news|abc\s*news|pbs)(?:[^a-z0-9]|$)",
    )
    .unwrap()
});

fn prefix_aliases() -> &'static HashMap<&'static str, &'static str> {
    static ALIASES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        for &(k, v) in &[
            ("UK", "GB"), ("GB", "GB"), ("USA", "US"), ("UAE", "AE"), ("KSA", "SA"),
            ("GER", "DE"), ("DEU", "DE"), ("FRA", "FR"), ("ESP", "ES"), ("ITA", "IT"),
            ("POR", "PT"), ("NED", "NL"), ("HOL", "NL"), ("SUI", "CH"), ("SWI", "CH"),
            ("AUT", "AT"), ("DEN", "DK"), ("SWE", "SE"), ("NOR", "NO"), ("FIN", "FI"),
            ("GRE", "GR"), ("TUR", "TR"), ("POL", "PL"), ("RUS", "RU"), ("UKR", "UA"),
            ("CZE", "CZ"), ("ROM", "RO"), ("HUN", "HU"), ("BUL", "BG"), ("BRA", "BR"),
            ("MEX", "MX"), ("ARG", "AR"), ("COL", "CO"), ("CHI", "CL"), ("IND", "IN"),
            ("PAK", "PK"), ("JPN", "JP"), ("KOR", "KR"), ("CHN", "CN"), ("HKG", "HK"),
            ("AUS", "AU"), ("NZL", "NZ"), ("CAN", "CA"), ("ZAF", "ZA"), ("RSA", "ZA"),
            ("EGY", "EG"),
        ] {
            m.insert(k, v);
        }
        m
    });
    &*ALIASES
}

fn countries() -> &'static [CountryInfo] {
    static COUNTRIES: &[CountryInfo] = &[
        // ENGLISH - Western Europe
        CountryInfo { code: "GB", display_name: "United Kingdom", language: Language::English, region: Region::WesternEurope, tokens: &[
            "united kingdom", " uk ", "[uk]", "(uk)", "uk:", "uk|", "|uk", "|uk|",
            " gb ", "[gb]", "(gb)", "british", "england", "scotland",
            "wales", "northern ireland", "uk -", "uk |",
            " bbc", " itv", " itvx", "cymru", "channel 4", "channel4",
            " ch4 ", "channel 5", "channel5", " ch5 ", " s4c ",
            "sky news", "sky sports", "sky uk", "sky atlantic",
            "sky cinema", "sky one", "sky max", "sky witness", "sky arts",
            " u&", " dave", " eden ", " gold ", " drama ", " alibi ",
            " w ", "yesterday", "really", "more 4", "more4", "film4",
            "freeview", "freesat", "bt sport", "tnt sport"
        ]},
        CountryInfo { code: "IE", display_name: "Ireland", language: Language::English, region: Region::WesternEurope, tokens: &[
            " ireland ", " irish ", " eire ", " ie ", "[ie]", "(ie)", "ie:", "ie|", "|ie",
            " rte ", "rte 1", "rte 2", " tg4 ", "virgin media ireland"
        ]},
        // ENGLISH - North America
        CountryInfo { code: "US", display_name: "United States", language: Language::English, region: Region::NorthAmerica, tokens: &[
            "united states", " us ", "[us]", "(us)", "us:", "us|", "|us", " usa ",
            "[usa]", "(usa)", "(east)", "(west)", "american", " abc ", " cbs ", " nbc ",
            " fox ", " cw ", " espn", " cnn ", " msnbc", " hbo",
            " amc ", " tnt ", " tbs ", "usa network", "fox news",
            "fox sports", "showtime", " starz", "paramount",
            "pluto tv us", "nfl ", "nba ", "mlb ", "nhl "
        ]},
        CountryInfo { code: "CA", display_name: "Canada", language: Language::English, region: Region::NorthAmerica, tokens: &[
            " ca ", "[ca]", "(ca)", "ca:", "ca|", "|ca", " canada ", "canadian", " cbc ",
            " ctv ", "global tv", " tva ", " rds ", " tsn ", "sportsnet"
        ]},
        // ENGLISH - Oceania
        CountryInfo { code: "AU", display_name: "Australia", language: Language::English, region: Region::Oceania, tokens: &[
            "australia", "australian", " au ", "[au]", "(au)",
            "abc1 au", "nine au", "seven au", "ten au", "sbs au", "foxtel"
        ]},
        CountryInfo { code: "NZ", display_name: "New Zealand", language: Language::English, region: Region::Oceania, tokens: &[
            "new zealand", " nz ", "[nz]", "(nz)", " tvnz", "three nz"
        ]},
        // ENGLISH - Africa
        CountryInfo { code: "ZA", display_name: "South Africa", language: Language::English, region: Region::Africa, tokens: &[
            "south africa", " za ", "[za]", "(za)", "supersport", " etv ", " mnet"
        ]},
        // GERMAN-SPEAKING - Western Europe
        CountryInfo { code: "DE", display_name: "Germany", language: Language::German, region: Region::WesternEurope, tokens: &[
            " germany ", " german ", "deutsch", " de ", "[de]", "(de)",
            " ard ", " zdf ", " rtl ", "pro 7", "pro7", "sat 1",
            "sat1", "sky de", "kabel eins", "vox de", "n-tv", "ntv de"
        ]},
        CountryInfo { code: "AT", display_name: "Austria", language: Language::German, region: Region::WesternEurope, tokens: &[
            "austria", "austrian", " at ", "[at]", "(at)", " orf"
        ]},
        CountryInfo { code: "CH", display_name: "Switzerland", language: Language::German, region: Region::WesternEurope, tokens: &[
            "switzerland", "swiss", " srf", " ch ", "[ch]", "(ch)"
        ]},
        // FRENCH-SPEAKING - Western Europe
        CountryInfo { code: "FR", display_name: "France", language: Language::French, region: Region::WesternEurope, tokens: &[
            " france ", " french ", "francais", " fr ", "[fr]", "(fr)",
            " tf1", "france 2", "france 3", "france 4", "france 5",
            " m6 ", "canal+", "canal plus", " arte"
        ]},
        CountryInfo { code: "BE", display_name: "Belgium", language: Language::French, region: Region::WesternEurope, tokens: &[
            " belgium ", "belgian", " be ", "[be]", "(be)",
            " rtbf", " vrt", " vier ", " vtm "
        ]},
        // ITALIAN - Western Europe
        CountryInfo { code: "IT", display_name: "Italy", language: Language::Italian, region: Region::WesternEurope, tokens: &[
            " italy ", " italian ", " italia", " it ", "[it]", "(it)",
            " rai ", "rai 1", "rai 2", "rai 3", "mediaset", "sky italia",
            "canale 5", "tv8 it"
        ]},
        // SPANISH - Western Europe
        CountryInfo { code: "ES", display_name: "Spain", language: Language::Spanish, region: Region::WesternEurope, tokens: &[
            " spain ", "spanish", "espana", "espagne", " es ", "[es]",
            "telecinco", "antena 3", "movistar", " la 1 ", " la 2 ",
            "cuatro", " a3 ", "be mad"
        ]},
        // PORTUGUESE - Western Europe
        CountryInfo { code: "PT", display_name: "Portugal", language: Language::Portuguese, region: Region::WesternEurope, tokens: &[
            " portugal ", "portuguese", " pt ", "[pt]", "(pt)",
            " rtp ", " sic ", " tvi ", "benfica tv", "sporting tv"
        ]},
        // DUTCH - Western Europe
        CountryInfo { code: "NL", display_name: "Netherlands", language: Language::Dutch, region: Region::WesternEurope, tokens: &[
            "netherlands", " dutch ", "nederland", " nl ", "[nl]", "(nl)",
            " npo", "rtl 4", "rtl 5", "rtl 7", "rtl 8", "veronica"
        ]},
        // NORDIC - Western Europe
        CountryInfo { code: "SE", display_name: "Sweden", language: Language::Nordic, region: Region::WesternEurope, tokens: &[
            " sweden ", "swedish", "sverige", " se ", "[se]", "(se)", " svt ", " tv4 "
        ]},
        CountryInfo { code: "NO", display_name: "Norway", language: Language::Nordic, region: Region::WesternEurope, tokens: &[
            " norway ", "norwegian", " no ", "[no]", "(no)", " nrk ", "tv 2 no", "tv2 no"
        ]},
        CountryInfo { code: "DK", display_name: "Denmark", language: Language::Nordic, region: Region::WesternEurope, tokens: &[
            " denmark ", " danish ", "danmark", " dk ", "[dk]", "(dk)",
            " dr1 ", " dr2 ", "tv2 denmark", "tv 2 dk"
        ]},
        CountryInfo { code: "FI", display_name: "Finland", language: Language::Nordic, region: Region::WesternEurope, tokens: &[
            " finland ", " finnish ", " suomi", " fi ", "[fi]", "(fi)", " yle "
        ]},
        CountryInfo { code: "IS", display_name: "Iceland", language: Language::Nordic, region: Region::WesternEurope, tokens: &[
            " iceland ", " icelandic ", " ruv "
        ]},
        // EASTERN EUROPE
        CountryInfo { code: "PL", display_name: "Poland", language: Language::EasternEuropean, region: Region::EasternEurope, tokens: &[
            " poland ", " polish ", " polska ", " pl ", "[pl]", "(pl)",
            " tvp ", " polsat", " tvn ", " tvn7 "
        ]},
        CountryInfo { code: "RU", display_name: "Russia", language: Language::EasternEuropean, region: Region::EasternEurope, tokens: &[
            " russia ", " russian ", " ru ", "[ru]", "(ru)",
            " perviy", " rossiya", " ntv ", " tnt "
        ]},
        CountryInfo { code: "UA", display_name: "Ukraine", language: Language::EasternEuropean, region: Region::EasternEurope, tokens: &[
            " ukraine ", " ukrainian ", " ua ", "[ua]", "(ua)", " 1+1 ", " ictv "
        ]},
        CountryInfo { code: "CZ", display_name: "Czech Republic", language: Language::EasternEuropean, region: Region::EasternEurope, tokens: &[
            " czech ", " ceska ", " cz ", "[cz]", "(cz)", " ct1 ", " ct2 ", "nova cz"
        ]},
        CountryInfo { code: "RO", display_name: "Romania", language: Language::EasternEuropean, region: Region::EasternEurope, tokens: &[
            " romania ", " romanian ", " ro ", "[ro]", "(ro)",
            " tvr ", "pro tv", " antena 1"
        ]},
        CountryInfo { code: "HU", display_name: "Hungary", language: Language::EasternEuropean, region: Region::EasternEurope, tokens: &[
            " hungary ", " hungarian ", " magyar ", " hu ", "[hu]", "(hu)",
            " m1 hu", " m2 hu", " rtl klub"
        ]},
        CountryInfo { code: "BG", display_name: "Bulgaria", language: Language::EasternEuropean, region: Region::EasternEurope, tokens: &[
            " bulgaria ", " bulgarian ", " bnt ", " btv ", "nova bg"
        ]},
        CountryInfo { code: "GR", display_name: "Greece", language: Language::Greek, region: Region::EasternEurope, tokens: &[
            " greece ", " greek ", " ellada ", " gr ", "[gr]", "(gr)",
            " ert ", "mega gr", " ant1 ", " skai "
        ]},
        // MIDDLE EAST
        CountryInfo { code: "TR", display_name: "Turkey", language: Language::Turkish, region: Region::MiddleEast, tokens: &[
            " turkey ", " turkish ", " turkiye ", " tr ", "[tr]", "(tr)",
            " trt ", " atv ", "show tv", "kanal d", "fox tr", "star tr"
        ]},
        CountryInfo { code: "SA", display_name: "Saudi Arabia", language: Language::Arabic, region: Region::MiddleEast, tokens: &[
            " saudi ", " ksa ", " mbc ", " rotana"
        ]},
        CountryInfo { code: "AE", display_name: "United Arab Emirates", language: Language::Arabic, region: Region::MiddleEast, tokens: &[
            " dubai ", "abu dhabi", " uae ", "[uae]"
        ]},
        CountryInfo { code: "EG", display_name: "Egypt", language: Language::Arabic, region: Region::MiddleEast, tokens: &[
            " egypt ", " egyptian ", " misr "
        ]},
        // LATIN AMERICA
        CountryInfo { code: "BR", display_name: "Brazil", language: Language::Portuguese, region: Region::LatinAmerica, tokens: &[
            " brazil ", " brasil ", " brazilian ", " br ", "[br]", "(br)",
            " globo ", " sbt ", " record "
        ]},
        CountryInfo { code: "MX", display_name: "Mexico", language: Language::Spanish, region: Region::LatinAmerica, tokens: &[
            " mexico ", " mexican ", " mx ", "[mx]", "(mx)",
            "televisa", "tv azteca", " canal 5"
        ]},
        CountryInfo { code: "AR", display_name: "Argentina", language: Language::Spanish, region: Region::LatinAmerica, tokens: &[
            " argentina ", " argentine ", " ar ", "[ar]", "(ar)", " telefe ", " c5n "
        ]},
        CountryInfo { code: "CO", display_name: "Colombia", language: Language::Spanish, region: Region::LatinAmerica, tokens: &[
            " colombia ", " colombian ", "caracol tv", " rcn "
        ]},
        CountryInfo { code: "CL", display_name: "Chile", language: Language::Spanish, region: Region::LatinAmerica, tokens: &[
            " chile ", " chilean ", " cl ", "[cl]", "(cl)"
        ]},
        // SOUTH ASIA
        CountryInfo { code: "IN", display_name: "India", language: Language::Hindi, region: Region::SouthAsia, tokens: &[
            " india ", " indian ", " hindi ", " in ", "[in]", "(in)",
            "star plus", " zee ", " colors ", " sony tv", " sonyliv"
        ]},
        CountryInfo { code: "PK", display_name: "Pakistan", language: Language::Hindi, region: Region::SouthAsia, tokens: &[
            " pakistan ", " pakistani ", " pk ", "[pk]", "(pk)", " geo tv", " ary "
        ]},
        // EAST ASIA
        CountryInfo { code: "JP", display_name: "Japan", language: Language::Japanese, region: Region::EastAsia, tokens: &[
            " japan ", " japanese ", " jp ", "[jp]", "(jp)", " nhk ", "fuji tv", "tbs jp"
        ]},
        CountryInfo { code: "KR", display_name: "South Korea", language: Language::Korean, region: Region::EastAsia, tokens: &[
            " korea ", " korean ", " kr ", "[kr]", "(kr)", " kbs ", "mbc kr", "sbs kr"
        ]},
        CountryInfo { code: "CN", display_name: "China", language: Language::Chinese, region: Region::EastAsia, tokens: &[
            " china ", " chinese ", " cn ", "[cn]", "(cn)", " cctv", "phoenix tv"
        ]},
        CountryInfo { code: "HK", display_name: "Hong Kong", language: Language::Chinese, region: Region::EastAsia, tokens: &[
            "hong kong", " hk ", "[hk]", "(hk)", "hk:", "hk|", "|hk", " tvb "
        ]},
    ];
    COUNTRIES
}

fn by_code() -> &'static HashMap<&'static str, CountryInfo> {
    static BY_CODE: LazyLock<HashMap<&'static str, CountryInfo>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        for c in countries() {
            m.insert(c.code, c.clone());
        }
        m
    });
    &*BY_CODE
}

pub fn country_from_playlist_prefix(text: &str) -> Option<CountryInfo> {
    country_from_playlist_prefix_inner(text, false)
}

/// [loose] is for group-title / category strings. Twin of
/// `CountryAffinity.countryFromPlaylistPrefix(text, loose)`.
fn country_from_playlist_prefix_inner(text: &str, loose: bool) -> Option<CountryInfo> {
    let trimmed = text.trim();
    if trimmed.is_empty() { return None; }
    let raw_caps = PLAYLIST_PREFIX_REGEX.captures(trimmed)
        .or_else(|| PLAYLIST_PREFIX_PIPE_REGEX.captures(trimmed))
        .or_else(|| PLAYLIST_PREFIX_PAREN_REGEX.captures(trimmed))
        .or_else(|| PLAYLIST_PREFIX_SPACE_REGEX.captures(trimmed))
        .or_else(|| if loose {
            PLAYLIST_PREFIX_LOOSE_SEP_REGEX.captures(trimmed)
        } else {
            None
        });
    if let Some(raw_caps) = raw_caps {
        let raw = raw_caps[1].to_uppercase();
        let code = *prefix_aliases().get(raw.as_str()).unwrap_or(&raw.as_str());
        return by_code().get(code).cloned();
    }
    if !loose { return None; }
    if let Some(c) = bare_country_token(trimmed) { return Some(c); }
    display_name_prefix(trimmed)
}

fn bare_country_token(text: &str) -> Option<CountryInfo> {
    let raw = text.trim().to_uppercase();
    if raw.len() < 2 || raw.len() > 3 || !raw.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let code = *prefix_aliases().get(raw.as_str()).unwrap_or(&raw.as_str());
    by_code().get(code).cloned()
}

fn display_name_prefix(text: &str) -> Option<CountryInfo> {
    let n = text.trim();
    if n.is_empty() { return None; }
    let n_lower = n.to_lowercase();
    let mut best: Option<CountryInfo> = None;
    for info in countries() {
        let name_lower = info.display_name.to_lowercase();
        if !n_lower.starts_with(&name_lower) { continue; }
        let name_len = info.display_name.len();
        let ok = n.len() == name_len || matches!(
            n.as_bytes().get(name_len).copied(),
            Some(b' ') | Some(b'|') | Some(b'-') | Some(b':') | Some(b'_') | Some(b'/')
        );
        if ok {
            let best_len = best.as_ref().map(|b| b.display_name.len()).unwrap_or(0);
            if name_len > best_len {
                best = Some(info.clone());
            }
        }
    }
    best
}

pub fn country_for_text(text: &str) -> Option<CountryInfo> {
    if text.trim().is_empty() { return None; }
    if let Some(c) = country_from_playlist_prefix(text) { return Some(c); }
    let n = format!(" {} ", without_sci_fi(text).to_lowercase().trim());
    let mut best: Option<CountryInfo> = None;
    let mut best_score = 0;
    for info in countries() {
        for token in info.tokens {
            if n.contains(token) && token.len() > best_score {
                best = Some(info.clone());
                best_score = token.len();
            }
        }
    }
    best
}

pub fn country_for_name(text: &str) -> Option<CountryInfo> { country_for_text(text) }

pub fn country_from_name_hint(channel_name: &str) -> Option<CountryInfo> {
    if channel_name.trim().is_empty() { return None; }
    let cleaned = without_sci_fi(channel_name);
    let channel_name: &str = &cleaned;
    for caps in NAME_FEED_TAG_REGEX.captures_iter(channel_name) {
        // Leading `UK: …` / `CA| …` is a playlist prefix, not a feed tag — see Kotlin twin.
        if is_leading_playlist_country_prefix(channel_name, &caps) { continue; }
        let raw = caps[2].to_uppercase();
        // AT/BE/IN/NO are also English words — skip at string start ("At The Races" ≠ Austria).
        let code_start = caps.get(2).map(|m| m.start()).unwrap_or(usize::MAX);
        if code_start == 0 && matches!(raw.as_str(), "AT" | "BE" | "IN" | "NO") {
            continue;
        }
        if raw == "PL" && SKY_SPORTS_PL_REGEX.is_match(channel_name) {
            continue;
        }
        // `5 USA` is Channel 5's UK brand, not a United States feed tag.
        if raw != "USA" || !FIVE_USA_REGEX.is_match(channel_name) {
            let code = *prefix_aliases().get(raw.as_str()).unwrap_or(&raw.as_str());
            if let Some(c) = by_code().get(code) {
                return Some(c.clone());
            }
        }
    }
    let n = channel_name.to_lowercase();
    if n.contains("new zealand") { return by_code().get("NZ").cloned(); }
    if n.contains("south africa") { return by_code().get("ZA").cloned(); }
    if n.contains("australia") { return by_code().get("AU").cloned(); }
    None
}

fn is_leading_playlist_country_prefix(channel_name: &str, caps: &regex::Captures) -> bool {
    let Some(code) = caps.get(2) else { return false };
    if code.start() != 0 { return false; }
    let after = code.end();
    let bytes = channel_name.as_bytes();
    if after >= bytes.len() { return false; }
    matches!(bytes[after], b':' | b'|')
}

pub fn is_us_timezone_feed(name: &str) -> bool {
    US_TIMEZONE_FEED_REGEX.is_match(name)
}

/// True when `name` is one of the Irish broadcasters — see [`IRISH_BROADCASTER_REGEX`].
pub fn is_irish_broadcaster(name: &str) -> bool {
    IRISH_BROADCASTER_REGEX.is_match(name)
}

/// True when `name` is a UK broadcaster — see [`UK_BROADCASTER_REGEX`].
pub fn is_uk_broadcaster(name: &str) -> bool {
    UK_BROADCASTER_REGEX.is_match(name)
}

/// True when `name` is one of the US networks — see [`US_BROADCASTER_REGEX`].
pub fn is_us_broadcaster(name: &str) -> bool {
    US_BROADCASTER_REGEX.is_match(name)
}

pub fn detect_for_channel(channel_name: &str, genre: &str) -> Option<CountryInfo> {
    if is_us_timezone_feed(channel_name) {
        if let Some(c) = by_code().get("US") { return Some(c.clone()); }
    }
    // Same shape of override, for the same reason: the country tags on these rows are wrong
    // and the channel is not.
    if is_irish_broadcaster(channel_name) {
        if let Some(c) = by_code().get("IE") { return Some(c.clone()); }
    }
    if let Some(c) = country_from_playlist_prefix_inner(channel_name, false) { return Some(c); }
    if let Some(c) = country_from_name_hint(channel_name) { return Some(c); }
    if is_uk_broadcaster(channel_name) {
        if let Some(c) = by_code().get("GB") { return Some(c.clone()); }
    }
    // Same treatment for US networks whose name is their only country evidence.
    if is_us_broadcaster(channel_name) {
        if let Some(c) = by_code().get("US") { return Some(c.clone()); }
    }
    if let Some(c) = country_from_playlist_prefix_inner(genre, true) { return Some(c); }
    if let Some(c) = country_for_text(genre) { return Some(c); }
    if let Some(c) = country_for_text(channel_name) { return Some(c); }
    None
}

pub fn affinity_tier(user_country_code: Option<&str>, channel_country: Option<&CountryInfo>) -> i32 {
    let user_code = match user_country_code {
        Some(c) if !c.trim().is_empty() => c,
        _ => return TIER_SAME_COUNTRY,
    };
    let user = match by_code().get(user_code.to_uppercase().as_str()) {
        Some(u) => u,
        None => return TIER_REST,
    };
    let channel = match channel_country {
        Some(c) => c,
        None => return TIER_REST,
    };
    if channel.code == user.code { TIER_SAME_COUNTRY }
    else if channel.language == user.language { TIER_SAME_LANGUAGE }
    else if channel.region == user.region { TIER_SAME_REGION }
    else { TIER_REST }
}

pub fn display_name(country_code: &str) -> String {
    by_code().get(country_code).map(|c| c.display_name.to_string()).unwrap_or_else(|| country_code.to_string())
}

pub fn info_for_code(country_code: Option<&str>) -> Option<CountryInfo> {
    country_code.and_then(|c| if c.trim().is_empty() { None } else { by_code().get(c).cloned() })
}

pub fn same_language_peers(country_code: &str) -> Vec<CountryInfo> {
    let user = match by_code().get(country_code) { Some(u) => u, None => return vec![] };
    countries().iter().filter(|c| c.language == user.language && c.code != user.code).cloned().collect()
}

pub fn same_region_peers(country_code: &str) -> Vec<CountryInfo> {
    let user = match by_code().get(country_code) { Some(u) => u, None => return vec![] };
    countries().iter().filter(|c| c.region == user.region && c.code != user.code && c.language != user.language).cloned().collect()
}

pub fn catalogue() -> Vec<CountryInfo> {
    let mut sorted: Vec<CountryInfo> = countries().to_vec();
    sorted.sort_by(|a, b| a.display_name.cmp(b.display_name));
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_for_channel_uk_broadcaster_beats_foreign_genre() {
        assert_eq!(detect_for_channel("BBC One HD", "US | Entertainment").unwrap().code, "GB");
        assert_eq!(detect_for_channel("Sky Sports Main Event", "DE | Sport").unwrap().code, "GB");
    }

    #[test]
    fn detect_for_channel_uk_racing_brands_stamp_gb() {
        assert_eq!(detect_for_channel("Racing UK", "Entertainment").unwrap().code, "GB");
        assert_eq!(detect_for_channel("RacingUK", "Entertainment").unwrap().code, "GB");
        assert_eq!(detect_for_channel("Racing TV", "Entertainment").unwrap().code, "GB");
        assert_eq!(detect_for_channel("At The Races", "Entertainment").unwrap().code, "GB");
        assert!(country_from_name_hint("At The Races").is_none());
        assert_eq!(country_from_name_hint("ORF AT").unwrap().code, "AT");
    }

    #[test]
    fn detect_for_channel_uk_prefix() {
        let c = detect_for_channel("BBC One", "UK | Entertainment").unwrap();
        assert_eq!(c.code, "GB");
    }

    #[test]
    fn detect_for_channel_name_feed_tag_beats_uk_group() {
        assert_eq!(detect_for_channel("Sky Sports NZ 1", "UK | Sports").unwrap().code, "NZ");
        assert_eq!(detect_for_channel("Sky Sports AU", "UK Sports").unwrap().code, "AU");
        assert_eq!(detect_for_channel("ESPN US", "UK | Sports").unwrap().code, "US");
        assert_eq!(detect_for_channel("TSN 1 CA", "UK Sports").unwrap().code, "CA");
        assert_eq!(detect_for_channel("SuperSport ZA", "UK | Sports").unwrap().code, "ZA");
        assert_eq!(detect_for_channel("Sky Sports Main Event", "UK | Sports").unwrap().code, "GB");
    }

    #[test]
    fn country_from_name_hint_ignores_five_usa() {
        assert!(country_from_name_hint("5USA").is_none());
        assert!(country_from_name_hint("5 USA HD").is_none());
        assert_eq!(country_from_name_hint("USA Network").unwrap().code, "US");
    }

    #[test]
    fn detect_for_channel_us_timezone() {
        let c = detect_for_channel("ESPN (East)", "").unwrap();
        assert_eq!(c.code, "US");
    }

    #[test]
    fn country_from_playlist_prefix_bracket() {
        let c = country_from_playlist_prefix("[DE] Sport").unwrap();
        assert_eq!(c.code, "DE");
    }

    #[test]
    fn country_from_playlist_prefix_loose_genre_shapes() {
        assert_eq!(country_from_playlist_prefix_inner("UK Entertainment", true).unwrap().code, "GB");
        assert_eq!(country_from_playlist_prefix_inner("USA Movies", true).unwrap().code, "US");
        assert_eq!(country_from_playlist_prefix_inner("UK_SPORTS", true).unwrap().code, "GB");
        assert_eq!(country_from_playlist_prefix_inner("US/News", true).unwrap().code, "US");
        assert_eq!(country_from_playlist_prefix_inner("United States | News", true).unwrap().code, "US");
        assert_eq!(country_from_playlist_prefix_inner("UK", true).unwrap().code, "GB");
        assert!(country_from_playlist_prefix_inner("UK_SPORTS", false).is_none());
        assert!(country_from_playlist_prefix_inner("HD", true).is_none());
    }

    #[test]
    fn detect_for_channel_underscore_genre() {
        assert_eq!(detect_for_channel("BBC One HD", "UK_SPORTS").unwrap().code, "GB");
        assert_eq!(detect_for_channel("ESPN", "US | News").unwrap().code, "US");
    }

    #[test]
    fn affinity_tier_same_country() {
        let channel = info_for_code(Some("GB")).unwrap();
        assert_eq!(affinity_tier(Some("GB"), Some(&channel)), TIER_SAME_COUNTRY);
    }

    #[test]
    fn affinity_tier_same_language() {
        let channel = info_for_code(Some("US")).unwrap();
        assert_eq!(affinity_tier(Some("GB"), Some(&channel)), TIER_SAME_LANGUAGE);
    }

    #[test]
    fn affinity_tier_null_user() {
        assert_eq!(affinity_tier(None, None), TIER_SAME_COUNTRY);
    }

    #[test]
    fn catalogue_sorted() {
        let cat = catalogue();
        assert!(cat.len() > 40);
        assert_eq!(cat[0].display_name, "Argentina");
    }
}

#[cfg(test)]
mod us_broadcaster_tests {
    use super::*;

    /// The reported case: "OAN Network" in a genre of plain "News" had no country at all,
    /// while the same broadcaster listed as "US: ONE AMERICAN NEWS" resolved fine.
    #[test]
    fn oan_without_a_playlist_prefix_is_still_american() {
        assert_eq!(
            detect_for_channel("OAN Network", "News").map(|c| c.code),
            Some("US"),
        );
        assert_eq!(detect_for_channel("OANN", "News").map(|c| c.code), Some("US"));
        assert_eq!(
            detect_for_channel("One American News Network HD", "News").map(|c| c.code),
            Some("US"),
        );
    }

    #[test]
    fn other_unmistakable_us_networks() {
        for name in ["Newsmax", "MSNBC", "Fox News", "CNBC", "C-SPAN", "PBS Kids", "ABC News Live"] {
            assert_eq!(
                detect_for_channel(name, "News").map(|c| c.code),
                Some("US"),
                "{name} should be US",
            );
        }
    }

    /// Must not steal channels whose own name or prefix says otherwise.
    #[test]
    fn explicit_country_evidence_still_wins() {
        assert_eq!(detect_for_channel("CA: CBC News", "News").map(|c| c.code), Some("CA"));
        assert_eq!(detect_for_channel("TNT Sport 1", "Sport").map(|c| c.code), Some("GB"));
        assert_eq!(detect_for_channel("BBC News", "News").map(|c| c.code), Some("GB"));
    }

    /// Deliberately NOT matched: bare network letters collide with other countries.
    #[test]
    fn ambiguous_bare_letters_are_left_alone() {
        assert!(!is_us_broadcaster("ABC Australia"));
        assert!(!is_us_broadcaster("TNT Latin America"));
    }
}

#[cfg(test)]
mod sci_fi_is_not_finland {
    use super::*;

    #[test]
    fn sci_fi_in_any_spelling_names_no_country() {
        for name in ["Sky Sci-Fi", "Sky Sci-Fi HD", "Sky SciFi", "Sky Sci Fi FHD", "Sky Cinema Sci-Fi Horror", "SCI-FI Channel"] {
            assert_eq!(country_from_name_hint(name).map(|c| c.code), None, "{name}");
            assert_ne!(detect_for_channel(name, "ENTERTAINMENT").map(|c| c.code), Some("FI"), "{name}");
        }
        // A UK folder still says GB, and Finland is still Finland.
        assert_eq!(detect_for_channel("Sky Sci-Fi", "UK: Entertainment").map(|c| c.code), Some("GB"));
        assert_eq!(detect_for_channel("Yle TV1 FI", "").map(|c| c.code), Some("FI"));
        assert_eq!(detect_for_channel("MTV3 (FI)", "").map(|c| c.code), Some("FI"));
    }
}

