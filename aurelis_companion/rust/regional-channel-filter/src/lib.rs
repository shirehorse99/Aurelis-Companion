//! Native port of `RegionalChannelFilter` (`core/common` `RegionalChannelFilter.kt`):
//! postcode -> broadcast-region lookup tables for the UK/US/Canada/Germany/
//! France/Italy/Spain/Netherlands, plus channel-name regional-tag detection
//! used to keep a viewer's "Local" EPG row limited to their own broadcast
//! region.
//!
//! This is a fully self-contained class -- every public function here is a
//! byte-for-byte port, including several latent quirks in the original
//! Kotlin (documented inline and covered by tests rather than "corrected"):
//! `regionForPostcode`'s branch order makes the Canadian- and most non-UK
//! numeric-postcode paths effectively unreachable through that entry point
//! (any letter-leading input short-circuits to the UK table; any 5-digit
//! input short-circuits to the US-ZIP table before Germany/France/Italy/
//! Spain are ever checked). `regionForCanadianPostal`/`regionForGermanPostal`/
//! etc. remain directly reachable through `isWarningLocal`, which dispatches
//! on an explicit `countryCode` instead of sniffing the postcode shape.
//!
//! Owns no long-lived state. Not wired in as the app's active filter
//! anywhere -- `RegionalChannelFilter`'s own Kotlin implementation remains
//! the only one actually selected, mirroring every other crate in this repo.

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jstring, JNI_FALSE, JNI_TRUE};
use jni::{Env, EnvUnowned};
use regex::Regex;
use std::sync::OnceLock;

/// Bump when the JNI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

fn word_regex(body: &str) -> Regex {
    Regex::new(&format!(r"(?i)(?:^|\W)(?:{body})(?:\W|$)")).unwrap()
}

/// Ordered (label, pattern) pairs -- order matters: multi-word regions are
/// listed before the bare directional words they contain (e.g. "North West"
/// before "North"), so a name like "BBC North West" resolves to "North
/// West" rather than the coarser "North".
fn region_patterns() -> &'static [(&'static str, Regex)] {
    static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            vec![
                ("North West", word_regex(r"north\s*west|\bnw\b")),
                ("North East", word_regex(r"north\s*east|\bne\b")),
                ("South West", word_regex(r"south\s*west")),
                ("South East", word_regex(r"south\s*east")),
                ("East Midlands", word_regex(r"east\s*midlands|\bemid\b")),
                ("West Midlands", word_regex(r"west\s*midlands|\bwm\b")),
                ("Channel Islands", word_regex(r"channel\s*island(?:s)?")),
                ("Tyne Tees", word_regex(r"tyne\s*tees")),
                ("Northern Ireland", word_regex(r"northern\s*ireland|utv|\bni\b")),
                ("Yorkshire", word_regex(r"yorks(?:hire)?|\byks\b")),
                ("Lincolnshire", word_regex(r"lincolnshire")),
                ("Midlands", word_regex(r"midlands")),
                ("Anglia", word_regex(r"anglia")),
                ("Granada", word_regex(r"granada")),
                ("Central", word_regex(r"central")),
                ("Meridian", word_regex(r"meridian")),
                ("Border", word_regex(r"border")),
                ("Calendar", word_regex(r"calendar")),
                ("Oxford", word_regex(r"oxford")),
                ("Cambridge", word_regex(r"cambridge")),
                ("Hull", word_regex(r"hull")),
                ("Cumbria", word_regex(r"cumbria")),
                ("Scotland", word_regex(r"scotland|alba|stv|\bsco\b|\bscot\b")),
                ("Wales", word_regex(r"wales|cymru|\bwal\b|\bs4c\b")),
                ("London", word_regex(r"london|\bldn\b|\blon\b")),
                ("North", word_regex(r"north")),
                ("South", word_regex(r"south")),
                ("East", word_regex(r"east")),
                ("West", word_regex(r"west")),
            ]
        })
        .as_slice()
}

fn region_for_uk_postcode_prefix(prefix: &str) -> Option<&'static str> {
    match prefix {
        "SW" | "SE" | "W" | "WC" | "N" | "E" | "EC" | "NW" | "HA" | "UB" | "TW" | "KT" | "CR"
        | "BR" | "DA" | "IG" | "RM" | "EN" | "SM" | "WD" => Some("London"),
        "M" | "L" | "BL" | "BB" | "PR" | "SK" | "WA" | "WN" | "CH" | "CW" | "LA" | "FY" | "OL" => {
            Some("North West")
        }
        "NE" | "SR" | "TS" | "DH" | "DL" => Some("North East"),
        "LS" | "S" | "YO" | "BD" | "HD" | "HX" | "WF" | "DN" | "HG" | "HU" => Some("Yorkshire"),
        "B" | "CV" | "DY" | "WS" | "WV" | "LE" | "DE" | "NG" | "ST" | "NN" | "HR" | "WR" | "SY"
        | "TF" => Some("Midlands"),
        "BS" | "BA" | "TA" | "EX" | "TQ" | "PL" | "TR" | "SN" | "SP" | "DT" | "GL" => {
            Some("South West")
        }
        "BN" | "CT" | "GU" | "ME" | "PO" | "RH" | "SO" | "TN" | "MK" | "OX" | "SL" | "RG" | "LU"
        | "AL" | "HP" => Some("South East"),
        "CB" | "IP" | "NR" | "PE" | "CM" | "SS" | "CO" | "SG" => Some("Anglia"),
        "G" | "EH" | "AB" | "DD" | "IV" | "KW" | "PA" | "PH" | "FK" | "KY" | "ML" | "TD" | "DG"
        | "HS" | "ZE" => Some("Scotland"),
        "CF" | "SA" | "LL" | "LD" | "NP" => Some("Wales"),
        "BT" => Some("Northern Ireland"),
        "CA" => Some("Border"),
        "JE" | "GY" => Some("Channel Islands"),
        _ => None,
    }
}

fn region_for_canadian_postal(postal: &str) -> Option<&'static str> {
    let prov = postal.chars().next()?.to_ascii_uppercase();
    Some(match prov {
        'A' => "Newfoundland and Labrador",
        'B' => "Nova Scotia",
        'C' => "Prince Edward Island",
        'E' => "New Brunswick",
        'G' | 'H' | 'J' => "Quebec",
        'K' | 'L' | 'M' | 'N' | 'P' => "Ontario",
        'R' => "Manitoba",
        'S' => "Saskatchewan",
        'T' => "Alberta",
        'V' => "British Columbia",
        'Y' => "Yukon",
        'X' => "Northwest Territories",
        'Z' => "Nunavut",
        _ => return None,
    })
}

fn region_for_german_postal(digits: &str) -> Option<&'static str> {
    let first: i32 = digits.chars().take(2).collect::<String>().parse().ok()?;
    Some(match first {
        10..=20 => "Berlin/Brandenburg",
        21..=25 => "Schleswig-Holstein/Hamburg",
        26..=27 => "Lower Saxony",
        28..=29 => "Bremen",
        30..=37 => "North Rhine-Westphalia",
        38..=39 => "Hesse",
        40..=49 => "North Rhine-Westphalia",
        50..=56 => "Rhineland-Palatinate",
        57..=59 => "North Rhine-Westphalia",
        60..=65 => "Hesse/Bavaria",
        66 => "Saarland",
        67..=69 => "Rhineland-Palatinate/Baden-Württemberg",
        70..=89 => "Baden-Württemberg",
        90..=99 => "Bavaria",
        _ => return None,
    })
}

fn region_for_french_postal(digits: &str) -> Option<&'static str> {
    let dept: i32 = digits.chars().take(2).collect::<String>().parse().ok()?;
    Some(match dept {
        75 | 76 | 77 | 78 | 91..=95 => "Île-de-France",
        13 => "Provence-Alpes-Côte d'Azur",
        69 => "Rhône-Alpes",
        59..=62 => "Hauts-de-France",
        44 => "Pays de la Loire",
        33 => "Nouvelle-Aquitaine",
        31 | 32 => "Occitanie",
        67 | 68 => "Grand Est",
        _ => "France",
    })
}

fn region_for_italian_postal(digits: &str) -> Option<&'static str> {
    let first: i32 = digits.chars().take(2).collect::<String>().parse().ok()?;
    Some(match first {
        20..=26 => "Lombardy",
        10..=12 => "Piedmont",
        16..=19 => "Liguria",
        34..=37 => "Friuli-Venezia Giulia",
        // Kotlin's `first in 38..46` then `first in 40..47` overlap 40..46;
        // the second `when` arm never wins there since `when` takes the
        // first true branch -- so 38..39 -> Veneto, 40..46 -> Veneto too
        // (first arm still matches), 47 -> Emilia-Romagna alone.
        38..=46 => "Veneto",
        47 => "Emilia-Romagna",
        50..=59 => "Tuscany",
        60..=61 => "Marche",
        65..=67 => "Abruzzo",
        70..=73 => "Apulia",
        80..=87 => "Campania",
        88..=89 => "Calabria",
        90..=92 => "Sicily",
        7 => "Lazio",
        _ => "Italy",
    })
}

fn region_for_spanish_postal(digits: &str) -> Option<&'static str> {
    let prov: i32 = digits.chars().take(2).collect::<String>().parse().ok()?;
    Some(match prov {
        1 | 2 => "Aragon",
        3 => "Asturias",
        4 => "Balearic Islands",
        5 => "Basque Country",
        6 => "Canary Islands",
        7 => "Catalonia",
        8 => "Castile and León",
        9 => "Castile-La Mancha",
        10 => "Extremadura",
        11 | 12 => "Galicia",
        13 => "Madrid",
        14 => "Murcia",
        15 => "Navarre",
        16..=19 => "Valencian Community",
        20 | 21 => "Basque Country",
        22..=26 => "Aragon",
        27..=32 => "Galicia",
        34..=39 => "Cantabria/Castile and León",
        40..=45 => "Castile and León",
        46 => "Valencian Community",
        47..=49 => "Castile and León",
        50 => "Zaragoza",
        _ => "Spain",
    })
}

fn region_for_dutch_postal(postal: &str) -> Option<&'static str> {
    let digits: String = postal.chars().filter(|c| c.is_ascii_digit()).take(4).collect();
    let n: i32 = digits.parse().ok()?;
    Some(match n {
        1000..=1999 => "Amsterdam",
        2000..=2999 => "The Hague/Rotterdam",
        3000..=3999 => "Utrecht",
        4000..=4999 => "South",
        5000..=5999 => "Limburg",
        6000..=6999 => "Arnhem/Nijmegen",
        7000..=7999 => "Gelderland",
        8000..=8999 => "Overijssel",
        9000..=9999 => "Groningen/Friesland",
        _ => "Netherlands",
    })
}

fn region_for_us_zip(zip: &str) -> Option<&'static str> {
    let zip_num: i32 = zip.chars().take(5).collect::<String>().parse().ok()?;
    Some(match zip_num {
        10001..=14999 => "New York",
        90001..=96999 => "California",
        75001..=79999 => "Texas",
        32004..=34999 => "Florida",
        15001..=19999 => "Pennsylvania",
        60001..=62999 => "Illinois",
        43001..=45999 => "Ohio",
        48001..=49999 => "Michigan",
        7001..=8999 => "New Jersey",
        27001..=28999 => "North Carolina",
        1001..=2999 => "Massachusetts",
        98001..=99999 => "Washington",
        80001..=81999 => "Colorado",
        20101..=24999 => "Virginia",
        85001..=86999 => "Arizona",
        30001..=31999 => "Georgia",
        55001..=56999 => "Minnesota",
        _ => return None,
    })
}

/// Byte-for-byte port of `regionForPostcode`, including its unreachable
/// Canadian/German/Italian/Spanish/French sub-branches (see module docs).
pub fn region_for_postcode(postcode: Option<&str>) -> Option<String> {
    let raw = postcode?.trim();
    if raw.is_empty() {
        return None;
    }

    if raw.chars().next().unwrap().is_alphabetic() {
        let upper = raw.to_uppercase();
        let prefix: String = upper.chars().take_while(|c| c.is_alphabetic()).collect();
        return region_for_uk_postcode_prefix(&prefix).map(str::to_string);
    }

    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 4 {
        if digits.len() == 5 || raw.chars().count() == 5 {
            return region_for_us_zip(&digits[..digits.len().min(5)]).map(str::to_string);
        }
        let raw_chars: Vec<char> = raw.chars().collect();
        if raw_chars.len() == 7 && raw_chars[3] == ' ' {
            return region_for_canadian_postal(raw).map(str::to_string);
        }
        if digits.len() == 5 && raw.chars().all(|c| c.is_ascii_digit() || c.is_whitespace()) {
            return region_for_german_postal(&digits).map(str::to_string);
        }
        if digits.len() >= 5 {
            return region_for_french_postal(&digits[..5]).map(str::to_string);
        }
        if digits.len() == 5
            && raw.chars().filter(|c| !c.is_whitespace()).count() == 5
        {
            return region_for_italian_postal(&digits).map(str::to_string);
        }
        if digits.len() == 5 && raw.chars().all(|c| c.is_ascii_digit() || c.is_whitespace()) {
            return region_for_spanish_postal(&digits).map(str::to_string);
        }
        if digits.len() == 4 && raw.chars().count() >= 6 {
            return region_for_dutch_postal(raw).map(str::to_string);
        }
    }

    None
}

/// Byte-for-byte port of `regionForUkPostcode`.
pub fn region_for_uk_postcode(postcode: Option<&str>) -> Option<String> {
    let raw = postcode?.trim().to_uppercase();
    if raw.is_empty() {
        return None;
    }
    let prefix: String = raw.chars().take_while(|c| c.is_alphabetic()).collect();
    region_for_uk_postcode_prefix(&prefix).map(str::to_string)
}

const US_REGIONS: &[(&str, &[&str])] = &[
    ("new york", &["new york", "nyc", "ny1", "channel 1"]),
    ("California", &["california", "los angeles", "la", "san francisco", "sf"]),
    ("Texas", &["texas", "houston", "dallas", "austin"]),
    ("Florida", &["florida", "miami", "tampa", "orlando"]),
    ("Pennsylvania", &["pennsylvania", "philadelphia", "pittsburgh"]),
    ("Illinois", &["illinois", "chicago"]),
    ("Ohio", &["ohio", "cleveland", "columbus"]),
    ("Michigan", &["michigan", "detroit"]),
    ("New Jersey", &["new jersey", "nj"]),
    ("Massachusetts", &["massachusetts", "boston"]),
    ("Washington", &["washington", "seattle"]),
    ("Colorado", &["colorado", "denver"]),
    ("Arizona", &["arizona", "phoenix"]),
    ("Georgia", &["georgia", "atlanta"]),
    ("Minnesota", &["minnesota", "minneapolis"]),
];

const EU_REGIONS: &[(&str, &[&str])] = &[
    ("Île-de-France", &["paris", "île-de-france", "idf"]),
    ("Germany", &["berlin", "hamburg", "frankfurt", "cologne"]),
    ("Lombardy", &["milan", "lombardy"]),
    ("Catalonia", &["barcelona", "catalonia", "catalan"]),
    ("Île-de-France", &["france 2", "france 3"]),
];

/// The channel name with its UK regional marker removed.
///
/// Used by the channel-merge engine, which carries the region as its own grouping key
/// component. Once it does that, leaving the region text inside the normalized name splits
/// two spellings of the SAME region apart: `BBC One Scot` normalizes to `bbc1scot` while
/// `UK: BBC ONE SCOTLAND FHD` normalizes to `bbc1` (the full word is stripped by
/// `normalize_for_match`, the abbreviation is not), so the two providers' copies of BBC One
/// Scotland never met. Removing the marker before normalizing makes both `bbc1`, and the
/// region they share is what keeps them apart from Wales.
///
/// Only the UK `region_patterns` are stripped — the US/EU keyword tables match on city and
/// state names that are frequently part of a channel's actual identity ("NY1"), so removing
/// them would merge genuinely different channels.
pub fn strip_region_in_name(channel_name: &str) -> String {
    if channel_name.trim().is_empty() {
        return channel_name.to_string();
    }
    let haystack = channel_name.to_lowercase();
    for (region, regex) in region_patterns() {
        if regex.is_match(&haystack) {
            let strip = region_name_only_pattern(region).unwrap_or(regex);
            return strip.replace_all(&haystack, " ").to_string();
        }
    }
    haystack
}

/// For `strip_region_in_name`: the words that NAME a region, without the ones that name a
/// service in it. Mirrors `RegionalChannelFilter.regionNameOnlyPatterns`. Cymru, S4C, Alba, STV
/// and UTV mark their region well enough to detect it, but inside a channel's name they are the
/// channel: stripping "cymru" as a spelling of Wales made BBC Radio Wales and BBC Radio Cymru
/// both "bbc radio" in Wales, and they merged (the aerial lineup, 11 Sep 2026).
fn region_name_only_pattern(region: &str) -> Option<&'static Regex> {
    static WALES: OnceLock<Regex> = OnceLock::new();
    static SCOTLAND: OnceLock<Regex> = OnceLock::new();
    static NORTHERN_IRELAND: OnceLock<Regex> = OnceLock::new();
    match region {
        "Wales" => Some(WALES.get_or_init(|| word_regex(r"wales|\bwal\b"))),
        "Scotland" => Some(SCOTLAND.get_or_init(|| word_regex(r"scotland|\bsco\b|\bscot\b"))),
        "Northern Ireland" => {
            Some(NORTHERN_IRELAND.get_or_init(|| word_regex(r"northern\s*ireland|\bni\b")))
        }
        _ => None,
    }
}

/// Byte-for-byte port of `detectRegionInName`.
pub fn detect_region_in_name(channel_name: &str) -> Option<String> {
    if channel_name.trim().is_empty() {
        return None;
    }
    let haystack = channel_name.to_lowercase();

    for (label, regex) in region_patterns() {
        if regex.is_match(&haystack) {
            return Some((*label).to_string());
        }
    }
    for (region, keywords) in US_REGIONS {
        if keywords.iter().any(|k| haystack.contains(k)) {
            return Some((*region).to_string());
        }
    }
    for (region, keywords) in EU_REGIONS {
        if keywords.iter().any(|k| haystack.contains(k)) {
            return Some((*region).to_string());
        }
    }
    None
}

/// Byte-for-byte port of `isForeignRegion`.
pub fn is_foreign_region(channel_name: &str, user_region: Option<&str>) -> bool {
    let Some(user_region) = user_region else { return false };
    let n = channel_name.to_lowercase();
    if !user_region.eq_ignore_ascii_case("wales") && n.contains("s4c") {
        return true;
    }
    let Some(tagged) = detect_region_in_name(channel_name) else { return false };
    !tagged.eq_ignore_ascii_case(user_region)
}

fn strip_accents(s: &str) -> String {
    s.replace('î', "i")
        .replace('ê', "e")
        .replace('é', "e")
        .replace('è', "e")
        .replace('â', "a")
        .replace('ô', "o")
        .replace('û', "u")
}

/// Byte-for-byte port of `isWarningLocal`.
pub fn is_warning_local(
    warning_region: &str,
    user_postcode: Option<&str>,
    country_code: Option<&str>,
) -> bool {
    let Some(postcode) = user_postcode.map(str::trim) else { return false };
    let Some(cc) = country_code.map(|c| c.trim().to_uppercase()) else { return false };
    if postcode.is_empty() {
        return false;
    }

    let digits: String = postcode.chars().filter(|c| c.is_ascii_digit()).collect();
    let user_region: Option<String> = match cc.as_str() {
        "US" => region_for_us_zip(&digits[..digits.len().min(5)]).map(str::to_string),
        "FR" => {
            if digits.len() >= 5 {
                region_for_french_postal(&digits[..5]).map(str::to_string)
            } else {
                None
            }
        }
        "DE" => {
            if digits.len() == 5 {
                region_for_german_postal(&digits).map(str::to_string)
            } else {
                None
            }
        }
        "ES" => {
            if digits.len() == 5 {
                region_for_spanish_postal(&digits).map(str::to_string)
            } else {
                None
            }
        }
        "IT" => {
            if digits.len() == 5 {
                region_for_italian_postal(&digits).map(str::to_string)
            } else {
                None
            }
        }
        "NL" => region_for_dutch_postal(postcode).map(str::to_string),
        "CA" => region_for_canadian_postal(postcode).map(str::to_string),
        "GB" | "UK" => region_for_uk_postcode(Some(postcode)),
        _ => region_for_postcode(Some(postcode)),
    };

    let Some(user_region) = user_region else {
        // NWS alerts for US (geocoded via coordinates) are inherently local at API level.
        return cc == "US";
    };

    let ur = user_region.trim().to_lowercase();
    let wr = warning_region.trim().to_lowercase();

    match cc.as_str() {
        "GB" | "UK" => {
            if wr.contains(&ur) || ur.contains(&wr) {
                true
            } else if ur == "london" && wr.contains("south east") {
                true
            } else if ur == "south east" && wr.contains("london") {
                true
            } else if ur == "anglia"
                && (wr.contains("east of england") || wr.contains("anglia") || wr.contains("east"))
            {
                true
            } else if ur == "scotland"
                && [
                    "scotland", "highlands", "strathclyde", "orkney", "shetland", "tayside",
                    "fife", "lothian", "borders", "grampian",
                ]
                .iter()
                .any(|k| wr.contains(k))
            {
                true
            } else if ur == "midlands" && (wr.contains("midlands") || wr.contains("east") || wr.contains("west")) {
                true
            } else if ur == "yorkshire" && (wr.contains("yorkshire") || wr.contains("humber")) {
                true
            } else {
                false
            }
        }
        "US" => true,
        _ => {
            let clean_ur = strip_accents(&ur);
            let clean_wr = strip_accents(&wr);
            if clean_wr.contains(&clean_ur) || clean_ur.contains(&clean_wr) {
                true
            } else if clean_ur.contains("ile-de-france") && (clean_wr.contains("paris") || clean_wr.contains("idf")) {
                true
            } else if clean_wr.contains("ile-de-france") && clean_ur.contains("paris") {
                true
            } else {
                let ur_parts: Vec<&str> =
                    clean_ur.split(['/', ' ', '-']).filter(|p| !p.is_empty()).collect();
                let wr_parts: Vec<&str> =
                    clean_wr.split(['/', ' ', '-']).filter(|p| !p.is_empty()).collect();
                ur_parts.iter().any(|p1| {
                    p1.len() > 2 && wr_parts.iter().any(|p2| p2.contains(p1) || p1.contains(p2))
                })
            }
        }
    }
}

// ── JNI boundary ──────────────────────────────────────────────────────────

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

fn opt(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
/// An empty string stands in for Kotlin's `null` on both sides -- this filter
/// never distinguishes "blank input" from "no region resolved" (both yield
/// `null`/`""`), so the sentinel is unambiguous.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_RegionalFilterNativeBridge_nativeRegionForPostcode<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    postcode: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &postcode);
        let result = region_for_postcode(opt(&text)).unwrap_or_default();
        env.new_string(result).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_RegionalFilterNativeBridge_nativeRegionForUkPostcode<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    postcode: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &postcode);
        let result = region_for_uk_postcode(opt(&text)).unwrap_or_default();
        env.new_string(result).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_RegionalFilterNativeBridge_nativeDetectRegionInName<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    channel_name: JString<'local>,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let text = jstring_to_string(env, &channel_name);
        let result = detect_region_in_name(&text).unwrap_or_default();
        env.new_string(result).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_RegionalFilterNativeBridge_nativeIsForeignRegion<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    channel_name: JString<'local>,
    user_region: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let name = jstring_to_string(env, &channel_name);
        let region = jstring_to_string(env, &user_region);
        if is_foreign_region(&name, opt(&region)) { JNI_TRUE } else { JNI_FALSE }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// # Safety
/// Called only by the JVM via JNI with a valid `JNIEnv`/`JClass`/`JString`.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_RegionalFilterNativeBridge_nativeIsWarningLocal<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    warning_region: JString<'local>,
    user_postcode: JString<'local>,
    country_code: JString<'local>,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let wr = jstring_to_string(env, &warning_region);
        let postcode = jstring_to_string(env, &user_postcode);
        let cc = jstring_to_string(env, &country_code);
        if is_warning_local(&wr, opt(&postcode), opt(&cc)) { JNI_TRUE } else { JNI_FALSE }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_RegionalFilterNativeBridge_nativeEngineVersion<
    'local,
>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jint {
    ENGINE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uk_postcode_prefix_maps_to_region() {
        assert_eq!(region_for_uk_postcode(Some("SA11 1AA")), Some("Wales".to_string()));
        assert_eq!(region_for_uk_postcode(Some("m1 1aa")), Some("North West".to_string()));
        assert_eq!(region_for_uk_postcode(Some("zz1 1aa")), None);
        assert_eq!(region_for_uk_postcode(None), None);
        assert_eq!(region_for_uk_postcode(Some("  ")), None);
    }

    #[test]
    fn region_for_postcode_uk_letter_leading_short_circuits() {
        assert_eq!(region_for_postcode(Some("CF10 1AA")), Some("Wales".to_string()));
    }

    #[test]
    fn region_for_postcode_five_digit_numeric_always_resolves_as_us_zip() {
        // Documented quirk: a bare 5-digit code always hits the US-ZIP branch
        // first, so e.g. a French postcode like "75001" (Paris) resolves via
        // regionForUsZip ("Texas", since 75001 falls in that range) rather
        // than regionForFrenchPostal ("Île-de-France").
        assert_eq!(region_for_postcode(Some("75001")), Some("Texas".to_string()));
        assert_eq!(region_for_postcode(Some("90210")), Some("California".to_string()));
    }

    #[test]
    fn region_for_postcode_dutch_style_is_reachable() {
        assert_eq!(region_for_postcode(Some("1012 AB")), Some("Amsterdam".to_string()));
    }

    #[test]
    fn region_for_canadian_postal_direct_call() {
        assert_eq!(region_for_canadian_postal("K1A 0B1"), Some("Ontario"));
        assert_eq!(region_for_canadian_postal("v6b 1a1"), Some("British Columbia"));
    }

    #[test]
    fn region_for_german_postal_ranges() {
        assert_eq!(region_for_german_postal("10115"), Some("Berlin/Brandenburg"));
        // Table quirk ported byte-for-byte: 70..=89 maps to Baden-Württemberg
        // even for postcodes like Munich's 80331 that are really Bavarian --
        // the Kotlin source's ranges don't split on real state borders here.
        assert_eq!(region_for_german_postal("80331"), Some("Baden-Württemberg"));
        assert_eq!(region_for_german_postal("90402"), Some("Bavaria"));
    }

    #[test]
    fn region_for_italian_postal_overlap_resolves_first_arm() {
        assert_eq!(region_for_italian_postal("40100"), Some("Veneto"));
        assert_eq!(region_for_italian_postal("47100"), Some("Emilia-Romagna"));
    }

    #[test]
    fn region_for_us_zip_ranges() {
        assert_eq!(region_for_us_zip("10001"), Some("New York"));
        assert_eq!(region_for_us_zip("99999"), Some("Washington"));
        assert_eq!(region_for_us_zip("00000"), None);
    }

    #[test]
    fn detect_region_in_name_prefers_multi_word_over_bare_direction() {
        assert_eq!(detect_region_in_name("BBC North West Today"), Some("North West".to_string()));
        assert_eq!(detect_region_in_name("BBC North Today"), Some("North".to_string()));
    }

    #[test]
    fn strip_region_keeps_names_that_are_the_service() {
        let words = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(words(strip_region_in_name("BBC Radio Wales")), "bbc radio");
        assert_eq!(words(strip_region_in_name("BBC Radio Cymru")), "bbc radio cymru");
        assert_eq!(words(strip_region_in_name("BBC ALBA")), "bbc alba");
        assert_eq!(words(strip_region_in_name("S4C HD")), "s4c hd");
        assert_eq!(words(strip_region_in_name("ITV Cymru Wales")), "itv cymru");
        assert_eq!(words(strip_region_in_name("BBC One Scotland")), "bbc one");
    }

    #[test]
    fn detect_region_in_name_wales_aliases() {
        assert_eq!(detect_region_in_name("S4C"), Some("Wales".to_string()));
        assert_eq!(detect_region_in_name("ITV Cymru Wales"), Some("Wales".to_string()));
    }

    #[test]
    fn detect_region_in_name_us_and_eu_fallbacks() {
        assert_eq!(detect_region_in_name("NYC Local News"), Some("new york".to_string()));
        assert_eq!(detect_region_in_name("France 3 Paris"), Some("Île-de-France".to_string()));
        assert_eq!(detect_region_in_name("Generic Channel"), None);
    }

    #[test]
    fn is_foreign_region_flags_s4c_outside_wales() {
        assert!(is_foreign_region("S4C", Some("Scotland")));
        assert!(!is_foreign_region("S4C", Some("Wales")));
        assert!(!is_foreign_region("Generic Channel", Some("Wales")));
        assert!(!is_foreign_region("BBC Scotland", None));
    }

    #[test]
    fn is_foreign_region_untagged_channel_passes_through() {
        assert!(!is_foreign_region("Channel 4", Some("Wales")));
    }

    #[test]
    fn is_foreign_region_mismatched_tag() {
        assert!(is_foreign_region("BBC Scotland", Some("Wales")));
        assert!(!is_foreign_region("BBC Wales Today", Some("Wales")));
    }

    #[test]
    fn is_warning_local_gb_matches_by_substring_and_special_cases() {
        assert!(is_warning_local("Wales", Some("CF10 1AA"), Some("GB")));
        assert!(is_warning_local("South East", Some("SW1A 1AA"), Some("GB")));
        assert!(!is_warning_local("Scotland", Some("CF10 1AA"), Some("GB")));
    }

    #[test]
    fn is_warning_local_us_always_true_when_region_known_or_not() {
        assert!(is_warning_local("Anything", Some("90210"), Some("US")));
        assert!(is_warning_local("Anything", Some("00000"), Some("US")));
    }

    #[test]
    fn is_warning_local_missing_inputs_are_false() {
        assert!(!is_warning_local("Wales", None, Some("GB")));
        assert!(!is_warning_local("Wales", Some("CF10 1AA"), None));
        assert!(!is_warning_local("Wales", Some(""), Some("GB")));
    }

    #[test]
    fn is_warning_local_accented_french_region() {
        assert!(is_warning_local("Île-de-France", Some("75001"), Some("FR")));
    }

    #[test]
    fn is_warning_local_fr_without_resolvable_postcode_is_false() {
        // Non-numeric postcode -> userRegion stays None -> only the US
        // special-case returns true for an unresolved region.
        assert!(!is_warning_local("Île-de-France", Some("Paris"), Some("FR")));
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
