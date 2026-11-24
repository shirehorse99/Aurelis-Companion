//! Port of `LocaleChannelPlan` — country-specific channel ordering plans.
//!
//! Lower rank = earlier in guide. Category-specific plans override the general plan
//! when `rank_for` is called with a `ContentCategory`. All quirks preserved, not "fixed".

use super::channel_name_keys::normalize_for_plan_match;
use super::*;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Category-plan ranks live in a band above every general-dial rank so the two keyspaces
/// never collide when `guide_rank` compares across plans.
const CATEGORY_BAND: i32 = 100_000;

// ── Structs ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PlanEntry {
    pub rank: i32,
    pub tokens: Vec<&'static str>,
}

struct Match {
    rank: i32,
    token_length: usize,
}

/// Helper to reduce boilerplate when building plan entries.
fn pe(rank: i32, tokens: &[&'static str]) -> PlanEntry {
    PlanEntry {
        rank,
        tokens: tokens.to_vec(),
    }
}

static NON_ALPHA_NUM_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());

// ── Core functions ────────────────────────────────────────────────────────────

/// Uses the shared plan-match normalizer — NOT a plain lowercase/strip — so provider
/// abbreviations hit plan tokens. Falls back to naive lowercase+strip if the canonical
/// form is empty.
fn normalize(value: &str) -> String {
    let canonical = normalize_for_plan_match(value);
    if !canonical.is_empty() {
        canonical
    } else {
        NON_ALPHA_NUM_REGEX
            .replace_all(&value.to_lowercase(), "")
            .to_string()
    }
}

fn token_matches(normalized_name: &str, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if normalized_name == token {
        return true;
    }
    // Short tokens ("id", "e4") are exact-only — prefix/suffix matching at 1-2 chars
    // pulls in unrelated channels ("Ideal World" is not Investigation Discovery).
    if token.len() < 3 {
        return false;
    }
    if normalized_name.starts_with(token) {
        // "dazn1" must not claim "dazn10"; "skysports1" must not claim "skysports10".
        let token_ends_digit = token
            .chars()
            .last()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
        if token_ends_digit {
            let next = normalized_name.as_bytes().get(token.len()).copied();
            if matches!(next, Some(b'0'..=b'9')) {
                return false;
            }
        }
        return true;
    }
    if token.len() >= 4 && normalized_name.ends_with(token) {
        return true;
    }
    token.len() >= 5 && normalized_name.contains(token)
}

/// Most-specific match wins: the entry with the longest matching token, not the first
/// entry in list order. Equal-length ties resolve to the lower (earlier) rank.
fn best_match(plan: &[PlanEntry], normalized_name: &str) -> Option<Match> {
    let mut best: Option<Match> = None;
    for entry in plan {
        for token in &entry.tokens {
            if !token_matches(normalized_name, token) {
                continue;
            }
            let token_len = token.len();
            match &best {
                None => best = Some(Match {
                    rank: entry.rank,
                    token_length: token_len,
                }),
                Some(m) => {
                    if token_len > m.token_length
                        || (token_len == m.token_length && entry.rank < m.rank)
                    {
                        best = Some(Match {
                            rank: entry.rank,
                            token_length: token_len,
                        });
                    }
                }
            }
        }
    }
    best
}

pub fn rank_for(
    country_code: Option<&str>,
    channel_name: &str,
    category: Option<ContentCategory>,
) -> Option<i32> {
    let cc_raw = country_code?;
    if cc_raw.trim().is_empty() {
        return None;
    }
    let cc = cc_raw.to_uppercase();
    let n = normalize(channel_name);
    if let Some(cat) = category {
        if let Some(cat_map) = CATEGORY_PLANS.get(cc.as_str()) {
            if let Some(plan) = cat_map.get(&cat) {
                if let Some(m) = best_match(plan, &n) {
                    return Some(m.rank);
                }
            }
        }
    }
    PLANS
        .get(cc.as_str())
        .and_then(|plan| best_match(plan, &n))
        .map(|m| m.rank)
}

pub fn guide_rank(
    country_code: Option<&str>,
    category: Option<ContentCategory>,
    channel_name: &str,
) -> Option<i32> {
    let cc_raw = country_code?;
    if cc_raw.trim().is_empty() {
        return None;
    }
    let cc = cc_raw.to_uppercase();
    let n = normalize(channel_name);
    let general = PLANS
        .get(cc.as_str())
        .and_then(|plan| best_match(plan, &n));
    let cat = category.and_then(|c| {
        CATEGORY_PLANS
            .get(cc.as_str())
            .and_then(|cm| cm.get(&c))
            .and_then(|plan| best_match(plan, &n))
    });
    match (general, cat) {
        (None, None) => None,
        (Some(g), None) => Some(g.rank),
        (None, Some(c)) => Some(CATEGORY_BAND + c.rank),
        (Some(g), Some(c)) => {
            if c.token_length > g.token_length {
                Some(CATEGORY_BAND + c.rank)
            } else {
                Some(g.rank)
            }
        }
    }
}

pub fn supported_country_codes() -> Vec<String> {
    let mut keys: Vec<String> = PLANS.keys().map(|k| (*k).to_string()).collect();
    keys.sort();
    keys
}

// ── Static plan data ──────────────────────────────────────────────────────────

static PLANS: LazyLock<HashMap<&'static str, Vec<PlanEntry>>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("GB", gb_general_plan());
    m.insert("US", us_general_plan());
    m.insert("CA", ca_general_plan());
    m.insert("HK", hk_general_plan());
    m.insert("DE", de_general_plan());
    m.insert("FR", fr_general_plan());
    m.insert("IT", it_general_plan());
    m.insert("ES", es_general_plan());
    m.insert("PT", pt_general_plan());
    m.insert("NL", nl_general_plan());
    m.insert("IE", ie_general_plan());
    m.insert("AU", au_general_plan());
    m
});

static CATEGORY_PLANS: LazyLock<HashMap<&'static str, HashMap<ContentCategory, Vec<PlanEntry>>>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();

        m.insert("GB", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Local, gb_local_plan());
            cm.insert(ContentCategory::Sport, gb_sport_plan());
            cm.insert(ContentCategory::Entertainment, gb_entertainment_plan());
            cm.insert(ContentCategory::Documentaries, gb_documentaries_plan());
            cm.insert(ContentCategory::News, gb_news_plan());
            cm.insert(ContentCategory::Movies, gb_movies_plan());
            cm.insert(ContentCategory::Radio, gb_radio_plan());
            cm.insert(ContentCategory::Children, gb_kids_plan());
            cm.insert(ContentCategory::Music, gb_music_plan());
            cm.insert(ContentCategory::Shopping, gb_shopping_plan());
            cm.insert(ContentCategory::Religious, gb_religious_plan());
            cm.insert(ContentCategory::Regional, gb_regional_plan());
            cm
        });

        m.insert("US", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Local, us_local_plan());
            cm.insert(ContentCategory::Entertainment, us_entertainment_plan());
            cm.insert(ContentCategory::Movies, us_movies_plan());
            cm.insert(ContentCategory::Sport, us_sport_plan());
            cm.insert(ContentCategory::News, us_news_plan());
            cm.insert(ContentCategory::Documentaries, us_documentaries_plan());
            cm.insert(ContentCategory::Children, us_kids_plan());
            cm.insert(ContentCategory::Music, us_music_plan());
            cm.insert(ContentCategory::Shopping, us_shopping_plan());
            cm.insert(ContentCategory::Religious, us_religious_plan());
            cm.insert(ContentCategory::Regional, us_regional_plan());
            cm
        });

        m.insert("CA", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Local, ca_local_plan());
            cm.insert(ContentCategory::Entertainment, ca_entertainment_plan());
            cm.insert(ContentCategory::Movies, ca_movies_plan());
            cm.insert(ContentCategory::Sport, ca_sport_plan());
            cm.insert(ContentCategory::News, ca_news_plan());
            cm.insert(ContentCategory::Documentaries, ca_documentaries_plan());
            cm.insert(ContentCategory::Children, ca_kids_plan());
            cm.insert(ContentCategory::Music, ca_music_plan());
            cm.insert(ContentCategory::Shopping, ca_shopping_plan());
            cm.insert(ContentCategory::Religious, ca_religious_plan());
            cm.insert(ContentCategory::Regional, ca_regional_plan());
            cm
        });

        m.insert("AU", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Local, au_local_plan());
            cm.insert(ContentCategory::Sport, au_sport_plan());
            cm.insert(ContentCategory::News, au_news_plan());
            cm
        });

        m.insert("NZ", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Local, nz_local_plan());
            cm.insert(ContentCategory::Sport, nz_sport_plan());
            cm
        });

        m.insert("ZA", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Local, za_local_plan());
            cm.insert(ContentCategory::Sport, za_sport_plan());
            cm
        });

        m.insert("FR", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Local, fr_local_plan());
            cm.insert(ContentCategory::Sport, fr_sport_plan());
            cm
        });

        m.insert("DE", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Local, de_local_plan());
            cm.insert(ContentCategory::Sport, de_sport_plan());
            cm
        });

        m.insert("ES", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Local, es_local_plan());
            cm.insert(ContentCategory::Sport, es_sport_plan());
            cm
        });

        m.insert("HK", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Sport, hk_sport_plan());
            cm
        });

        m.insert("IE", {
            let mut cm = HashMap::new();
            cm.insert(ContentCategory::Local, ie_local_plan());
            cm.insert(ContentCategory::Sport, ie_sport_plan());
            cm.insert(ContentCategory::Entertainment, ie_entertainment_plan());
            cm
        });

        m
    });

// ── GB category plans ─────────────────────────────────────────────────────────

fn gb_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["skysportsmainevent", "skysportsme", "skysportmainevent"]),
        pe(2, &["skysportspremierleague", "skysportspl", "sspremierleague"]),
        pe(3, &["skysportsfootball", "skysportfootball"]),
        pe(4, &["skysportscricket", "skysportcricket"]),
        pe(5, &["skysportsgolf", "skysportgolf"]),
        pe(6, &["skysportsf1", "skysportf1"]),
        pe(7, &["skysportsaction", "skysportaction", "skysportsnfl", "skysportnfl"]),
        pe(8, &["skysportsarena", "skysportarena"]),
        pe(9, &["skysportstennis", "skysporttennis"]),
        pe(10, &["skysportsplus", "skysports+", "skysport+", "skysportplus"]),
        pe(11, &["skysportsnews", "skysportnews", "skysportsnewshq"]),
        pe(12, &["skysportsmix", "skysportmix"]),
        pe(13, &["skysportsracing", "skysportracing", "skyracing"]),
        pe(14, &["skysportsdarts", "skysportdarts"]),
        pe(15, &["skysports1", "skysport1"]),
        pe(16, &["skysports2", "skysport2"]),
        pe(17, &["skysports3", "skysport3"]),
        pe(18, &["skysports4", "skysport4"]),
        pe(19, &["skysports5", "skysport5"]),
        pe(20, &["skysports6", "skysport6"]),
        pe(21, &["skysports", "skysport"]),
        pe(30, &["tntsports1", "btsport1"]),
        pe(31, &["tntsports2", "btsport2"]),
        pe(32, &["tntsports3", "btsport3"]),
        pe(33, &["tntsports4", "btsport4"]),
        pe(34, &["tntsports5", "btsport5"]),
        pe(35, &["tntsports", "tntsport", "btsport", "btsports", "tntsportsextra"]),
        pe(40, &["premiersports1", "premsports1"]),
        pe(41, &["premiersports2", "premsports2"]),
        pe(42, &["premiersports", "premsports", "premiersport"]),
        pe(45, &["laligatv", "laliga"]),
        pe(46, &["eurosport1"]),
        pe(47, &["eurosport2"]),
        pe(48, &["eurosport"]),
        pe(50, &["viaplaysports1", "viaplaysport1"]),
        pe(51, &["viaplaysports2", "viaplaysport2"]),
        pe(55, &[
            "racinguk", "racingtv", "racingtvplus", "racingtvextra", "racingtv+",
            "attheraces", "racing",
        ]),
        pe(56, &["dazn1uk", "dazn1"]),
        pe(57, &["rallytvuk", "rallytv"]),
        pe(58, &["mutv", "manutd"]),
        pe(59, &["lfctv", "liverpoolfc"]),
        pe(60, &["mancity", "manchestercity"]),
        pe(61, &["skysportsboxoffice", "skysportsbo", "tntsportsboxoffice", "tntsportsbo", "boxoffice", "sportsppv", "ppv"]),
        pe(62, &["hbomax"]),
    ]
}

fn gb_entertainment_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["skymax", "skyone", "sky1"]),
        pe(2, &["skycomedy"]),
        pe(3, &["skyatlantic"]),
        pe(3, &["skywitness"]),
        pe(4, &["skyshowcase"]),
        pe(6, &["skysci fi", "skyscifi", "skysf"]),
        pe(7, &["skyarts"]),
        pe(8, &["gold", "uandgold"]),
        pe(9, &["wchannel", "uandw", "w"]),
        pe(10, &["alibi"]),
        pe(11, &["drama", "uanddrama"]),
        pe(12, &["itv2"]),
        pe(13, &["itv3"]),
        pe(14, &["itv4"]),
        pe(15, &["itvbe"]),
        pe(16, &["e4extra"]),
        pe(17, &["e4"]),
        pe(18, &["more4"]),
        pe(19, &["4seven"]),
        pe(20, &["dave", "uanddave", "uanddavejavu"]),
        pe(21, &["comedycentral"]),
        pe(22, &["comedycentralextra"]),
        pe(23, &["quest"]),
        pe(24, &["questred"]),
        pe(25, &["tlc", "discoverytlc"]),
        pe(26, &["5star"]),
        pe(27, &["5usa"]),
        pe(28, &["5action"]),
        pe(29, &["5select", "5drama"]),
        pe(30, &["foodnetwork"]),
        pe(31, &["challenge"]),
        pe(32, &["thatstv2", "thatstv3", "thatstv"]),
        pe(33, &["togethertv", "together"]),
        pe(35, &["greattv"]),
        pe(40, &["rte1", "rteone"]),
        pe(41, &["rte2"]),
        pe(42, &["tg4"]),
    ]
}

fn gb_kids_plan() -> Vec<PlanEntry> {
    vec![
        // The owner's order (11 Sep 2026): Sky Kids first, then Sky UK's run as before.
        pe(1, &["skykids"]),
        pe(2, &["cartoonnetwork", "cartoonnet"]),
        pe(3, &["boomerang"]),
        pe(4, &["nickelodeon"]),
        pe(5, &["nicktoons"]),
        pe(6, &["nickjr", "nickjunior"]),
        pe(7, &["cbbc"]),
        pe(8, &["cbeebies"]),
        pe(9, &["cartoonito"]),
        pe(10, &["nickjrtoo", "nickjuniortoo", "nickjr2", "nickjrpawpatrol", "pawpatrol"]),
        pe(11, &["disneyjr", "disneyjunior"]),
        pe(12, &["rtekidsjr", "rtekids", "rtejr", "rtejunior"]),
        pe(13, &["babytv"]),
        pe(23, &["ducktv"]),
    ]
}

fn gb_documentaries_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["skydocumentaries", "skydocs", "skydocmntrs"]),
        pe(2, &["skynature"]),
        pe(3, &["skyhistory", "skyhist"]),
        pe(4, &["skyhistory2", "mysteryx"]), // MysteryX: Sky History 2 renamed, Sept 2026
        pe(5, &["skycrime"]),
        pe(6, &["crimeandinvestigation", "crimeplusinvestigation", "crimeinvestigation", "crimeplusinv", "crimeinv"]),
        pe(7, &["truecrimeextra", "truecrime"]),
        pe(8, &["dmax"]),
        pe(10, &["discovery", "discoverychannel", "discoverych"]),
        pe(11, &["discoveryhistory", "discoveryhist", "dischistory"]),
        pe(12, &["discoveryscience", "discoverysci", "discscience"]),
        pe(13, &["discoveryturbo", "turbo"]),
        pe(14, &["animalplanet"]),
        pe(16, &["investigationdiscovery", "discoveryid", "id"]),
        pe(20, &["bbcearth"]),
        pe(21, &["uandyesterday", "yesterday"]),
        pe(22, &["uandeden", "eden"]),
        pe(23, &["natgeo", "nationalgeographic"]),
        pe(24, &["natgeowild", "nationalgeographicwild"]),
        pe(25, &["pbsamerica"]),
        pe(26, &["blaze"]),
        pe(27, &["smithsonian", "smithsonianchannel"]),
        pe(30, &["courttv"]),
        pe(31, &["travelxp"]),
        pe(32, &["americanheroes", "americanheroeschannel"]),
    ]
}

fn gb_news_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["skynews"]),
        pe(2, &["bbcnews"]),
        pe(3, &["gbnews"]),
        pe(4, &["talktv", "talk", "talkradio"]),
        pe(5, &["cnn", "cnninternational"]),
        pe(6, &["bloomberg"]),
        pe(7, &["cnbc"]),
        pe(8, &["euronews"]),
        pe(9, &["aljazeera"]),
        pe(10, &["france24"]),
        pe(11, &["bbcparliament"]),
        pe(12, &["lbcnews", "lbc"]),
        pe(13, &["itvnews"]),
        pe(14, &["channel4news", "ch4news"]),
        pe(15, &["channel5news", "ch5news"]),
        pe(16, &["rtenews"]),
        pe(17, &["cgtn"]),
        pe(18, &["arirang"]),
        pe(19, &["dw"]),
        pe(20, &["nhkworldjapan", "nhkworld", "nhk"]),
        pe(21, &["trtworld", "trt"]),
        // Twin of LocaleChannelPlan.gbNewsPlan — see the Kotlin side for why. This is the
        // path that actually runs: rank_for is called through nativeRankFor before the
        // Kotlin body, so a Kotlin-only edit here would change nothing on a device.
        pe(22, &["skynewsarabia"]),
    ]
}

fn gb_movies_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["skycinemapremiere", "skypremiere"]),
        pe(2, &["skycinemapopup", "skycinemapopup2", "skycinemabourne"]),
        pe(3, &["skycinemafamily", "skyfamily"]),
        pe(4, &["disneypluscinema", "disneycinema"]),
        pe(5, &["skycinemaaction", "skyaction"]),
        pe(6, &["skycinemagreats", "skygreats"]),
        pe(7, &["skycinemacomedy"]),
        pe(8, &["skycinemathriller", "skythriller"]),
        pe(9, &["skycinemadrama", "skydrama"]),
        pe(10, &["skycinemascifiandhorror", "skycinemascifihorror", "skycinemascifi", "skycinemahorror", "skyscifihor"]),
        pe(11, &["movies24plus", "movies24", "christmas24plus", "christmas24"]),
        pe(12, &["film4"]),
        pe(13, &["legend", "legendxtra"]),
        pe(14, &["greataction"]),
        pe(15, &["greatmystery"]),
        pe(16, &["greatromance", "greatchristmas"]),
        pe(17, &["talkingpicturestv", "talkingpictures"]),
        pe(30, &["skycinemaselect", "skyselect", "skycinemachristmas"]),
        pe(31, &["skycinemahits", "skyhits"]),
        pe(32, &["skycinemaanimation", "skyanimation"]),
        pe(33, &["skycinema"]),
        pe(34, &["greatmovies", "greatmoviesclassic"]),
        pe(36, &["thatsmovies", "thatsmoviesclassic", "thats60s", "thats70s", "thats80s"]),
        pe(37, &["horrorchannel"]),
        pe(38, &["tcm", "turnerclassicmovies"]),
    ]
}

fn gb_local_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["bbcone", "bbc1"]),
        pe(2, &["bbctwo", "bbc2"]),
        pe(3, &["itv1", "itv"]),
        pe(4, &["channel4", "ch4"]),
        pe(5, &["channel5", "ch5"]),
        pe(6, &["thatstv", "thatstv2", "thatstv3", "thatstvuk"]),
        pe(7, &["s4c"]),
        pe(8, &["stv"]),
        pe(9, &["utv"]),
        pe(10, &["bbcalba"]),
        pe(11, &["bbcscotland"]),
    ]
}

fn gb_music_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["mtvmusic"]),
        pe(3, &["mtvclassic"]),
        pe(4, &["mtv80s"]),
        pe(5, &["mtv00s"]),
        pe(6, &["mtvlive"]),
        pe(7, &["now70s"]),
        pe(8, &["now80s"]),
        pe(9, &["now90s"]),
        pe(10, &["now00s", "nowrock"]),
        pe(11, &["clublandtv", "clubber"]),
        pe(12, &["magictv"]),
        pe(13, &["kerrangtv", "kerrang"]),
        pe(14, &["kisstv", "kiss"]),
        pe(15, &["boxhits"]),
        pe(16, &["4music"]),
        pe(17, &["tracevault"]),
        pe(18, &["traceurban"]),
        pe(19, &["tracelatina"]),
    ]
}

fn gb_radio_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["bbcradio1", "bbcr1"]),
        pe(2, &["bbcradio1xtra", "bbc1xtra"]),
        pe(3, &["bbcradio2", "bbcr2"]),
        pe(4, &["bbcradio3", "bbcr3"]),
        pe(5, &["bbcradio4extra", "bbcr4extra"]),
        pe(6, &["bbcradio4", "bbcr4"]),
        pe(7, &["bbcradio5livesportsextra", "bbc5livesportsextra"]),
        pe(8, &["bbcradio5live", "bbc5live", "bbcradio5"]),
        pe(9, &["bbcradio6music", "bbc6music"]),
        pe(10, &["bbcasiannetwork"]),
        pe(11, &["bbcworldservice"]),
        pe(20, &["classicfm"]),
        pe(21, &["absoluteradio", "absolute"]),
        pe(22, &["capitalfm", "capital"]),
        pe(23, &["heartfm", "heart"]),
        pe(24, &["kissfm", "kiss"]),
        pe(25, &["smoothradio", "smooth"]),
        pe(26, &["magicradio", "magic"]),
        pe(27, &["talksport"]),
        pe(28, &["talkradio"]),
        pe(29, &["virginradio"]),
        pe(30, &["planetrock"]),
        pe(31, &["kerrangradio", "kerrang"]),
        pe(32, &["jazzfm"]),
        pe(33, &["greatesthitsradio"]),
        pe(34, &["radiox"]),
        pe(35, &["lbc"]),
        pe(40, &[
            "clyde1", "clyde2", "forth1", "forth2", "bordersradio", "radioborders",
            "centralfm", "northsound1", "northsound2", "westsound", "tayfm", "tay2",
            "mfr", "morayfirth", "skyeradio", "chriscountry",
        ]),
    ]
}

fn gb_shopping_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["qvc"]),
        pe(2, &["musthaveideas"]),
        pe(3, &["tjc", "thejewellerychannel"]),
        pe(4, &["qvcbeauty"]),
        pe(5, &["qvcstyle"]),
        pe(6, &["gemporia"]),
        pe(7, &["highstreettv1", "highstreettv", "hstv1", "hstv"]),
        pe(8, &["highstreettv2", "hstv2"]),
        pe(9, &["idealworld"]),
        pe(10, &["bestdirect"]),
        pe(11, &["hobbymaker"]),
        pe(12, &["qvcextra"]),
        pe(13, &["shopunlimited"]),
        pe(14, &["tvwarehouse"]),
        pe(15, &["jewellerymaker", "jewelrymaker"]),
        pe(16, &["cruise1sttv", "cruise1st"]),
        pe(30, &["gemstv"]),
        pe(31, &["hochanda"]),
        pe(32, &["createandcraft", "createcraft", "craftextra"]),
        pe(33, &["jmldirect"]),
    ]
}

fn gb_religious_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["godchannel", "godtv"]),
        pe(2, &["revelationtv", "revelation"]),
        pe(3, &["tbnuk", "tbn"]),
        pe(4, &["daystar"]),
        pe(5, &["loveworldtv", "loveworld"]),
        pe(6, &["ewtn"]),
        pe(7, &["faithworldtv", "faithworld"]),
        pe(8, &["dunamistv", "dunamis"]),
        pe(9, &["sonlifebroadcastingnetwork", "sonlifebroadcasting", "sonlife", "sbn"]),
        pe(20, &["islamchannel"]),
        pe(21, &["sikhchannel"]),
        pe(22, &["sangattv", "sangat"]),
        pe(23, &["mtainternational", "mta"]),
    ]
}

fn gb_regional_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["bbconelondon", "bbc1london"]),
        pe(2, &["bbconescotland", "bbc1scotland"]),
        pe(3, &["bbconeni", "bbc1ni", "bbconenorthernireland"]),
        pe(4, &["bbconewales", "bbc1wales"]),
        pe(5, &["bbconenortheast", "bbc1northeast", "bbconene"]),
        pe(6, &["bbconeyorkshire", "bbc1yorkshire"]),
        pe(7, &["bbconeeastmidlands", "bbc1eastmidlands"]),
        pe(8, &["bbconewestmidlands", "bbc1westmidlands"]),
        pe(9, &["bbconeeast", "bbc1east"]),
        pe(10, &["bbconesouth", "bbc1south"]),
        pe(11, &["bbconewest", "bbc1west"]),
        pe(12, &["bbconesouthwest", "bbc1southwest"]),
        pe(13, &["bbconechannelislands", "bbc1channelislands"]),
        pe(14, &["bbconenorthwest", "bbc1northwest"]),
        pe(15, &["bbconesoutheast", "bbc1southeast"]),
        pe(20, &["bbctwolondon", "bbc2london"]),
        pe(21, &["bbctwoscotland", "bbc2scotland"]),
        pe(22, &["bbctwoni", "bbc2ni", "bbctwonorthernireland"]),
        pe(23, &["bbctwowales", "bbc2wales"]),
        pe(30, &["itv1london", "itvlondon"]),
        pe(31, &["itv1granada", "itvgranada"]),
        pe(32, &["itv1meridian", "itvmeridian"]),
        pe(33, &["itv1central", "itvcentral"]),
        pe(34, &["itv1west", "itvwest"]),
        pe(35, &["itv1wales", "itvwales", "itvcymruwales"]),
        pe(36, &["itv1tynetees", "itvtynetees"]),
        pe(37, &["itv1anglia", "itvanglia"]),
        pe(38, &["itv1westcountry", "itvwestcountry"]),
        pe(39, &["itv1border", "itvborder"]),
        pe(40, &["itv1yorkshire", "itvyorkshire"]),
        pe(41, &["itv1waleswest", "itvwaleswest"]),
        pe(50, &["stv", "stvwest", "stveast", "stvnorth"]),
        pe(51, &["utv"]),
        pe(52, &["s4c"]),
    ]
}

// ── US category plans ─────────────────────────────────────────────────────────

fn us_local_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["wcbs", "cbs"]),
        pe(2, &["wnbc", "nbc"]),
        pe(3, &["wnyw", "fox"]),
        pe(4, &["wabc", "abc"]),
        pe(5, &["wwor", "mynetworktv"]),
        pe(6, &["wpix", "cw"]),
        pe(7, &["wnet", "pbs"]),
        pe(8, &["wxtv", "univision"]),
        pe(9, &["wnju", "telemundo"]),
    ]
}

fn us_entertainment_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["paramount", "paramountnetwork"]),
        pe(2, &["usa", "usanetwork"]),
        pe(3, &["syfy"]),
        pe(4, &["tnt"]),
        pe(5, &["tbs"]),
        pe(6, &["fx"]),
        pe(7, &["comedy", "comedycentral"]),
        pe(8, &["e"]),
        pe(9, &["aande", "a&e"]),
    ]
}

fn us_movies_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["hbo"]),
        pe(2, &["cinemax"]),
        pe(3, &["starz"]),
        pe(4, &["showtime"]),
        pe(5, &["lmn", "lifetimemovies"]),
        pe(6, &["amc"]),
        pe(7, &["tcm", "turnerclassicmovies"]),
        pe(8, &["fxm", "fxmovies"]),
        pe(9, &["mgm"]),
        pe(10, &["hallmarkmovies", "hallmarkmoviesmysteries"]),
    ]
}

fn us_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["espn"]),
        pe(2, &["espn2"]),
        pe(3, &["nflnetwork"]),
        pe(4, &["mlbnetwork"]),
        pe(5, &["nhlnetwork"]),
        pe(6, &["nbatv"]),
        pe(7, &["tennischannel"]),
        pe(8, &["golfchannel"]),
        pe(9, &["fs1"]),
        pe(10, &["fs2"]),
        pe(11, &["cbssports", "cbssportsnetwork"]),
        pe(12, &["secnetwork"]),
        pe(13, &["accnetwork"]),
        pe(14, &["bigtennetwork"]),
    ]
}

fn us_news_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["cnn"]),
        pe(2, &["hln"]),
        pe(3, &["bbcnews"]),
        pe(4, &["newsmax"]),
        pe(5, &["bloomberg"]),
        pe(6, &["cnbc"]),
        pe(7, &["msnbc"]),
        pe(8, &["scrippsnews"]),
        pe(9, &["foxbusiness"]),
        pe(10, &["foxnews"]),
    ]
}

fn us_documentaries_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["discovery", "discoverychannel"]),
        pe(2, &["history", "historychannel"]),
        pe(3, &["natgeo", "nationalgeographic"]),
        pe(4, &["smithsonian"]),
        pe(5, &["natgeowild"]),
        pe(6, &["investigationdiscovery", "id"]),
        pe(7, &["science"]),
        pe(8, &["animalplanet"]),
        pe(9, &["foodnetwork"]),
        pe(10, &["hgtv", "homeandgarden"]),
        pe(11, &["travelchannel"]),
        pe(12, &["tlc"]),
        pe(13, &["magnolianetwork"]),
        pe(14, &["oxygen"]),
        pe(15, &["destinationamerica"]),
        pe(16, &["cookingchannel"]),
        pe(17, &["americanheroeschannel"]),
    ]
}

fn us_kids_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["pbskids"]),
        pe(2, &["disneychannel", "disney"]),
        pe(3, &["disneyjunior"]),
        pe(4, &["universalkids"]),
        pe(5, &["cartoonnetwork"]),
        pe(6, &["boomerang"]),
        pe(7, &["nickelodeon", "nick"]),
        pe(8, &["nicktoons"]),
        pe(9, &["teennick"]),
    ]
}

fn us_music_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["greatamericanfamily"]),
        pe(2, &["cmt"]),
        pe(3, &["mtv"]),
        pe(4, &["mtv2"]),
        pe(5, &["vh1"]),
        pe(6, &["revolt"]),
        pe(7, &["nickmusic"]),
    ]
}

fn us_shopping_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["hsn"]),
        pe(2, &["jtv"]),
        pe(3, &["shophq"]),
        pe(4, &["qvc"]),
    ]
}

fn us_religious_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["hopechannel"]),
        pe(2, &["daystar"]),
        pe(3, &["ewtn"]),
        pe(4, &["catholictv"]),
        pe(5, &["tbn"]),
        pe(6, &["wordnetwork"]),
        pe(7, &["byutv"]),
    ]
}

fn us_regional_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["yesnetwork", "yes"]),
        pe(2, &["msg"]),
        pe(3, &["msgsportsnet"]),
        pe(4, &["sny"]),
    ]
}

// ── CA category plans ─────────────────────────────────────────────────────────

fn ca_local_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["cbc"]),
        pe(2, &["ctv"]),
        pe(3, &["global"]),
        pe(4, &["citytv", "city"]),
        pe(5, &["iciradiocanada", "radiocanada"]),
        pe(6, &["tva"]),
        pe(7, &["noovo"]),
        pe(8, &["omni"]),
        pe(9, &["tvo"]),
        pe(10, &["tfo"]),
    ]
}

fn ca_entertainment_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["showcase"]),
        pe(2, &["wnetwork", "wchannel"]),
        pe(3, &["ctvdrama"]),
        pe(4, &["ctvscifi"]),
        pe(5, &["ctvcomedy"]),
        pe(6, &["slice"]),
        pe(7, &["adultswim"]),
        pe(8, &["dtour"]),
        pe(9, &["oln"]),
        pe(10, &["gametv"]),
    ]
}

fn ca_movies_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["crave", "crave1", "crave2", "crave3", "crave4"]),
        pe(2, &["hbocanada", "hbo"]),
        pe(3, &["starz", "starz1", "starz2"]),
        pe(4, &["hollywoodsuite"]),
        pe(5, &["silverscreenclassics"]),
        pe(6, &["rewind"]),
        pe(7, &["lifetime"]),
        pe(8, &["ctvlife"]),
    ]
}

fn ca_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["tsn", "tsn1", "tsn2", "tsn3", "tsn4", "tsn5"]),
        pe(2, &["sportsnet", "sportsneteast", "sportsnetontario", "sportsnetwest", "sportsnetpacific"]),
        pe(3, &["sportsnetone"]),
        pe(4, &["sportsnet360"]),
        pe(5, &["rds", "rds1", "rds2", "rdsinfo"]),
        pe(6, &["tvasports", "tvasports1", "tvasports2", "tvasports3"]),
        pe(7, &["fightnetwork"]),
    ]
}

fn ca_news_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["cbcnews"]),
        pe(2, &["ctvnews"]),
        pe(3, &["cp24"]),
        pe(4, &["bnnbloomberg"]),
        pe(5, &["icirdi", "rdi"]),
        pe(6, &["lcn"]),
    ]
}

fn ca_documentaries_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["discovery", "discoverychannel"]),
        pe(2, &["history"]),
        pe(3, &["natgeo", "nationalgeographic"]),
        pe(4, &["animalplanet"]),
        pe(5, &["science"]),
        pe(6, &["smithsonian"]),
        pe(7, &["lovenature"]),
        pe(8, &["cottagelife"]),
        pe(9, &["travelandescape", "te"]),
        pe(10, &["crimeandinvestigation"]),
    ]
}

fn ca_kids_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["ytv"]),
        pe(2, &["treehouse"]),
        pe(3, &["cartoonnetwork"]),
        pe(4, &["disneychannel", "disney"]),
        pe(5, &["disneyjunior"]),
        pe(6, &["familychannel", "family"]),
        pe(7, &["familyjr"]),
        pe(8, &["wildbraintv"]),
        pe(9, &["yoopa"]),
        pe(10, &["telequebec"]),
    ]
}

fn ca_music_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["stingray"]),
        pe(2, &["cmt"]),
    ]
}

fn ca_shopping_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["tsc", "todaysshoppingchoice"]),
    ]
}

fn ca_religious_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["visiontv"]),
        pe(2, &["saltandlight"]),
        pe(3, &["daystar"]),
    ]
}

fn ca_regional_plan() -> Vec<PlanEntry> {
    vec![]
}

// ── HK category plans ─────────────────────────────────────────────────────────

fn hk_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(600, &["sportsschedulehighlights"]),
        pe(611, &["nowsports4k1"]),
        pe(612, &["nowsports4k2"]),
        pe(613, &["nowsports4k3"]),
        pe(620, &["nowsportspremierleaguetv"]),
        pe(621, &["nowsportspremierleague1"]),
        pe(622, &["nowsportspremierleague2"]),
        pe(623, &["nowsportspremierleague3"]),
        pe(624, &["nowsportspremierleague4"]),
        pe(625, &["nowsportspremierleague5"]),
        pe(626, &["nowsportspremierleague6"]),
        pe(627, &["nowsportspremierleague7"]),
        pe(630, &["nowsportsprime"]),
        pe(631, &["nowsports1"]),
        pe(632, &["nowsports2"]),
        pe(633, &["nowsports3"]),
        pe(634, &["nowsports4"]),
        pe(635, &["nowsports5"]),
        pe(636, &["nowsports6"]),
        pe(637, &["nowsports7"]),
        pe(638, &["beinsports1"]),
        pe(639, &["beinsports2"]),
        pe(640, &["mutv"]),
        pe(642, &["nbatv"]),
        pe(643, &["beinsports3"]),
        pe(644, &["beinsports4"]),
        pe(645, &["beinsports5"]),
        pe(646, &["beinsports6"]),
        pe(668, &["now668"]),
        pe(674, &["astrocricket"]),
        pe(679, &["premiersports"]),
        pe(680, &["nowsportsplus"]),
        pe(683, &["nowgolf2"]),
        pe(684, &["nowgolf3"]),
    ]
}

// ── IE category plans ─────────────────────────────────────────────────────────

fn ie_local_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["rte1", "rteone"]),
        pe(2, &["rte2"]),
        pe(3, &["rtenews"]),
        pe(4, &["tg4"]),
        pe(5, &["virginone", "virginmediaone", "virgin1"]),
        pe(6, &["virgintwo", "virginmedia2", "virgin2"]),
        pe(7, &["virginthree", "virginmedia3", "virgin3"]),
        pe(8, &["virginfour", "virginmedia4", "virgin4"]),
    ]
}

fn ie_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["premsports1", "premiersports1", "virginmedia sport"]),
        pe(2, &["premsports2", "premiersports2"]),
        pe(3, &["eurosport1"]),
        pe(4, &["eurosport2"]),
        pe(5, &["rtsport", "rte sport"]),
    ]
}

fn ie_entertainment_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["rte1", "rteone"]),
        pe(2, &["rte2"]),
        pe(3, &["virginone", "virginmediaone"]),
        pe(4, &["virgintwo", "virginmedia2"]),
        pe(5, &["virginthree", "virginmedia3"]),
        pe(6, &["virginfour", "virginmedia4"]),
        pe(7, &["tg4"]),
        pe(8, &["bbcone", "bbc1"]),
        pe(9, &["bbctwo", "bbc2"]),
        pe(10, &["itv1", "itv"]),
        pe(11, &["channel4", "ch4"]),
    ]
}

// ── AU category plans ─────────────────────────────────────────────────────────

fn au_local_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["abc", "abctv"]),
        pe(2, &["sbs"]),
        pe(3, &["seven"]),
        pe(4, &["nine"]),
        pe(5, &["ten"]),
    ]
}

fn au_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["foxsports", "foxsport"]),
        pe(2, &["espn"]),
        pe(3, &["beinsports", "beinsport"]),
        pe(4, &["stansport"]),
    ]
}

fn au_news_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["skynews"]),
        pe(2, &["abcnews"]),
        pe(3, &["cnn"]),
        pe(4, &["bbcnews"]),
    ]
}

// ── NZ category plans ─────────────────────────────────────────────────────────

fn nz_local_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["tvnz"]),
        pe(2, &["three"]),
        pe(3, &["bravo"]),
        pe(4, &["prime"]),
    ]
}

fn nz_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["skysport"]),
        pe(2, &["espn"]),
    ]
}

// ── ZA category plans ─────────────────────────────────────────────────────────

fn za_local_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["sabc"]),
        pe(2, &["etv"]),
        pe(3, &["mnet"]),
    ]
}

fn za_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["supersport"]),
        pe(2, &["espn"]),
    ]
}

// ── FR category plans ─────────────────────────────────────────────────────────

fn fr_local_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["tf1"]),
        pe(2, &["france2"]),
        pe(3, &["france3"]),
        pe(4, &["canalplus"]),
        pe(5, &["france5"]),
        pe(6, &["m6"]),
        pe(7, &["arte"]),
    ]
}

fn fr_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["canalplussport"]),
        pe(2, &["beinsports"]),
        pe(3, &["eurosport"]),
        pe(4, &["rmcsport"]),
    ]
}

// ── DE category plans ─────────────────────────────────────────────────────────

fn de_local_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["ard", "daserste"]),
        pe(2, &["zdf"]),
        pe(3, &["rtl"]),
        pe(4, &["sat1"]),
        pe(5, &["prosieben"]),
    ]
}

fn de_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["skysport"]),
        pe(2, &["dazn"]),
        pe(3, &["eurosport"]),
    ]
}

// ── ES category plans ─────────────────────────────────────────────────────────

fn es_local_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["la1"]),
        pe(2, &["la2"]),
        pe(3, &["antena3"]),
        pe(4, &["cuatro"]),
        pe(5, &["telecinco"]),
        pe(6, &["lasexta"]),
    ]
}

fn es_sport_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["movistardeportes"]),
        pe(2, &["movistarlaliga"]),
        pe(3, &["dazn"]),
    ]
}

// ── General plans (plans map) ─────────────────────────────────────────────────

fn gb_general_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["bbcone", "bbc1"]),
        pe(2, &["bbctwo", "bbc2"]),
        pe(3, &["itv1", "itv"]),
        pe(4, &["channel4", "ch4"]),
        pe(5, &["channel5", "ch5", "5"]),
        pe(6, &["thatstv", "thatstv2", "thatstv3", "thatstvuk"]),
        pe(7, &["s4c"]),
        pe(106, &["skymax"]),
        pe(107, &["skyshowcase"]),
        pe(108, &["skyatlantic"]),
        pe(109, &["skywitness"]),
        pe(110, &["skycomedy"]),
        pe(111, &["skydocumentaries"]),
        pe(112, &["skynature"]),
        pe(113, &["skyhistory"]),
        pe(114, &["skyhistory2", "mysteryx"]),
        pe(115, &["skycrime"]),
        pe(116, &["skyarts"]),
        pe(401, &["skysportsmainevent", "skysportsme"]),
        pe(402, &["skysportspremierleague", "skysportspl"]),
        pe(403, &["skysportsfootball"]),
        pe(404, &["skysportscricket"]),
        pe(405, &["skysportsgolf"]),
        pe(406, &["skysportsf1"]),
        pe(407, &["skysportsaction", "skysportaction", "skysportsnfl", "skysportnfl"]),
        pe(408, &["skysportsarena"]),
        pe(409, &["skysportsnews"]),
        pe(410, &["skysportsmix"]),
        pe(411, &["skysportsracing"]),
        pe(424, &["racinguk", "racingtv", "racingtvplus", "racingtvextra", "racingtv+"]),
        pe(425, &["dazn1uk", "dazn1"]),
        pe(426, &["rallytvuk", "rallytv"]),
        pe(501, &["skynews"]),
        pe(503, &["bbcnews"]),
    ]
}

fn us_general_plan() -> Vec<PlanEntry> {
    vec![
        // Locals
        pe(2, &["wcbs", "cbs"]),
        pe(4, &["wnbc", "nbc"]),
        pe(5, &["wnyw", "fox"]),
        pe(7, &["wabc", "abc"]),
        pe(9, &["wwor", "mynetworktv"]),
        pe(11, &["wpix", "cw"]),
        pe(13, &["wnet", "pbs"]),
        pe(41, &["wxtv", "univision"]),
        pe(47, &["wnju", "telemundo"]),
        // Entertainment
        pe(241, &["paramount", "paramountnetwork"]),
        pe(242, &["usa", "usanetwork"]),
        pe(244, &["syfy"]),
        pe(245, &["tnt"]),
        pe(247, &["tbs"]),
        pe(248, &["fx"]),
        pe(249, &["comedy", "comedycentral"]),
        pe(236, &["e"]),
        pe(265, &["aande", "a&e"]),
        // Movies
        pe(501, &["hbo"]),
        pe(515, &["cinemax"]),
        pe(525, &["starz"]),
        pe(545, &["showtime"]),
        pe(253, &["lmn", "lifetimemovies"]),
        pe(254, &["amc"]),
        pe(256, &["tcm", "turnerclassicmovies"]),
        pe(258, &["fxm", "fxmovies"]),
        pe(558, &["mgm"]),
        pe(565, &["hallmarkmovies", "hallmarkmoviesmysteries"]),
        // Sports
        pe(206, &["espn"]),
        pe(209, &["espn2"]),
        pe(212, &["nflnetwork"]),
        pe(213, &["mlbnetwork"]),
        pe(215, &["nhlnetwork"]),
        pe(216, &["nbatv"]),
        pe(217, &["tennischannel"]),
        pe(218, &["golfchannel"]),
        pe(219, &["fs1"]),
        pe(618, &["fs2"]),
        pe(221, &["cbssports", "cbssportsnetwork"]),
        pe(611, &["secnetwork"]),
        pe(612, &["accnetwork"]),
        pe(610, &["bigtennetwork"]),
        // News
        pe(202, &["cnn"]),
        pe(204, &["hln"]),
        pe(346, &["bbcnews"]),
        pe(349, &["newsmax"]),
        pe(353, &["bloomberg"]),
        pe(355, &["cnbc"]),
        pe(356, &["msnbc"]),
        pe(358, &["scrippsnews"]),
        pe(359, &["foxbusiness"]),
        pe(360, &["foxnews"]),
        // Documentaries
        pe(278, &["discovery", "discoverychannel"]),
        pe(269, &["history", "historychannel"]),
        pe(276, &["natgeo", "nationalgeographic"]),
        pe(282, &["animalplanet"]),
        pe(284, &["sciencechannel"]),
        pe(570, &["smithsonian"]),
        pe(283, &["natgeowild"]),
        pe(285, &["id", "investigationdiscovery"]),
        pe(277, &["travelchannel"]),
        // Kids
        pe(288, &["pbskids"]),
        pe(290, &["disneychannel", "disney"]),
        pe(289, &["disneyjunior"]),
        pe(295, &["universalkids"]),
        pe(296, &["cartoonnetwork"]),
        pe(298, &["boomerang"]),
        pe(299, &["nickelodeon", "nick"]),
        pe(302, &["nicktoons"]),
        pe(303, &["teennick"]),
        // Music
        pe(326, &["greatamericanfamily"]),
        pe(327, &["cmt"]),
        pe(331, &["mtv"]),
        pe(332, &["mtv2"]),
        pe(335, &["vh1"]),
        pe(338, &["revolt"]),
        pe(300, &["nickmusic"]),
        // Shopping
        pe(240, &["hsn"]),
        pe(313, &["jtv"]),
        pe(316, &["shophq"]),
        pe(317, &["qvc"]),
        // Religion
        pe(368, &["hopechannel"]),
        pe(369, &["daystar"]),
        pe(370, &["ewtn"]),
        pe(371, &["catholictv"]),
        pe(372, &["tbn"]),
        pe(373, &["wordnetwork"]),
        pe(374, &["byutv"]),
        // Regional
        pe(631, &["yesnetwork", "yes"]),
        pe(634, &["msg"]),
        pe(635, &["msgsportsnet"]),
        pe(639, &["sny"]),
    ]
}

fn ca_general_plan() -> Vec<PlanEntry> {
    vec![
        // Local
        pe(1, &["cbc"]),
        pe(2, &["ctv"]),
        pe(3, &["global"]),
        pe(4, &["citytv", "city"]),
        pe(5, &["iciradiocanada", "radiocanada"]),
        pe(6, &["tva"]),
        pe(7, &["noovo"]),
        pe(8, &["omni"]),
        pe(9, &["tvo"]),
        pe(10, &["tfo"]),
        // Entertainment
        pe(101, &["showcase"]),
        pe(102, &["wnetwork", "wchannel"]),
        pe(103, &["ctvdrama"]),
        pe(104, &["ctvscifi"]),
        pe(105, &["ctvcomedy"]),
        pe(106, &["slice"]),
        pe(107, &["adultswim"]),
        pe(108, &["dtour"]),
        pe(109, &["oln"]),
        pe(110, &["gametv"]),
        // Movies
        pe(201, &["crave", "crave1", "crave2", "crave3", "crave4"]),
        pe(202, &["hbocanada", "hbo"]),
        pe(203, &["starz", "starz1", "starz2"]),
        pe(204, &["hollywoodsuite"]),
        pe(205, &["silverscreenclassics"]),
        pe(206, &["rewind"]),
        pe(207, &["lifetime"]),
        pe(208, &["ctvlife"]),
        // Sports
        pe(301, &["tsn", "tsn1", "tsn2", "tsn3", "tsn4", "tsn5"]),
        pe(302, &["sportsnet", "sportsneteast", "sportsnetontario", "sportsnetwest", "sportsnetpacific"]),
        pe(303, &["sportsnetone"]),
        pe(304, &["sportsnet360"]),
        pe(305, &["rds", "rds1", "rds2", "rdsinfo"]),
        pe(306, &["tvasports", "tvasports1", "tvasports2", "tvasports3"]),
        pe(307, &["fightnetwork"]),
        // News
        pe(401, &["cbcnews"]),
        pe(402, &["ctvnews"]),
        pe(403, &["cp24"]),
        pe(404, &["bnnbloomberg"]),
        pe(405, &["icirdi", "rdi"]),
        pe(406, &["lcn"]),
        // Documentaries
        pe(501, &["discovery", "discoverychannel"]),
        pe(502, &["history"]),
        pe(503, &["natgeo", "nationalgeographic"]),
        pe(504, &["animalplanet"]),
        pe(505, &["science"]),
        pe(506, &["smithsonian"]),
        pe(507, &["lovenature"]),
        pe(508, &["cottagelife"]),
        pe(509, &["travelandescape", "te"]),
        pe(510, &["crimeandinvestigation"]),
        // Kids
        pe(601, &["ytv"]),
        pe(602, &["treehouse"]),
        pe(603, &["cartoonnetwork"]),
        pe(604, &["disneychannel", "disney"]),
        pe(605, &["disneyjunior"]),
        pe(606, &["familychannel", "family"]),
        pe(607, &["familyjr"]),
        pe(608, &["wildbraintv"]),
        pe(609, &["yoopa"]),
        pe(610, &["telequebec"]),
        // Music
        pe(701, &["stingray"]),
        pe(702, &["cmt"]),
        // Shopping
        pe(801, &["tsc", "todaysshoppingchoice"]),
        // Religion
        pe(901, &["visiontv"]),
        pe(902, &["saltandlight"]),
        pe(903, &["daystar"]),
    ]
}

fn hk_general_plan() -> Vec<PlanEntry> {
    vec![
        // Domestic Free
        pe(81, &["jade"]),
        pe(82, &["tvbplus"]),
        pe(83, &["tvbnews"]),
        pe(84, &["pearl"]),
        pe(85, &["phoenixhongkong"]),
        pe(96, &["viutvsix"]),
        pe(99, &["viutv"]),
        pe(76, &["hoyinternational"]),
        pe(77, &["hoytv"]),
        pe(78, &["hoyinfotainment"]),
        // Domestic Pay (Now TV) Highlights
        pe(102, &["viuchannel"]),
        pe(105, &["nowchinesedrama"]),
        pe(111, &["hbohits"]),
        pe(112, &["hbofamily"]),
        pe(113, &["cinemax"]),
        pe(114, &["hbosignature"]),
        pe(115, &["hbo"]),
        pe(116, &["moviemovie"]),
        pe(119, &["hitsmovies"]),
        pe(155, &["tvn"]),
        pe(156, &["kbsworld"]),
        pe(208, &["discoveryasia"]),
        pe(209, &["discoverychannel"]),
        pe(210, &["animalplanet"]),
        pe(316, &["cnninternational", "cnn"]),
        pe(319, &["cnbc"]),
        pe(320, &["bbcnews"]),
        pe(321, &["bloomberg"]),
        pe(332, &["nownews"]),
        pe(333, &["nowbusiness"]),
        pe(512, &["axn"]),
        pe(513, &["hits"]),
        pe(548, &["phoenixchinese"]),
    ]
}

fn de_general_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["ard", "daserste"]),
        pe(2, &["zdf"]),
        pe(3, &["rtl"]),
        pe(100, &["skyonede", "skyone"]),
        pe(101, &["skyshowcasede", "skyshowcase"]),
        pe(102, &["skyreals", "skyreal"]),
        pe(110, &["skycinemapremierede", "skycinema"]),
        pe(111, &["skycinemaaction"]),
        pe(112, &["skycinemafamily"]),
        pe(200, &["skysportnewshd", "skysportnews"]),
        pe(201, &["skysporttopevent"]),
        pe(202, &["skysportbundesliga"]),
        pe(203, &["skysportf1"]),
        pe(204, &["skysportpremierleague"]),
        pe(211, &["skysport1"]),
        pe(212, &["skysport2"]),
        pe(213, &["skysport3"]),
    ]
}

fn fr_general_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["tf1"]),
        pe(2, &["france2"]),
        pe(3, &["france3"]),
        pe(4, &["canalplus", "canal"]),
        pe(5, &["france5"]),
        pe(6, &["m6"]),
        pe(7, &["arte"]),
    ]
}

fn it_general_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["rai1"]),
        pe(2, &["rai2"]),
        pe(3, &["rai3"]),
        pe(4, &["rete4"]),
        pe(5, &["canale5"]),
        pe(6, &["italia1"]),
        pe(7, &["la7"]),
    ]
}

fn es_general_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["la1"]),
        pe(2, &["la2"]),
        pe(3, &["antena3"]),
        pe(4, &["telecinco"]),
        pe(5, &["cuatro"]),
        pe(6, &["lasexta"]),
        pe(7, &["movistarplus", "movistar+"]),
        pe(11, &["movistarestrenos", "mestr"]),
        pe(12, &["movistarestrenos2"]),
        pe(13, &["movistarseries", "mseries"]),
        pe(15, &["movistarcomedia"]),
        pe(16, &["movistardrama"]),
        pe(18, &["movistaraccion"]),
        pe(51, &["movistarliga", "mlaliga"]),
        pe(52, &["movistarligacampeones", "mchampions"]),
        pe(53, &["movistardeportes", "mdeportes"]),
        pe(57, &["movistargolf"]),
        pe(58, &["movistarf1"]),
    ]
}

fn pt_general_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["rtp1"]),
        pe(2, &["rtp2"]),
        pe(3, &["sic"]),
        pe(4, &["tvi"]),
    ]
}

fn nl_general_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["npo1"]),
        pe(2, &["npo2"]),
        pe(3, &["npo3"]),
        pe(4, &["rtl4"]),
        pe(5, &["rtl5"]),
        pe(6, &["sbs6"]),
        pe(7, &["veronica"]),
    ]
}

fn ie_general_plan() -> Vec<PlanEntry> {
    vec![
        pe(1, &["rte1", "rteone"]),
        pe(2, &["rte2"]),
        pe(3, &["tg4"]),
        pe(4, &["virginone", "virginmediaone"]),
        pe(5, &["virgintwo", "virginmedia2"]),
        pe(6, &["virginthree", "virginmedia3"]),
        pe(7, &["virginfour", "virginmedia4"]),
        pe(8, &["rtenews"]),
        pe(20, &["premsports1", "premiersports1"]),
        pe(21, &["premsports2", "premiersports2"]),
    ]
}

fn au_general_plan() -> Vec<PlanEntry> {
    vec![
        pe(2, &["abc", "abctv"]),
        pe(7, &["seven", "channel7"]),
        pe(9, &["nine", "channel9"]),
        pe(10, &["ten", "channel10"]),
        pe(11, &["sbs"]),
        pe(501, &["foxsports501"]),
        pe(502, &["foxsports502"]),
        pe(503, &["foxsports503"]),
        pe(504, &["foxsports504"]),
        pe(505, &["foxsports505"]),
        pe(510, &["espn1", "espn"]),
        pe(511, &["espn2"]),
        pe(520, &["beinsports1"]),
        pe(521, &["beinsports2"]),
        pe(522, &["beinsports3"]),
    ]
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysteryx_keeps_sky_history_2s_place() {
        assert_eq!(rank_for(Some("GB"), "MysteryX", None), rank_for(Some("GB"), "Sky History 2", None));
        assert_eq!(rank_for(Some("GB"), "MysteryX", None), Some(114));
        assert_eq!(
            rank_for(Some("GB"), "UK: MYSTERYX HD", Some(ContentCategory::Documentaries)),
            rank_for(Some("GB"), "Sky History 2", Some(ContentCategory::Documentaries)),
        );
    }

    // 5Select became 5Drama (Oct 2026): the new name keeps the old one's place on the dial. Twin of
    // LocaleChannelPlanTest, which cannot catch a Rust divergence.
    #[test]
    fn five_drama_keeps_five_selects_place() {
        // The Channel 5 diginets are ranked inside the Entertainment plan, so the category is given.
        let cat = Some(ContentCategory::Entertainment);
        assert!(rank_for(Some("GB"), "5Select", cat).is_some());
        assert_eq!(rank_for(Some("GB"), "5Drama", cat), rank_for(Some("GB"), "5Select", cat));
    }

    #[test]
    fn rank_for_gb_bbc_one() {
        assert_eq!(rank_for(Some("GB"), "BBC One", None), Some(1));
    }

    #[test]
    fn rank_for_gb_sky_sports_main_event() {
        assert_eq!(rank_for(Some("GB"), "Sky Sports Main Event", None), Some(401));
    }

    #[test]
    fn rank_for_gb_sky_news() {
        assert_eq!(rank_for(Some("GB"), "Sky News", None), Some(501));
    }

    #[test]
    fn rank_for_gb_sky_sports_with_sport_category() {
        // When category is provided, the category plan is consulted first.
        assert_eq!(
            rank_for(Some("GB"), "Sky Sports Main Event", Some(ContentCategory::Sport)),
            Some(1)
        );
    }

    #[test]
    fn rank_for_gb_sky_sports_action_and_nfl() {
        // Both Sky Sports Action and Sky Sports NFL map to rank 7 in Sport and 407 in General.
        assert_eq!(
            rank_for(Some("GB"), "Sky Sports Action", Some(ContentCategory::Sport)),
            Some(7)
        );
        assert_eq!(
            rank_for(Some("GB"), "Sky Sports NFL", Some(ContentCategory::Sport)),
            Some(7)
        );
        assert_eq!(
            rank_for(Some("GB"), "Sky Sports Action", None),
            Some(407)
        );
        assert_eq!(
            rank_for(Some("GB"), "Sky Sports NFL", None),
            Some(407)
        );
    }

    #[test]
    fn rank_for_us_cbs() {
        assert_eq!(rank_for(Some("US"), "CBS", None), Some(2));
    }

    #[test]
    fn rank_for_us_hbo() {
        assert_eq!(rank_for(Some("US"), "HBO", None), Some(501));
    }

    #[test]
    fn guide_rank_gb_sport_sky_sports_main_event() {
        // Both general and sport plan have the same token "skysportsmainevent" (length 20).
        // Equal-length ties resolve to the general plan, so this returns 401 (the general
        // dial number), not the sport plan's rank 1 banded at CATEGORY_BAND + 1.
        assert_eq!(
            guide_rank(Some("GB"), Some(ContentCategory::Sport), "Sky Sports Main Event"),
            Some(401)
        );
    }

    #[test]
    fn guide_rank_gb_sport_category_only_match() {
        // A channel that only exists in the sport plan (not the general plan)
        // should get CATEGORY_BAND + sport_rank.
        // "TNT Sports 1" is in gb_sport_plan at rank 30 but not in gb_general_plan.
        let result = guide_rank(Some("GB"), Some(ContentCategory::Sport), "TNT Sports 1");
        assert_eq!(result, Some(CATEGORY_BAND + 30));
    }

    #[test]
    fn rank_for_unknown_country_returns_none() {
        assert_eq!(rank_for(Some("XX"), "BBC One", None), None);
    }

    #[test]
    fn rank_for_none_country_returns_none() {
        assert_eq!(rank_for(None, "BBC One", None), None);
    }

    #[test]
    fn rank_for_blank_country_returns_none() {
        assert_eq!(rank_for(Some(""), "BBC One", None), None);
        assert_eq!(rank_for(Some("   "), "BBC One", None), None);
    }

    #[test]
    fn supported_country_codes_sorted() {
        let codes = supported_country_codes();
        assert_eq!(
            codes,
            vec!["AU", "CA", "DE", "ES", "FR", "GB", "HK", "IE", "IT", "NL", "PT", "US"]
        );
    }

    #[test]
    fn supported_country_codes_is_sorted() {
        let codes = supported_country_codes();
        let mut sorted = codes.clone();
        sorted.sort();
        assert_eq!(codes, sorted);
    }

    #[test]
    fn normalize_basic() {
        // normalize_for_plan_match replaces word "one" with "1", so "BBC One" → "bbc1".
        assert_eq!(normalize("BBC One"), "bbc1");
        assert_eq!(normalize("Sky Sports Main Event"), "skysportsmainevent");
        assert_eq!(normalize("Sky News"), "skynews");
    }

    #[test]
    fn token_matches_exact() {
        assert!(token_matches("bbcone", "bbcone"));
        assert!(!token_matches("bbcone", "bbctwo"));
    }

    #[test]
    fn token_matches_short_exact_only() {
        // Short tokens (< 3 chars) are exact-only.
        assert!(token_matches("id", "id"));
        assert!(!token_matches("idealworld", "id"));
    }

    #[test]
    fn token_matches_prefix() {
        assert!(token_matches("skysportsmainevent", "skysports"));
        assert!(!token_matches("skysports", "skysportscricket"));
    }

    #[test]
    fn token_matches_digit_boundary() {
        assert!(token_matches("dazn1", "dazn1"));
        assert!(token_matches("dazn1uk", "dazn1"));
        assert!(!token_matches("dazn10", "dazn1"));
        assert!(token_matches("dazn1uk", "dazn1uk"));
    }

    // Twin: LocaleChannelPlanTest `TNT Sports and BT Sport keep their dial slots`.
    #[test]
    fn guide_rank_gb_sport_tnt_and_bt_keep_their_slots() {
        let r = |n: &str| guide_rank(Some("GB"), Some(ContentCategory::Sport), n).unwrap();
        assert_eq!(r("TNT Sports 1"), r("BT Sport 1"));
        assert!(r("TNT Sports 1") < r("TNT Sports 2"));
        assert!(r("TNT Sports 4") < r("TNT Sports Extra"));
        assert!(r("Sky Sports Prem Sp") == r("Sky Sports Premier League"));
    }

    #[test]
    fn guide_rank_places_dazn_and_rally_behind_racing_tv() {
        let sport = |name| guide_rank(Some("GB"), Some(ContentCategory::Sport), name);
        assert_eq!(sport("Racing TV"), Some(424));
        assert_eq!(sport("Racing UK"), Some(424));
        assert_eq!(sport("DAZN 1"), Some(425));
        assert_eq!(sport("DAZN1 UK"), Some(425));
        assert_eq!(sport("UK: Rally TV FHD"), Some(426));
        assert_eq!(sport("DAZN 10"), None);
    }

    #[test]
    fn token_matches_suffix() {
        // Suffix matching requires token length >= 4.
        assert!(token_matches("skynews", "news"));
        assert!(!token_matches("skynews", "ws"));
    }

    #[test]
    fn token_matches_contains() {
        assert!(token_matches("theskynewschannel", "skynews"));
    }

    #[test]
    fn best_match_most_specific_wins() {
        let plan = vec![
            pe(1, &["sky"]),
            pe(2, &["skysports"]),
            pe(3, &["skysportsnews"]),
        ];
        let m = best_match(&plan, "skysportsnews").unwrap();
        assert_eq!(m.rank, 3);
        assert_eq!(m.token_length, "skysportsnews".len());
    }

    #[test]
    fn best_match_equal_length_tie_lower_rank_wins() {
        let plan = vec![
            pe(10, &["abc"]),
            pe(5, &["abc"]),
        ];
        let m = best_match(&plan, "abc").unwrap();
        assert_eq!(m.rank, 5);
    }

    #[test]
    fn rank_for_gb_lowercase_country_code() {
        assert_eq!(rank_for(Some("gb"), "BBC One", None), Some(1));
    }

    #[test]
    fn rank_for_hk_sport_category() {
        assert_eq!(
            rank_for(Some("HK"), "Now Sports 1", Some(ContentCategory::Sport)),
            Some(631)
        );
    }

    #[test]
    fn rank_for_ca_empty_regional_plan() {
        // CA regional plan is empty, so no match.
        assert_eq!(
            rank_for(Some("CA"), "Some Regional Channel", Some(ContentCategory::Regional)),
            None
        );
    }

    /// Twin of `LocaleChannelPlanTest.sky news arabia ranks below the domestic uk news
    /// channels`. This is the path that actually runs: `rank_for` is reached through
    /// `nativeRankFor` before the Kotlin body ever executes, so the Kotlin test alone would
    /// pass while a device kept the old order.
    #[test]
    fn rank_for_gb_news_sky_news_arabia_sits_below_sky_news() {
        let sky = rank_for(Some("GB"), "Sky News", Some(ContentCategory::News)).expect("Sky News is planned");
        let arabia = rank_for(Some("GB"), "Sky News Arabia", Some(ContentCategory::News))
            .expect("Sky News Arabia is planned, not unranked");
        assert!(
            arabia > sky,
            "Sky News Arabia ({arabia}) must rank below Sky News ({sky})"
        );
    }

    #[test]
    fn rank_for_gb_news_sky_news_keeps_its_own_rank() {
        // The longest-token rule is what makes the fix work: adding "skynewsarabia" must not
        // pull "Sky News" itself off rank 1.
        assert_eq!(rank_for(Some("GB"), "Sky News", Some(ContentCategory::News)), Some(1));
    }

    #[test]
    fn rank_for_gb_news_sky_news_arabia_sits_below_the_other_international_desks() {
        let arabia = rank_for(Some("GB"), "Sky News Arabia", Some(ContentCategory::News)).unwrap();
        for name in ["CGTN", "Arirang", "DW", "NHK World", "TRT World"] {
            if let Some(other) = rank_for(Some("GB"), name, Some(ContentCategory::News)) {
                assert!(
                    arabia > other,
                    "Sky News Arabia ({arabia}) should sit after {name} ({other})"
                );
            }
        }
    }


    #[test]
    fn guide_rank_gb_sky_news_arabia_below_sky_and_bbc_in_news_tab() {
        let sky = guide_rank(Some("GB"), Some(ContentCategory::News), "Sky News").unwrap();
        let bbc = guide_rank(Some("GB"), Some(ContentCategory::News), "BBC News").unwrap();
        let arabia = guide_rank(Some("GB"), Some(ContentCategory::News), "Sky News Arabia").unwrap();
        assert!(arabia > sky && arabia > bbc);
    }

    #[test]
    fn gb_local_plan_thats_tv_and_s4c_after_channel_5() {
        let ch5 = guide_rank(Some("GB"), Some(ContentCategory::Local), "Channel 5").unwrap();
        let thats = guide_rank(Some("GB"), Some(ContentCategory::Local), "That's TV").unwrap();
        let s4c = guide_rank(Some("GB"), Some(ContentCategory::Local), "S4C").unwrap();
        assert_eq!(ch5, 5);
        assert_eq!(thats, 6);
        assert_eq!(s4c, 7);
    }

}
