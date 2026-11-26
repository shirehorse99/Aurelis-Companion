//! Lineup & Channel Overrides Hub.
//!
//! Provides household-wide channel management:
//! 1. Manual channel overrides (permanently moving channels to different countries or groups,
//!    custom display names, pinned positions, channel numbers per country group, EPG IDs, or hiding).
//! 2. Channel catalog reporting from TV boxes.
//! 3. Two-way synchronization between Home Assistant and Aurelis TV boxes.
//! 4. Automatic & manual channel numbering per country group.

use axum::Extension;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use crate::auth::Session;
use crate::state::{AppState, WallClock};
use crate::bundle_store::write_atomic;

const OVERRIDES_FILE: &str = "lineup_overrides.json";
/// How long a reported channel stays in the editor without any television reporting it again.
pub const STALE_CHANNEL_MS: i64 = 21 * 24 * 3600 * 1000;
const CHANNELS_FILE: &str = "lineup_channels.json";
const COUNTRY_ORDER_FILE: &str = "lineup_country_order.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelOverride {
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_number: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub updated_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sports_extra_group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSummary {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_number: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub updated_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aurelis_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sports_extra_group: Option<String>,
    /// The television that last reported this channel, by name (set by the Companion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_by: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveOverrideRequest {
    pub channel_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub position: Option<i32>,
    #[serde(default)]
    pub channel_number: Option<i32>,
    #[serde(default)]
    pub epg_id: Option<String>,
    #[serde(default)]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub updated_at_ms: Option<i64>,
    #[serde(default)]
    pub logo_url: Option<String>,
    #[serde(default)]
    pub sports_extra_group: Option<String>,
    #[serde(default)]
    pub move_to_position: Option<usize>,
    #[serde(default)]
    pub clear_channel_number: bool,
    #[serde(default)]
    pub row_edit: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelReportRequest {
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
    pub channels: Vec<ChannelSummary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignNumbersRequest {
    /// Target country code (e.g. "GB", "US"). If omitted, all countries are processed.
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    /// Starting channel number (e.g. 101 for GB, 1 for US). Defaults to standard country base or 1.
    #[serde(default)]
    pub start_number: Option<i32>,
    /// Whether to overwrite existing custom channel numbers. Defaults to false.
    #[serde(default)]
    pub overwrite_existing: Option<bool>,
    /// The order numbers are handed out in: `current` (the order the editor and the TVs show
    /// now; the default), `name` (A to Z) or `provider` (the provider's own channel numbers).
    #[serde(default)]
    pub order: Option<String>,
}

/// The orders Auto-Assign can number a group in, as (form value, label).
pub const ASSIGN_ORDERS: &[(&str, &str)] = &[
    ("current", "Keep the current order"),
    ("name", "Name A to Z"),
    ("provider", "Provider's channel numbers"),
];

/// The orders the editor's list can be sorted in, as (query value, label).
pub const LIST_SORTS: &[(&str, &str)] = &[
    ("", "TV order (group, then dial #)"),
    ("number", "Dial # across all groups"),
    ("name", "Name A to Z"),
    ("name_desc", "Name Z to A"),
    ("edited", "Recently edited first"),
    ("provider", "Provider category"),
    ("country", "Country, then name"),
];

/// What the televisions are sent: the stored overrides, with each managed group's positions
/// worked out from the order the editor shows (see [`LineupStore::served_lineup`]).
#[derive(Debug, Clone)]
pub struct ServedLineup {
    pub overrides: Vec<ChannelOverride>,
    /// The household's order of countries; each television puts its own home country first.
    pub country_order: Vec<String>,
    pub etag: String,
}

#[derive(Default, Debug, Clone)]
pub struct ChannelQuery {
    pub country: Option<String>,
    pub category: Option<String>,
    pub search: Option<String>,
    pub page: Option<usize>,
    pub edited_only: bool,
    pub hidden_only: bool,
    pub unnumbered_only: bool,
    pub flash: Option<String>,
    pub sort: Option<String>,
}

#[derive(Debug, Clone)]
pub enum BulkAction {
    Hide,
    Unhide,
    SetCountry(String),
    SetGroup {
        category: Option<String>,
        sports_extra_group: Option<String>,
    },
    ClearNumber,
    Reset,
}

#[derive(Debug, Clone)]
pub struct BulkOverrideRequest {
    pub channel_ids: Vec<String>,
    pub action: BulkAction,
}

// The keys are ContentCategory.name on Android. Keep them separate from the display labels.
pub const AURELIS_GROUPS: &[(&str, &str)] = &[
    ("LOCAL", "Local"),
    ("ENTERTAINMENT", "Entertainment"),
    ("DOCUMENTARIES", "Documentaries"),
    ("MOVIES", "Movies"),
    ("SPORT", "Sport"),
    ("SPORT_EXTRA", "Sports Extra"),
    ("UHD", "4K UHD"),
    ("NEWS", "News"),
    ("CHILDREN", "Children"),
    ("MUSIC", "Music"),
    ("SHOPPING", "Shopping"),
    ("RELIGIOUS", "Religious"),
    ("RADIO", "Radio"),
    ("INTERNATIONAL", "International"),
    ("REGIONAL", "Regional"),
    ("OTHERS", "Others"),
    ("ADULT", "Adult"),
    ("HIDDEN", "Remove from groups"),
];

pub fn aurelis_group_key(value: &str) -> Option<&'static str> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("Sports") {
        return Some("SPORT");
    }
    if value.eq_ignore_ascii_case("UHD / 4K") {
        return Some("UHD");
    }
    AURELIS_GROUPS.iter().find_map(|&(key, label)| {
        (value.eq_ignore_ascii_case(key) || value.eq_ignore_ascii_case(label)).then_some(key)
    })
}

/// The display label for an Aurelis group key ("DOCUMENTARIES" -> "Documentaries").
pub fn group_label(key: &str) -> &'static str {
    AURELIS_GROUPS.iter().find(|&&(k, _)| k == key).map_or("Others", |&(_, label)| label)
}

pub fn aurelis_category_label(cat: aurelis_heuristics::ContentCategory) -> &'static str {
    use aurelis_heuristics::ContentCategory;
    match cat {
        ContentCategory::Local => "Local",
        ContentCategory::Regional => "Regional",
        ContentCategory::Entertainment => "Entertainment",
        ContentCategory::Documentaries => "Documentaries",
        ContentCategory::Movies => "Movies",
        ContentCategory::Sport => "Sport",
        ContentCategory::Uhd => "4K UHD",
        ContentCategory::News => "News",
        ContentCategory::Children => "Children",
        ContentCategory::Music => "Music",
        ContentCategory::Shopping => "Shopping",
        ContentCategory::Religious => "Religious",
        ContentCategory::Radio => "Radio",
        ContentCategory::International => "International",
        ContentCategory::Adult => "Adult",
        ContentCategory::Hidden => "Remove from groups",
        ContentCategory::Others => "Others",
    }
}

pub fn resolve_aurelis_category(ch: &ChannelSummary) -> &'static str {
    if let Some(override_key) = ch.original_genre.as_ref().and_then(|_| ch.genre.as_deref())
        .and_then(|genre| genre.split(',').next()).and_then(aurelis_group_key)
    {
        return AURELIS_GROUPS.iter().find(|&&(key, _)| key == override_key).unwrap().1;
    }
    resolve_automatic_aurelis_category(ch)
}

pub fn resolve_automatic_aurelis_category(ch: &ChannelSummary) -> &'static str {
    if let Some(key) = ch.aurelis_category.as_deref().and_then(aurelis_group_key) {
        return AURELIS_GROUPS.iter().find(|&&(group, _)| group == key).unwrap().1;
    }
    let cat = aurelis_heuristics::provider_genre_mapper::categorize(
        ch.country_code.as_deref(),
        ch.original_genre.as_deref().or(ch.genre.as_deref()).unwrap_or(""),
        &ch.name,
        None,
        None,
    );
    aurelis_category_label(cat)
}

pub fn matches_sports_folder(bank: &str, folder: &str) -> bool {
    bank == folder || bank.strip_prefix(folder).is_some_and(|rest| rest.starts_with("::"))
}

pub fn matches_category_filter(ch: &ChannelSummary, filter: &str) -> bool {
    if let Some(group) = filter.strip_prefix("AURELIS::").and_then(aurelis_group_key) {
        if let Some(override_groups) = ch.original_genre.as_ref().and_then(|_| ch.genre.as_deref()) {
            return override_groups.split(',').filter_map(aurelis_group_key).any(|key| key == group);
        }
        return aurelis_group_key(resolve_automatic_aurelis_category(ch)) == Some(group);
    }
    if let Some(folder) = filter.strip_prefix("SPORT_EXTRA::") {
        return resolve_aurelis_category(ch) == "Sports Extra" &&
            ch.sports_extra_group.as_deref().is_some_and(|bank| matches_sports_folder(bank, folder));
    }
    let f = filter.trim().to_lowercase();
    if f.is_empty() {
        return true;
    }
    let aurelis = resolve_aurelis_category(ch).to_lowercase();
    if aurelis == f || aurelis.contains(&f) {
        return true;
    }
    if ch.genre.as_deref().map(|g| g.to_lowercase().contains(&f)) == Some(true) {
        return true;
    }
    ch.original_genre.as_deref().map(|g| g.to_lowercase().contains(&f)) == Some(true)
}

pub fn urlencoding_decode(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '+' {
            out.push(' ');
        } else if c == '%' {
            let h1 = chars.next().unwrap_or(' ');
            let h2 = chars.next().unwrap_or(' ');
            let hex = format!("{h1}{h2}");
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                out.push(byte as char);
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn matches_search_filter(ch: &ChannelSummary, q_lower: &str) -> bool {
    ch.name.to_lowercase().contains(q_lower)
        || ch.id.to_lowercase().contains(q_lower)
        || ch.original_name.as_deref().is_some_and(|n| n.to_lowercase().contains(q_lower))
        || ch.genre.as_deref().is_some_and(|g| g.to_lowercase().contains(q_lower))
        || ch.original_genre.as_deref().is_some_and(|g| g.to_lowercase().contains(q_lower))
        || ch.sports_extra_group.as_deref().is_some_and(|s| s.to_lowercase().contains(q_lower))
        || ch.channel_number.is_some_and(|n| n.to_string().contains(q_lower))
}

pub fn is_valid_logo_url(logo: &str) -> bool {
    let logo = logo.trim();
    !logo.is_empty()
        && logo.len() <= 2048
        && reqwest::Url::parse(logo).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
        })
}

pub fn parse_channel_query(raw_query: &str) -> ChannelQuery {
    let mut q = ChannelQuery::default();
    for part in raw_query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            let val = urlencoding_decode(v);
            if !val.trim().is_empty() {
                let truthy = matches!(val.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes");
                match k {
                    "country" => q.country = Some(val),
                    "category" => q.category = Some(val),
                    "search" | "q" => q.search = Some(val),
                    "page" => q.page = val.trim().parse::<usize>().ok().map(|p| p.max(1)),
                    "edited" => q.edited_only = truthy,
                    "hidden" => q.hidden_only = truthy,
                    "unnumbered" => q.unnumbered_only = truthy,
                    "f" => q.flash = Some(val.trim().to_string()),
                    "sort" => q.sort = LIST_SORTS.iter().find(|(key, _)| *key == val.trim()).map(|(key, _)| (*key).to_string()),
                    _ => {}
                }
            }
        }
    }
    q
}

#[derive(Serialize, Deserialize)]
struct StoredOverrides {
    version: u32,
    overrides: Vec<ChannelOverride>,
}

#[derive(Serialize, Deserialize)]
struct StoredChannels {
    version: u32,
    channels: Vec<ChannelSummary>,
}

impl ChannelOverride {
    fn new(channel_id: String, updated_at_ms: i64) -> Self {
        Self {
            channel_id,
            display_name: None,
            category: None,
            country_code: None,
            position: None,
            channel_number: None,
            epg_id: None,
            hidden: None,
            logo_url: None,
            sports_extra_group: None,
            updated_at_ms,
        }
    }

    fn is_empty(&self) -> bool {
        self.display_name.is_none()
            && self.category.is_none()
            && self.country_code.is_none()
            && self.position.is_none()
            && self.channel_number.is_none()
            && self.epg_id.is_none()
            && self.hidden.is_none()
            && self.logo_url.is_none()
            && self.sports_extra_group.is_none()
    }
}

pub fn dial_country(country: Option<&str>) -> String {
    match country.unwrap_or_default().trim().to_ascii_uppercase().as_str() {
        "" => "UNKNOWN".into(),
        "UK" => "GB".into(),
        country => country.into(),
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DialGroup {
    country: String,
    category: &'static str,
    folder: Option<String>,
}

impl std::fmt::Display for DialGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} / {}", self.country, self.category)?;
        if let Some(folder) = &self.folder {
            write!(f, " / {}", folder.replace("::", " / "))?;
        }
        Ok(())
    }
}

/// The television tab a group is drawn in. A television's Entertainment tab carries the viewer's
/// own Local channels (`EpgGroupFilter.isLocalOrRegionalForUser`). Numbered apart, the two lists
/// interleaved on screen: Local's first place beside Entertainment's first, and the same dial
/// number twice in one tab (owner, 25 Nov 2026). So Local is ordered and numbered inside
/// Entertainment. Regional (other regions' editions) has a tab of its own on a television with a
/// postcode, so it keeps its own group here (owner, 26 Nov 2026).
pub fn tab_category(label: &'static str) -> &'static str {
    match label {
        "Local" => "Entertainment",
        other => other,
    }
}

pub fn dial_group(channel: &ChannelSummary) -> DialGroup {
    let category = tab_category(resolve_aurelis_category(channel));
    DialGroup {
        country: dial_country(channel.country_code.as_deref()),
        category,
        folder: match category {
            "Sports Extra" => channel.sports_extra_group.clone(),
            "Others" => channel.original_genre.as_ref().or(channel.genre.as_ref())
                .map(|genre| genre.trim().to_owned()).filter(|genre| !genre.is_empty()),
            _ => None,
        },
    }
}

fn effective_channel(ch: &ChannelSummary, row: Option<&ChannelOverride>) -> ChannelSummary {
    let mut effective = ch.clone();
    if let Some(ov) = row {
        if let Some(name) = &ov.display_name {
            effective.original_name = Some(ch.name.clone());
            effective.name = name.clone();
        }
        if let Some(cat) = &ov.category {
            effective.original_genre = Some(ch.genre.clone().unwrap_or_default());
            effective.genre = Some(cat.clone());
        }
        if let Some(cc) = &ov.country_code {
            effective.country_code = Some(cc.clone());
        }
        if let Some(num) = ov.channel_number {
            effective.channel_number = Some(num);
        }
        if let Some(h) = ov.hidden {
            effective.hidden = Some(h);
        }
        if let Some(logo) = &ov.logo_url {
            effective.logo_url = Some(logo.clone());
        }
        if let Some(folder) = &ov.sports_extra_group {
            effective.sports_extra_group = Some(folder.clone());
        }
    }
    effective
}

fn channel_dial(channel: Option<&ChannelSummary>, row: Option<&ChannelOverride>) -> (DialGroup, Option<i32>) {
    if let Some(channel) = channel {
        let effective = effective_channel(channel, row);
        return (dial_group(&effective), effective.channel_number);
    }
    let category = row.and_then(|row| row.category.as_deref()).and_then(|csv| csv.split(',').next())
        .and_then(aurelis_group_key)
        .and_then(|key| AURELIS_GROUPS.iter().find(|&&(group, _)| group == key).map(|&(_, label)| label))
        .unwrap_or("Others");
    let category = tab_category(category);
    let folder = if category == "Sports Extra" { row.and_then(|row| row.sports_extra_group.clone()) } else { None };
    let country = dial_country(row.and_then(|row| row.country_code.as_deref()));
    (DialGroup { country, category, folder }, row.and_then(|row| row.channel_number))
}

pub struct LineupStore {
    dir: PathBuf,
    overrides: Mutex<HashMap<String, ChannelOverride>>,
    channels: Mutex<HashMap<String, ChannelSummary>>,
    /// Bumped on every write to either map; the served lineup is rebuilt when it moves.
    generation: AtomicU64,
    served: Mutex<Option<(u64, Arc<ServedLineup>)>>,
    /// The household's order of countries (see [`ServedLineup::country_order`]).
    country_order: Mutex<Vec<String>>,
    wall: WallClock,
}

/// A tab as the editor names it: a country, an Aurelis group key ("DOCUMENTARIES") and, for
/// Sports Extra, a folder.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabRef {
    pub country: String,
    pub group: String,
    #[serde(default)]
    pub folder: Option<String>,
}

impl TabRef {
    fn dial_group(&self) -> Option<DialGroup> {
        let key = aurelis_group_key(&self.group)?;
        let label = tab_category(AURELIS_GROUPS.iter().find(|&&(k, _)| k == key)?.1);
        Some(DialGroup {
            country: dial_country(Some(&self.country)),
            category: label,
            folder: self.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()).map(str::to_owned),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabSummary {
    pub country: String,
    pub group: &'static str,
    pub label: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub count: usize,
    pub hidden: usize,
    /// The household has ordered this tab, so the televisions draw exactly the editor's order.
    pub managed: bool,
}

impl TabSummary {
    fn new(group: &DialGroup) -> Self {
        let key = AURELIS_GROUPS.iter().find(|&&(_, l)| l == group.category).map_or("OTHERS", |&(k, _)| k);
        Self { country: group.country.clone(), group: key, label: group.category, folder: group.folder.clone(), count: 0, hidden: 0, managed: false }
    }
}

#[derive(Debug, Clone)]
pub struct TabView {
    pub tab: TabSummary,
    pub channels: Vec<ChannelSummary>,
    /// Channels in this tab carrying any household override.
    pub edited: HashSet<String>,
    /// A listed channel's twins: the same channel name in the same tab under other ids, from
    /// another television (or app version) that keyed it differently, or kept apart by one
    /// television's merge. Every change to the listed channel is made to its twins too.
    pub twins: HashMap<String, Vec<ChannelSummary>>,
}

/// Two reports of the same channel: the same name, ignoring case, spaces and punctuation.
pub fn twin_name_key(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric() || *c == '+').flat_map(char::to_lowercase).collect()
}

/// The ids, other than `id`, that are the same channel as it would be with `row` applied:
/// the same tab and the same name. A dial number shared among them is not a clash.
fn twin_ids(
    channels: &HashMap<String, ChannelSummary>,
    overrides: &HashMap<String, ChannelOverride>,
    id: &str,
    row: Option<&ChannelOverride>,
) -> HashSet<String> {
    let Some(base) = channels.get(id) else { return HashSet::new() };
    let me = effective_channel(base, row);
    let (group, key) = (dial_group(&me), twin_name_key(&me.name));
    channels.values().filter(|ch| ch.id != id).filter(|ch| {
        let other = effective_channel(ch, overrides.get(&ch.id));
        twin_name_key(&other.name) == key && dial_group(&other) == group
    }).map(|ch| ch.id.clone()).collect()
}

/// The television's automatic order for a tab nobody has ordered, as far as the Companion can
/// reproduce it: `EpgChannelSort`'s plan rank (a family takes its best member's rank), then the
/// family together, the main channel before its +1, then the provider's number, then the name.
/// The television's per-box "most watched" lane is not knowable here.
fn automatic_order(members: &mut [ChannelSummary], group: &DialGroup) {
    let category = AURELIS_GROUPS.iter().find(|&&(_, l)| l == group.category)
        .and_then(|&(key, _)| aurelis_heuristics::ContentCategory::from_str(key));
    let country = (group.country != "UNKNOWN").then_some(group.country.as_str());
    let keyed: Vec<(Option<i32>, String, bool)> = members.iter().map(|ch| (
        aurelis_heuristics::locale_channel_plan::guide_rank(country, category, &ch.name),
        aurelis_heuristics::channel_curation_keys::family_key(&ch.name),
        aurelis_heuristics::channel_name_keys::is_timeshift(&ch.name),
    )).collect();
    let mut family_best: HashMap<&str, i32> = HashMap::new();
    for (rank, family, shift) in &keyed {
        if let (Some(rank), false) = (rank, shift) {
            let best = family_best.entry(family.as_str()).or_insert(i32::MAX);
            *best = (*best).min(*rank);
        }
    }
    let mut order: Vec<usize> = (0..members.len()).collect();
    let key = |i: usize| {
        let (rank, family, shift) = &keyed[i];
        let effective = family_best.get(family.as_str()).copied().or(*rank).unwrap_or(i32::MAX);
        (effective, family.clone(), *shift, members[i].channel_number.unwrap_or(i32::MAX), members[i].name.to_lowercase(), members[i].id.clone())
    };
    order.sort_by_cached_key(|&i| key(i));
    let sorted: Vec<ChannelSummary> = order.into_iter().map(|i| members[i].clone()).collect();
    members.clone_from_slice(&sorted);
}

/// The order the editor lists a group in, and so the order the televisions draw it in:
/// numbered channels by dial number, then the unnumbered by name.
fn display_order_key(channel: &ChannelSummary) -> (bool, Option<i32>, String, String) {
    (channel.channel_number.is_none(), channel.channel_number, channel.name.clone(), channel.id.clone())
}

/// The overrides as the televisions should apply them.
///
/// A television sorts a tab by `position` before anything else and never reads the dial
/// number, while the editor lists a group by dial number. Stored positions went stale
/// whenever a number was typed or Auto-Assign ran (25 Nov 2026: 720 positions from one A-Z
/// run, so no renumbering since then reached a television, and a channel moved into a group
/// had no position and sank below every numbered one). So for every group the household has
/// numbered or moved anything in, each reported channel is sent its rank in the editor's
/// order. Groups nobody has touched keep the television's own smart order.
fn compute_served(
    channels: &HashMap<String, ChannelSummary>,
    overrides: &HashMap<String, ChannelOverride>,
    country_order: &[String],
) -> ServedLineup {
    let mut rows = overrides.clone();
    let mut groups: HashMap<DialGroup, Vec<ChannelSummary>> = HashMap::new();
    let mut managed: HashSet<DialGroup> = HashSet::new();
    for channel in channels.values() {
        let row = overrides.get(&channel.id);
        let effective = effective_channel(channel, row);
        let group = dial_group(&effective);
        if row.is_some_and(|row| row.channel_number.is_some() || row.position.is_some()) {
            managed.insert(group.clone());
        }
        groups.entry(group).or_default().push(effective);
    }
    for (id, row) in rows.iter_mut() {
        if channels.contains_key(id) {
            row.position = None;
        }
    }
    for (group, mut members) in groups {
        if !managed.contains(&group) {
            continue;
        }
        members.sort_by_cached_key(display_order_key);
        for (index, channel) in members.iter().enumerate() {
            rows.entry(channel.id.clone())
                .or_insert_with(|| ChannelOverride::new(channel.id.clone(), 0))
                .position = Some(index as i32);
        }
    }
    let mut overrides: Vec<ChannelOverride> = rows.into_values().filter(|row| !row.is_empty()).collect();
    overrides.sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
    let mut etag = compute_overrides_etag(&overrides);
    if !country_order.is_empty() {
        // A new order is a new lineup to the televisions.
        let mut hasher = Sha256::new();
        hasher.update(etag.as_bytes());
        hasher.update(country_order.join(",").as_bytes());
        let digest = hasher.finalize();
        etag = format!("v1-{}", digest[..8].iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
    ServedLineup { overrides, country_order: country_order.to_vec(), etag }
}

fn compute_overrides_etag(list: &[ChannelOverride]) -> String {
    let mut hasher = Sha256::new();
    for row in list {
        if let Ok(bytes) = serde_json::to_vec(row) {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(19);
    hex.push_str("v1-");
    for b in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

impl LineupStore {
    pub fn new(dir: PathBuf, wall: WallClock) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let dir_for_order = dir.clone();

        let overrides_map = {
            let path = dir.join(OVERRIDES_FILE);
            let mut map = HashMap::new();
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(stored) = serde_json::from_slice::<StoredOverrides>(&bytes) {
                    for ov in stored.overrides {
                        map.insert(ov.channel_id.clone(), ov);
                    }
                }
            }
            map
        };
        let overrides = Mutex::new(overrides_map);

        let channels = {
            let path = dir.join(CHANNELS_FILE);
            let mut map = HashMap::new();
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(stored) = serde_json::from_slice::<StoredChannels>(&bytes) {
                    for ch in stored.channels {
                        map.insert(ch.id.clone(), ch);
                    }
                }
            }
            Mutex::new(map)
        };

        Self {
            dir,
            overrides,
            channels,
            generation: AtomicU64::new(0),
            served: Mutex::new(None),
            country_order: Mutex::new(
                std::fs::read(dir_for_order.join(COUNTRY_ORDER_FILE)).ok()
                    .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
                    .unwrap_or_default(),
            ),
            wall,
        }
    }

    pub fn overrides_etag(&self) -> String {
        self.served_lineup().etag.clone()
    }

    /// What the televisions are sent, rebuilt only after a write.
    pub fn served_lineup(&self) -> Arc<ServedLineup> {
        let generation = self.generation.load(Ordering::SeqCst);
        if let Some((built, served)) = self.served.lock().unwrap().as_ref() {
            if *built == generation {
                return served.clone();
            }
        }
        let served = {
            let channels = self.channels.lock().unwrap();
            let overrides = self.overrides.lock().unwrap();
            let order = self.country_order.lock().unwrap().clone();
            Arc::new(compute_served(&channels, &overrides, &order))
        };
        *self.served.lock().unwrap() = Some((generation, served.clone()));
        served
    }

    /// The household's country order, as saved.
    pub fn country_order(&self) -> Vec<String> {
        self.country_order.lock().unwrap().clone()
    }

    /// Save the household's country order: two-letter codes, first to last, no repeats.
    pub fn set_country_order(&self, order: &[String]) -> Result<Vec<String>, String> {
        let mut clean: Vec<String> = Vec::new();
        for raw in order {
            let code = dial_country(Some(raw));
            if code.len() != 2 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err(format!("'{raw}' is not a two-letter country code."));
            }
            if !clean.contains(&code) {
                clean.push(code);
            }
        }
        let bytes = serde_json::to_vec(&clean).map_err(|e| e.to_string())?;
        write_atomic(&self.dir, COUNTRY_ORDER_FILE, &bytes)?;
        *self.country_order.lock().unwrap() = clean.clone();
        self.invalidate_served();
        Ok(clean)
    }

    fn invalidate_served(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn persist_overrides(&self, map: &HashMap<String, ChannelOverride>) -> Result<(), String> {
        let mut list: Vec<ChannelOverride> = map.values().cloned().collect();
        list.sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
        let stored = StoredOverrides {
            version: 1,
            overrides: list,
        };
        let bytes = serde_json::to_vec_pretty(&stored).map_err(|e| e.to_string())?;
        write_atomic(&self.dir, OVERRIDES_FILE, &bytes)?;
        self.invalidate_served();
        Ok(())
    }

    fn persist_channels(&self, map: &HashMap<String, ChannelSummary>) -> Result<(), String> {
        let mut list: Vec<ChannelSummary> = map.values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        let stored = StoredChannels {
            version: 1,
            channels: list,
        };
        let bytes = serde_json::to_vec_pretty(&stored).map_err(|e| e.to_string())?;
        let written = write_atomic(&self.dir, CHANNELS_FILE, &bytes);
        self.invalidate_served();
        written
    }

    pub fn get_overrides(&self) -> Vec<ChannelOverride> {
        let lock = self.overrides.lock().unwrap();
        let mut list: Vec<ChannelOverride> = lock.values().cloned().collect();
        list.sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
        list
    }

    pub fn get_override(&self, channel_id: &str) -> Option<ChannelOverride> {
        let lock = self.overrides.lock().unwrap();
        lock.get(channel_id).cloned()
    }

    pub fn set_override(&self, req: SaveOverrideRequest) -> Result<ChannelOverride, String> {
        self.save_override(req).map(|(row, _)| row)
    }

    /// [`Self::set_override`], also saying whether anything changed. A save that would leave
    /// the row as it was writes nothing, so "Save all changes" touches only edited rows.
    pub fn save_override(&self, req: SaveOverrideRequest) -> Result<(ChannelOverride, bool), String> {
        let channel_id = req.channel_id.trim().to_string();
        if channel_id.is_empty() {
            return Err("channelId cannot be empty".to_string());
        }

        let now = (self.wall)();
        let channels = self.channels.lock().unwrap();
        let mut lock = self.overrides.lock().unwrap();

        let norm_country = req.country_code.as_deref().and_then(|cc| {
            let t = cc.trim().to_uppercase();
            if t.is_empty() { None } else { Some(t) }
        });

        let norm_category = req.category.as_deref().and_then(|c| {
            let t = c.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        });

        let norm_name = req.display_name.as_deref().and_then(|n| {
            let t = n.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        });

        let norm_epg = req.epg_id.as_deref().and_then(|e| {
            let t = e.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        });

        if let Some(num) = req.channel_number {
            if num <= 0 {
                return Err("Enter a positive dial number, or leave it blank.".into());
            }
        }

        // Merge with existing override if present
        let mut existing = lock.get(&channel_id).cloned()
            .unwrap_or_else(|| ChannelOverride::new(channel_id.clone(), now));

        if req.display_name.is_some() {
            existing.display_name = norm_name;
        }
        if req.category.is_some() {
            existing.category = norm_category;
        }
        if req.country_code.is_some() {
            existing.country_code = norm_country;
        }
        if req.position.is_some() {
            existing.position = req.position;
        }
        if req.move_to_position.is_none() {
            if req.clear_channel_number {
                existing.channel_number = None;
            } else if req.channel_number.is_some() {
                existing.channel_number = req.channel_number;
            }
        }
        if req.epg_id.is_some() {
            existing.epg_id = norm_epg;
        }
        if req.hidden.is_some() {
            existing.hidden = req.hidden;
        }
        if let Some(raw) = req.logo_url.as_deref() {
            let logo = raw.trim();
            if !logo.is_empty() && !is_valid_logo_url(logo) {
                return Err("Enter an HTTP or HTTPS image URL, or leave it blank for the provider logo.".into());
            }
            existing.logo_url = (!logo.is_empty()).then(|| logo.to_owned());
        }
        if let Some(folder) = req.sports_extra_group.as_deref() {
            let folder = folder.trim();
            if !folder.is_empty() {
                let known = channels.values().filter_map(|ch| ch.sports_extra_group.as_deref())
                    .any(|bank| matches_sports_folder(bank, folder))
                    || lock.values().filter_map(|row| row.sports_extra_group.as_deref())
                        .any(|bank| matches_sports_folder(bank, folder));
                if !known {
                    return Err("Choose a Sports Extra folder reported by an Aurelis TV.".into());
                }
                existing.category = Some("SPORT_EXTRA".into());
            }
            existing.sports_extra_group = (!folder.is_empty()).then(|| folder.to_owned());
        } else if req.category.is_some() && existing.category.as_deref() != Some("SPORT_EXTRA") {
            existing.sports_extra_group = None;
        }

        let base = channels.get(&channel_id);
        if req.row_edit {
            if let Some(base) = base {
                if existing.display_name.as_deref() == Some(base.name.as_str()) {
                    existing.display_name = None;
                }
                if let Some(cc) = existing.country_code.as_deref() {
                    if dial_country(Some(cc)) == dial_country(base.country_code.as_deref()) {
                        existing.country_code = None;
                    }
                }
                if req.move_to_position.is_none() && existing.channel_number == base.channel_number {
                    existing.channel_number = None;
                }
                let base_hidden = base.hidden == Some(true);
                if existing.hidden == Some(base_hidden) {
                    existing.hidden = None;
                }
            } else if existing.hidden == Some(false) {
                existing.hidden = None;
            }
        }
        existing.updated_at_ms = req.updated_at_ms.unwrap_or(now);

        if let Some(position) = req.move_to_position {
            let mut updated = lock.clone();
            updated.insert(channel_id.clone(), existing);
            Self::move_in_group(&channels, &mut updated, &channel_id, position, now)?;
            self.persist_overrides(&updated)?;
            let result = updated[&channel_id].clone();
            *lock = updated;
            return Ok((result, true));
        }

        // Checked before the empty-row path: clearing a row back to its provider's number can
        // land it on a dial another channel has since been given.
        let previous_dial = channel_dial(base, lock.get(&channel_id));
        let new_dial = channel_dial(base, Some(&existing));
        if let (country, Some(number)) = &new_dial {
            if new_dial != previous_dial {
                let twins = twin_ids(&channels, &lock, &channel_id, Some(&existing));
                let conflict = channels.keys().chain(lock.keys()).find(|&id| {
                    id != &channel_id && !twins.contains(id) && channel_dial(channels.get(id), lock.get(id)) == new_dial
                });
                if let Some(id) = conflict {
                    let name = lock.get(id).and_then(|row| row.display_name.as_deref())
                        .or_else(|| channels.get(id).map(|channel| channel.name.as_str()))
                        .unwrap_or(id);
                    return Err(format!("Dial {number} is already assigned to {name} in {country}. Choose a different number."));
                }
            }
        }

        if req.row_edit && existing.is_empty() {
            let previous = lock.remove(&channel_id);
            let changed = previous.is_some();
            if previous.is_some() {
                if let Err(error) = self.persist_overrides(&lock) {
                    if let Some(previous) = previous {
                        lock.insert(channel_id, previous);
                    }
                    return Err(error);
                }
            }
            return Ok((existing, changed));
        }
        if let Some(previous) = lock.get(&channel_id) {
            let mut unchanged = existing.clone();
            unchanged.updated_at_ms = previous.updated_at_ms;
            if &unchanged == previous {
                return Ok((previous.clone(), false));
            }
        }

        let previous = lock.insert(channel_id.clone(), existing.clone());
        if let Err(error) = self.persist_overrides(&lock) {
            if let Some(previous) = previous {
                lock.insert(channel_id, previous);
            } else {
                lock.remove(&channel_id);
            }
            return Err(error);
        }
        Ok((existing, true))
    }

    /// Move a reported channel to a one-based position, using the group's existing dial
    /// slots in ascending order. Allocate more slots for unnumbered channels as needed.
    fn move_in_group(
        channels: &HashMap<String, ChannelSummary>,
        overrides: &mut HashMap<String, ChannelOverride>,
        id: &str,
        position: usize,
        now: i64,
    ) -> Result<(), String> {
        let channel = channels.get(id).ok_or("A TV must report this channel before it can be moved.")?;
        let (country, _) = channel_dial(Some(channel), overrides.get(id));
        let mut group: Vec<_> = channels.values().filter_map(|channel| {
            let entry = overrides.get(&channel.id);
            let (group_country, number) = channel_dial(Some(channel), entry);
            (group_country == country).then(|| {
                let name = entry.and_then(|row| row.display_name.as_deref()).unwrap_or(&channel.name);
                (channel, number, name.to_owned())
            })
        }).collect();
        if position == 0 || position > group.len() {
            return Err(format!("Choose a position from 1 to {} in {country}.", group.len()));
        }
        group.sort_by(|(a, an, name_a), (b, bn, name_b)| {
            an.is_none().cmp(&bn.is_none()).then_with(|| an.cmp(bn))
                .then_with(|| name_a.cmp(name_b)).then_with(|| a.id.cmp(&b.id))
        });
        let from = group.iter().position(|(channel, _, _)| channel.id == id).unwrap();
        let moved = group.remove(from);
        group.insert(position - 1, moved);

        let reserved: HashSet<_> = overrides.iter().filter_map(|(id, row)| {
            if channels.contains_key(id) { return None; }
            let (group_country, number) = channel_dial(None, Some(row));
            (group_country == country).then_some(number).flatten()
        }).collect();
        let mut slots: BTreeSet<_> = group.iter().filter_map(|(_, number, _)| *number)
            .filter(|number| !reserved.contains(number)).collect();
        let mut next = slots.last().map_or(if matches!(country.country.as_str(), "GB" | "IE") { 101 } else { 1 }, |n| i64::from(*n) + 1);
        while slots.len() < group.len() {
            let number = i32::try_from(next).map_err(|_| format!("No more dial numbers in {country}."))?;
            next += 1;
            if !reserved.contains(&number) {
                slots.insert(number);
            }
        }
        for (index, ((channel, _, _), number)) in group.into_iter().zip(slots).enumerate() {
            let row = overrides.entry(channel.id.clone())
                .or_insert_with(|| ChannelOverride::new(channel.id.clone(), now));
            row.position = Some(index as i32);
            row.channel_number = Some(number);
            row.updated_at_ms = now;
        }
        Ok(())
    }

    pub fn delete_override(&self, channel_id: &str) -> Result<bool, String> {
        let mut lock = self.overrides.lock().unwrap();
        if lock.remove(channel_id).is_some() {
            self.persist_overrides(&lock)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn report_channels(&self, channels: Vec<ChannelSummary>) -> Result<usize, String> {
        self.report_channels_from(channels, None)
    }

    /// [`Self::report_channels`], naming the television the list came from.
    pub fn report_channels_from(&self, channels: Vec<ChannelSummary>, reporter: Option<&str>) -> Result<usize, String> {
        let now = (self.wall)();
        let mut lock = self.channels.lock().unwrap();
        let mut updated = 0;
        for mut ch in channels {
            let id = ch.id.trim().to_string();
            if id.is_empty() {
                continue;
            }
            ch.id = id.clone();
            // Reported now, whatever the sender stamped: the age-out below reads this.
            ch.updated_at_ms = now;
            ch.reported_by = reporter.map(str::to_owned);
            if let Some(previous) = lock.get(&id) {
                if ch.aurelis_category.is_none() {
                    ch.aurelis_category = previous.aurelis_category.clone();
                    ch.sports_extra_group = previous.sports_extra_group.clone();
                }
            }
            lock.insert(id, ch);
            updated += 1;
        }
        // A channel no television has reported for three weeks has gone from every lineup (an
        // old box, a provider's renamed event channel): it leaves the editor. Its override, if
        // any, stays, so it comes back exactly as edited if a television reports it again.
        let cutoff = now - STALE_CHANNEL_MS;
        let before = lock.len();
        lock.retain(|_, ch| ch.updated_at_ms >= cutoff);
        let dropped = before - lock.len();
        if dropped > 0 {
            eprintln!("lineup: {dropped} channels no television has reported for 21 days left the editor");
        }
        self.persist_channels(&lock)?;
        Ok(updated)
    }

    pub fn get_channels(&self) -> Vec<ChannelSummary> {
        let channels_lock = self.channels.lock().unwrap();
        let overrides_lock = self.overrides.lock().unwrap();

        let mut out = Vec::with_capacity(channels_lock.len());
        for ch in channels_lock.values() {
            out.push(effective_channel(ch, overrides_lock.get(&ch.id)));
        }
        out.sort_by_cached_key(|channel| (
            dial_group(channel), channel.channel_number.is_none(),
            channel.channel_number, channel.name.clone(), channel.id.clone(),
        ));
        out
    }

    /// Automatically assigns dial numbers independently for each country and Aurelis group.
    ///
    /// Standard base numbers:
    /// - UK (GB): 101+
    /// - US: 1+
    /// - IE: 101+
    /// - Default: 1+
    pub fn assign_channel_numbers(&self, req: AssignNumbersRequest) -> Result<usize, String> {
        let now = (self.wall)();
        if let Some(start) = req.start_number {
            if start <= 0 {
                return Err("Enter a positive starting dial number, or leave it blank.".into());
            }
        }
        let target_country = req.country_code.as_deref().filter(|country| !country.trim().is_empty())
            .map(|country| dial_country(Some(country)));
        let overwrite = req.overwrite_existing.unwrap_or(false);
        let order = match req.order.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
            None => "current",
            Some(order) => ASSIGN_ORDERS.iter().find(|(key, _)| *key == order).map(|(key, _)| *key)
                .ok_or_else(|| format!("Unknown numbering order '{order}'."))?,
        };

        let channels = self.channels.lock().unwrap();
        let mut overrides = self.overrides.lock().unwrap();
        let mut updated = overrides.clone();
        let mut grouped: HashMap<DialGroup, Vec<(&ChannelSummary, Option<i32>)>> = HashMap::new();
        let mut reserved: HashMap<DialGroup, HashSet<i32>> = HashMap::new();

        for channel in channels.values() {
            let (country, number) = channel_dial(Some(channel), overrides.get(&channel.id));
            if target_country.as_ref().is_some_and(|target| target != &country.country) || req.group.as_deref().is_some_and(|group| {
                if let Some(folder) = group.strip_prefix("SPORT_EXTRA::") {
                    country.category != "Sports Extra" || !country.folder.as_deref().is_some_and(|bank| matches_sports_folder(bank, folder))
                } else {
                    let key = group.strip_prefix("AURELIS::").unwrap_or(group);
                    aurelis_group_key(key) != aurelis_group_key(country.category)
                }
            }) {
                continue;
            }
            if !overwrite {
                if let Some(number) = number {
                    reserved.entry(country.clone()).or_default().insert(number);
                }
            }
            grouped.entry(country).or_default().push((channel, number));
        }

        // An override can be entered by ID before any TV has reported that channel.
        for (_, row) in overrides.iter().filter(|(id, _)| !channels.contains_key(*id)) {
            let (country, number) = channel_dial(None, Some(row));
            if let Some(number) = number {
                reserved.entry(country).or_default().insert(number);
            }
        }

        let mut assigned_count = 0;
        for (country, mut group) in grouped {
            let effective_name = |channel: &ChannelSummary| {
                overrides.get(&channel.id).and_then(|row| row.display_name.as_deref())
            };
            group.sort_by(|(a, a_number), (b, b_number)| {
                let a_name = effective_name(a).unwrap_or(&a.name);
                let b_name = effective_name(b).unwrap_or(&b.name);
                let by_name = a_name.cmp(b_name).then_with(|| a.id.cmp(&b.id));
                match order {
                    "name" => by_name,
                    // The provider's own numbers, as the television reported them.
                    "provider" => a.channel_number.is_none().cmp(&b.channel_number.is_none())
                        .then_with(|| a.channel_number.cmp(&b.channel_number)).then_with(|| by_name),
                    // What the editor and the televisions show now, so a renumber keeps the order.
                    _ => a_number.is_none().cmp(&b_number.is_none())
                        .then_with(|| a_number.cmp(b_number)).then_with(|| by_name),
                }
            });

            let base = req.start_number.unwrap_or(match country.country.as_str() {
                "GB" | "IE" => 101,
                _ => 1,
            });
            let used = reserved.entry(country.clone()).or_default();
            let mut next = i64::from(base);
            for (channel, number) in group {
                if number.is_some() && !overwrite {
                    continue;
                }

                let number = loop {
                    let candidate = i32::try_from(next).map_err(|_| format!("No more dial numbers in {country}."))?;
                    next += 1;
                    if used.insert(candidate) {
                        break candidate;
                    }
                };
                let row = updated.entry(channel.id.clone())
                    .or_insert_with(|| ChannelOverride::new(channel.id.clone(), now));
                row.channel_number = Some(number);
                if overwrite {
                    row.position = None;
                }
                row.updated_at_ms = now;
                assigned_count += 1;
            }
        }

        if assigned_count > 0 {
            self.persist_overrides(&updated)?;
            *overrides = updated;
        }
        Ok(assigned_count)
    }

    /// Re-sort an already filtered list for the editor. `None` or an unknown value keeps the
    /// TV order [`Self::get_channels`] returns.
    pub fn sort_channels(&self, channels: &mut [ChannelSummary], sort: Option<&str>) {
        let lower = |s: &str| s.to_lowercase();
        match sort.unwrap_or("") {
            "number" => channels.sort_by_cached_key(|ch| (ch.channel_number.is_none(), ch.channel_number, lower(&ch.name), ch.id.clone())),
            "name" => channels.sort_by_cached_key(|ch| (lower(&ch.name), ch.id.clone())),
            "name_desc" => channels.sort_by_cached_key(|ch| (std::cmp::Reverse(lower(&ch.name)), ch.id.clone())),
            "country" => channels.sort_by_cached_key(|ch| (dial_country(ch.country_code.as_deref()), lower(&ch.name), ch.id.clone())),
            "provider" => channels.sort_by_cached_key(|ch| {
                let provider = ch.original_genre.as_deref().or(ch.genre.as_deref()).unwrap_or("").trim().to_lowercase();
                (provider.is_empty(), provider, lower(&ch.name), ch.id.clone())
            }),
            "edited" => {
                let overrides = self.overrides.lock().unwrap();
                channels.sort_by_cached_key(|ch| {
                    let edited = overrides.get(&ch.id).map(|row| row.updated_at_ms);
                    (edited.is_none(), std::cmp::Reverse(edited.unwrap_or(0)), lower(&ch.name), ch.id.clone())
                });
            }
            _ => {}
        }
    }

    /// Every tab a television draws (country, group, Sports Extra folder), with its size and
    /// whether the household has taken over its order. Sorted the way the editor lists them.
    pub fn tabs(&self) -> Vec<TabSummary> {
        let channels = self.channels.lock().unwrap();
        let overrides = self.overrides.lock().unwrap();
        let mut tabs: HashMap<DialGroup, TabSummary> = HashMap::new();
        let mut names: HashSet<(DialGroup, String)> = HashSet::new();
        for channel in channels.values() {
            let row = overrides.get(&channel.id);
            let effective = effective_channel(channel, row);
            let group = dial_group(&effective);
            let entry = tabs.entry(group.clone()).or_insert_with(|| TabSummary::new(&group));
            // Twins are one row on the page, so one in the count.
            if names.insert((group.clone(), twin_name_key(&effective.name))) {
                entry.count += 1;
                if effective.hidden == Some(true) {
                    entry.hidden += 1;
                }
            }
            if row.is_some_and(|row| row.channel_number.is_some() || row.position.is_some()) {
                entry.managed = true;
            }
        }
        let order = |label: &str| AURELIS_GROUPS.iter().position(|&(_, l)| l == label).unwrap_or(usize::MAX);
        let countries = self.country_order.lock().unwrap().clone();
        let country_rank = |c: &str| countries.iter().position(|x| x == c).unwrap_or(usize::MAX);
        let mut list: Vec<TabSummary> = tabs.into_values().collect();
        list.sort_by(|a, b| country_rank(&a.country).cmp(&country_rank(&b.country))
            .then_with(|| a.country.cmp(&b.country))
            .then_with(|| order(a.label).cmp(&order(b.label)))
            .then_with(|| a.folder.cmp(&b.folder)));
        list
    }

    /// One tab's channels in the order a television draws them. A tab the household has
    /// ordered is exactly what the televisions are sent. An untouched tab is ordered by the
    /// television's own rules as far as the Companion can know them (the country's channel
    /// plan, families with their +1s, the provider's numbers); a television can still differ,
    /// chiefly by lifting the channels it watches most, and the page says so.
    pub fn tab_channels(&self, tab: &TabRef) -> Option<TabView> {
        let group = tab.dial_group()?;
        let channels = self.channels.lock().unwrap();
        let overrides = self.overrides.lock().unwrap();
        let mut managed = false;
        let mut members: Vec<ChannelSummary> = channels.values().filter_map(|channel| {
            let row = overrides.get(&channel.id);
            let effective = effective_channel(channel, row);
            (dial_group(&effective) == group).then(|| {
                managed |= row.is_some_and(|row| row.channel_number.is_some() || row.position.is_some());
                effective
            })
        }).collect();
        if managed {
            members.sort_by_cached_key(display_order_key);
        } else {
            automatic_order(&mut members, &group);
        }
        // Twins collapse into the first of them in the order shown.
        let mut first_of: HashMap<String, String> = HashMap::new();
        let mut twins: HashMap<String, Vec<ChannelSummary>> = HashMap::new();
        let mut listed = Vec::with_capacity(members.len());
        for ch in members {
            let key = twin_name_key(&ch.name);
            match first_of.get(&key) {
                Some(rep) => twins.entry(rep.clone()).or_default().push(ch),
                None => {
                    first_of.insert(key, ch.id.clone());
                    listed.push(ch);
                }
            }
        }
        // "Edited" means something besides its place: a name, group, country, logo or hiding.
        let is_edited = |id: &str| overrides.get(id).is_some_and(|row| {
            let mut details = row.clone();
            details.channel_number = None;
            details.position = None;
            !details.is_empty()
        });
        let edited: HashSet<String> = listed.iter().filter(|ch| {
            is_edited(&ch.id) || twins.get(&ch.id).is_some_and(|t| t.iter().any(|tw| is_edited(&tw.id)))
        }).map(|ch| ch.id.clone()).collect();
        Some(TabView {
            tab: TabSummary { count: listed.len(), hidden: listed.iter().filter(|ch| ch.hidden == Some(true)).count(), managed, ..TabSummary::new(&group) },
            channels: listed,
            edited,
            twins,
        })
    }

    /// Put a tab in exactly this order. Ids the list leaves out keep their current order after
    /// the listed ones. The tab's dial numbers are reused in ascending order (more are added after
    /// the highest when the tab has fewer numbers than channels; a tab with none starts at the
    /// country's usual first number), so dial order and drawing order stay the same thing.
    pub fn set_tab_order(&self, tab: &TabRef, ids: &[String]) -> Result<usize, String> {
        let current = self.tab_channels(tab).ok_or("Unknown group.")?;
        let group = tab.dial_group().ok_or("Unknown group.")?;
        let in_tab: HashSet<&str> = current.channels.iter().map(|ch| ch.id.as_str()).collect();
        let mut seen = HashSet::new();
        let mut order: Vec<String> = Vec::with_capacity(current.channels.len());
        for id in ids {
            if !in_tab.contains(id.as_str()) {
                return Err("That channel is no longer in this group. Reload the page.".into());
            }
            if seen.insert(id.clone()) {
                order.push(id.clone());
            }
        }
        order.extend(current.channels.iter().filter(|ch| !seen.contains(&ch.id)).map(|ch| ch.id.clone()));

        let now = (self.wall)();
        let channels = self.channels.lock().unwrap();
        let mut overrides = self.overrides.lock().unwrap();
        let reserved: HashSet<i32> = overrides.iter().filter(|(id, _)| !channels.contains_key(*id))
            .filter_map(|(_, row)| {
                let (row_group, number) = channel_dial(None, Some(row));
                (row_group == group).then_some(number).flatten()
            }).collect();
        let mut slots: BTreeSet<i32> = current.channels.iter()
            .filter(|ch| current.tab.managed || overrides.get(&ch.id).is_some_and(|row| row.channel_number.is_some()))
            .filter_map(|ch| ch.channel_number).filter(|n| !reserved.contains(n)).collect();
        let mut next = slots.last().map_or(if matches!(group.country.as_str(), "GB" | "IE") { 101 } else { 1 }, |n| i64::from(*n) + 1);
        while slots.len() < order.len() {
            let number = i32::try_from(next).map_err(|_| format!("No more dial numbers in {group}."))?;
            next += 1;
            if !reserved.contains(&number) {
                slots.insert(number);
            }
        }
        let mut updated = overrides.clone();
        let mut changed = 0;
        for (listed, number) in order.iter().zip(slots) {
            // A channel and its twins share one place and one dial number.
            let ids = std::iter::once(listed.clone())
                .chain(current.twins.get(listed).into_iter().flatten().map(|ch| ch.id.clone()));
            for id in ids {
                let base_number = channels.get(&id).and_then(|ch| ch.channel_number);
                let row = updated.entry(id.clone()).or_insert_with(|| ChannelOverride::new(id.clone(), now));
                let before = (row.channel_number.or(base_number), row.position);
                row.channel_number = Some(number);
                row.position = None;
                if before != (Some(number), None) {
                    row.updated_at_ms = now;
                    changed += 1;
                }
            }
        }
        self.persist_overrides(&updated)?;
        *overrides = updated;
        Ok(changed)
    }

    /// Hand a tab back to the televisions' automatic order: its channels lose the dial numbers
    /// and positions the household gave them (everything else they were given stays).
    pub fn release_tab(&self, tab: &TabRef) -> Result<usize, String> {
        let current = self.tab_channels(tab).ok_or("Unknown group.")?;
        let mut overrides = self.overrides.lock().unwrap();
        let mut updated = overrides.clone();
        let mut changed = 0;
        let all = current.channels.iter().chain(current.twins.values().flatten());
        for ch in all {
            if let Some(row) = updated.get_mut(&ch.id) {
                if row.channel_number.is_some() || row.position.is_some() {
                    row.channel_number = None;
                    row.position = None;
                    changed += 1;
                }
                if row.is_empty() {
                    updated.remove(&ch.id);
                }
            }
        }
        if changed > 0 {
            self.persist_overrides(&updated)?;
            *overrides = updated;
        }
        Ok(changed)
    }

    /// Undo every change to a channel except its place in the order (its dial number), so
    /// undoing a rename does not send it to the bottom of an ordered group.
    pub fn reset_details(&self, channel_id: &str) -> Result<bool, String> {
        let mut lock = self.overrides.lock().unwrap();
        let Some(row) = lock.get(channel_id).cloned() else { return Ok(false) };
        let mut kept = ChannelOverride::new(row.channel_id.clone(), (self.wall)());
        kept.channel_number = row.channel_number;
        let mut updated = lock.clone();
        if kept.is_empty() {
            updated.remove(channel_id);
        } else {
            updated.insert(channel_id.to_owned(), kept);
        }
        self.persist_overrides(&updated)?;
        *lock = updated;
        Ok(true)
    }

    /// The other ids of the same channel (same name, same tab): see [`TabView::twins`]. The
    /// Spreadsheet view's saves, resets and bulk changes reach them too.
    pub fn twin_ids_of(&self, id: &str) -> Vec<String> {
        let channels = self.channels.lock().unwrap();
        let overrides = self.overrides.lock().unwrap();
        let mut ids: Vec<String> = twin_ids(&channels, &overrides, id, overrides.get(id)).into_iter().collect();
        ids.sort();
        ids
    }

    /// Each reported channel's own number, as a television sent it, before any override.
    pub fn reported_numbers(&self) -> HashMap<String, Option<i32>> {
        self.channels.lock().unwrap().iter().map(|(id, ch)| (id.clone(), ch.channel_number)).collect()
    }

    pub fn overrides_count(&self) -> usize {
        self.overrides.lock().unwrap().len()
    }

    pub fn channels_count(&self) -> usize {
        self.channels.lock().unwrap().len()
    }

    pub fn channel_display_name(&self, channel_id: &str) -> String {
        let overrides = self.overrides.lock().unwrap();
        if let Some(name) = overrides.get(channel_id).and_then(|o| o.display_name.as_deref()) {
            return name.to_string();
        }
        let channels = self.channels.lock().unwrap();
        channels
            .get(channel_id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| channel_id.to_string())
    }

    pub fn bulk_update(&self, req: BulkOverrideRequest) -> Result<usize, String> {
        let mut unique_ids = Vec::new();
        let mut seen = HashSet::new();
        for raw in req.channel_ids {
            let id = raw.trim().to_string();
            if !id.is_empty() && seen.insert(id.clone()) {
                unique_ids.push(id);
            }
        }
        if unique_ids.is_empty() {
            return Err("Select at least one channel first.".to_string());
        }

        let now = (self.wall)();
        let channels = self.channels.lock().unwrap();
        let mut overrides = self.overrides.lock().unwrap();
        let mut updated = overrides.clone();

        if let BulkAction::SetGroup { sports_extra_group: Some(ref folder), .. } = req.action {
            let folder = folder.trim();
            if !folder.is_empty() {
                let known = channels.values().filter_map(|ch| ch.sports_extra_group.as_deref())
                    .any(|bank| matches_sports_folder(bank, folder))
                    || overrides.values().filter_map(|row| row.sports_extra_group.as_deref())
                        .any(|bank| matches_sports_folder(bank, folder));
                if !known {
                    return Err("Choose a Sports Extra folder reported by an Aurelis TV.".into());
                }
            }
        }

        let mut affected = 0usize;
        for id in &unique_ids {
            let base = channels.get(id);
            match &req.action {
                BulkAction::Reset => {
                    if updated.remove(id).is_some() {
                        affected += 1;
                    }
                }
                BulkAction::Hide => {
                    let row = updated.entry(id.clone()).or_insert_with(|| ChannelOverride::new(id.clone(), now));
                    if row.hidden != Some(true) {
                        row.hidden = Some(true);
                        row.updated_at_ms = now;
                        affected += 1;
                    }
                }
                BulkAction::Unhide => {
                    let base_hidden = base.is_some_and(|ch| ch.hidden == Some(true));
                    if let Some(row) = updated.get_mut(id) {
                        let target = if base_hidden { Some(false) } else { None };
                        if row.hidden != target {
                            row.hidden = target;
                            row.updated_at_ms = now;
                            affected += 1;
                        }
                        if row.is_empty() {
                            updated.remove(id);
                        }
                    } else if base_hidden {
                        let mut row = ChannelOverride::new(id.clone(), now);
                        row.hidden = Some(false);
                        updated.insert(id.clone(), row);
                        affected += 1;
                    }
                }
                BulkAction::ClearNumber => {
                    if let Some(row) = updated.get_mut(id) {
                        if row.channel_number.is_some() || row.position.is_some() {
                            row.channel_number = None;
                            row.position = None;
                            row.updated_at_ms = now;
                            affected += 1;
                        }
                        if row.is_empty() {
                            updated.remove(id);
                        }
                    }
                }
                BulkAction::SetCountry(raw_cc) => {
                    let cc = raw_cc.trim().to_ascii_uppercase();
                    let norm = if cc == "UK" { "GB".to_string() } else { cc };
                    if norm.is_empty() {
                        if let Some(row) = updated.get_mut(id) {
                            if row.country_code.is_some() {
                                row.country_code = None;
                                row.updated_at_ms = now;
                                affected += 1;
                            }
                            if row.is_empty() {
                                updated.remove(id);
                            }
                        }
                    } else {
                        let same_as_base = base.is_some_and(|ch| dial_country(ch.country_code.as_deref()) == norm);
                        let row = updated.entry(id.clone()).or_insert_with(|| ChannelOverride::new(id.clone(), now));
                        let target = if same_as_base { None } else { Some(norm) };
                        if row.country_code != target {
                            row.country_code = target;
                            row.updated_at_ms = now;
                            affected += 1;
                        }
                        if row.is_empty() {
                            updated.remove(id);
                        }
                    }
                }
                BulkAction::SetGroup { category, sports_extra_group } => {
                    let norm_cat = category.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
                    let norm_folder = sports_extra_group.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
                    let target_cat = if norm_folder.is_some() {
                        Some("SPORT_EXTRA".to_string())
                    } else {
                        norm_cat
                    };
                    let row = updated.entry(id.clone()).or_insert_with(|| ChannelOverride::new(id.clone(), now));
                    if row.category != target_cat || row.sports_extra_group != norm_folder {
                        row.category = target_cat;
                        row.sports_extra_group = norm_folder;
                        row.updated_at_ms = now;
                        affected += 1;
                    }
                    if row.is_empty() {
                        updated.remove(id);
                    }
                }
            }
        }

        // Validate that country/group moves did not create duplicate dial numbers within a DialGroup.
        if matches!(req.action, BulkAction::SetCountry(_) | BulkAction::SetGroup { .. }) {
            let mut seen_dials: HashMap<(DialGroup, i32), String> = HashMap::new();
            let all_ids: BTreeSet<&String> = channels.keys().chain(updated.keys()).collect();
            for id in all_ids {
                let (group, num) = channel_dial(channels.get(id), updated.get(id));
                if let Some(number) = num {
                    if let Some(other_id) = seen_dials.insert((group.clone(), number), id.clone()) {
                        let name_a = updated.get(&other_id).and_then(|r| r.display_name.as_deref())
                            .or_else(|| channels.get(&other_id).map(|c| c.name.as_str()))
                            .unwrap_or(&other_id);
                        let name_b = updated.get(id).and_then(|r| r.display_name.as_deref())
                            .or_else(|| channels.get(id).map(|c| c.name.as_str()))
                            .unwrap_or(id);
                        return Err(format!(
                            "Bulk move would assign dial {number} to both {name_a} and {name_b} in {group}. Clear or change one dial number first."
                        ));
                    }
                }
            }
        }

        if affected > 0 {
            self.persist_overrides(&updated)?;
            *overrides = updated;
        }
        Ok(affected)
    }

    pub fn export_overrides_json(&self) -> Result<String, String> {
        let overrides = self.get_overrides();
        let payload = serde_json::json!({
            "version": 1,
            "exportedAtMs": (self.wall)(),
            "overrides": overrides,
        });
        serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())
    }

    pub fn import_overrides_json(&self, raw: &str, replace: bool) -> Result<usize, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("Paste exported overrides JSON to import.".to_string());
        }
        let parsed: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("Invalid JSON: {e}"))?;
        let rows_val = if let Some(arr) = parsed.as_array() {
            serde_json::Value::Array(arr.clone())
        } else if let Some(arr) = parsed.get("overrides").and_then(|v| v.as_array()) {
            serde_json::Value::Array(arr.clone())
        } else {
            return Err("Expected a JSON object with an \"overrides\" array, or a JSON array of overrides.".to_string());
        };
        let incoming: Vec<ChannelOverride> = serde_json::from_value(rows_val)
            .map_err(|e| format!("Could not read overrides list: {e}"))?;

        let now = (self.wall)();
        let channels = self.channels.lock().unwrap();
        let mut overrides = self.overrides.lock().unwrap();
        let mut updated = if replace {
            HashMap::new()
        } else {
            overrides.clone()
        };

        let mut imported = 0usize;
        for mut row in incoming {
            row.channel_id = row.channel_id.trim().to_string();
            if row.channel_id.is_empty() {
                return Err("Every override must have a non-empty channelId.".to_string());
            }
            if let Some(num) = row.channel_number {
                if num <= 0 {
                    return Err(format!("Channel '{}' has an invalid dial number ({num}).", row.channel_id));
                }
            }
            if let Some(ref logo) = row.logo_url {
                let logo = logo.trim();
                if logo.is_empty() {
                    row.logo_url = None;
                } else if !is_valid_logo_url(logo) {
                    return Err(format!("Channel '{}' has an invalid logo URL.", row.channel_id));
                } else {
                    row.logo_url = Some(logo.to_string());
                }
            }
            if let Some(ref cc) = row.country_code {
                let cc = cc.trim().to_ascii_uppercase();
                row.country_code = if cc.is_empty() {
                    None
                } else if cc == "UK" {
                    Some("GB".to_string())
                } else {
                    Some(cc)
                };
            }
            if row.updated_at_ms <= 0 {
                row.updated_at_ms = now;
            }
            if row.is_empty() {
                updated.remove(&row.channel_id);
            } else {
                updated.insert(row.channel_id.clone(), row);
                imported += 1;
            }
        }

        // Check for duplicate dial numbers within the same DialGroup.
        let mut seen_dials: HashMap<(DialGroup, i32), String> = HashMap::new();
        let all_ids: BTreeSet<&String> = channels.keys().chain(updated.keys()).collect();
        for id in all_ids {
            let (group, num) = channel_dial(channels.get(id), updated.get(id));
            if let Some(number) = num {
                if let Some(other_id) = seen_dials.insert((group.clone(), number), id.clone()) {
                    return Err(format!(
                        "Import contains duplicate dial {number} in {group} (channels '{other_id}' and '{id}')."
                    ));
                }
            }
        }

        self.persist_overrides(&updated)?;
        *overrides = updated;
        Ok(imported)
    }

    pub fn clear(&self) -> Result<(), String> {
        {
            let mut o = self.overrides.lock().unwrap();
            o.clear();
            let _ = self.persist_overrides(&o);
        }
        {
            let mut c = self.channels.lock().unwrap();
            c.clear();
            let _ = self.persist_channels(&c);
        }
        let _ = std::fs::remove_file(self.dir.join(OVERRIDES_FILE));
        let _ = std::fs::remove_file(self.dir.join(CHANNELS_FILE));
        let _ = std::fs::remove_dir_all(self.dir.join("arbiter_cache"));
        Ok(())
    }

    fn sanitize_arbiter_key(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.len() < 8 || trimmed.len() > 128 {
            return None;
        }
        if trimmed.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            Some(trimmed.to_string())
        } else {
            None
        }
    }

    pub fn get_arbiter_blob(&self, key: &str) -> Option<Vec<u8>> {
        let clean = Self::sanitize_arbiter_key(key)?;
        let path = self.dir.join("arbiter_cache").join(format!("{clean}.bin"));
        std::fs::read(path).ok()
    }

    pub fn put_arbiter_blob(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        let clean = Self::sanitize_arbiter_key(key).ok_or_else(|| "invalid arbiter key".to_string())?;
        if bytes.is_empty() {
            return Err("empty arbiter payload".to_string());
        }
        let cache_dir = self.dir.join("arbiter_cache");
        std::fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?;
        let file_name = format!("{clean}.bin");
        write_atomic(&cache_dir, &file_name, bytes).map_err(|e| e.to_string())?;

        // Retain only the 3 most recently modified arbiter snapshots.
        if let Ok(entries) = std::fs::read_dir(&cache_dir) {
            let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("bin") {
                        let m = e.metadata().and_then(|md| md.modified()).unwrap_or(std::time::UNIX_EPOCH);
                        Some((m, p))
                    } else {
                        None
                    }
                })
                .collect();
            if files.len() > 3 {
                files.sort_by(|a, b| b.0.cmp(&a.0));
                for (_, old_path) in files.into_iter().skip(3) {
                    let _ = std::fs::remove_file(old_path);
                }
            }
        }
        Ok(())
    }
}

// HTTP Route Handlers

fn json_resp(status: StatusCode, body: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

fn lineup_reply(session: Option<&Session>, status: StatusCode, body: serde_json::Value) -> Response {
    match session {
        Some(session) => session.sealed_json(status, &body),
        None => json_resp(status, body),
    }
}

fn etag_matches(if_none_match: &str, current: &str) -> bool {
    if_none_match.split(',').any(|part| {
        let candidate = part.trim().trim_start_matches("W/").trim_matches('"');
        candidate == "*" || candidate == current
    })
}

pub async fn handle_get_overrides(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    session: Option<Extension<Session>>,
) -> Response {
    let served = state.lineup.served_lineup();
    let etag = served.etag.clone();
    if let Some(dev_id) = headers.get("x-aurelis-device-id").and_then(|v| v.to_str().ok()) {
        let dev_name = headers.get("x-aurelis-device-name").and_then(|v| v.to_str().ok());
        state.record_device_lineup_sync(dev_id, dev_name, &etag);
    }

    let quoted_etag = format!("\"{etag}\"");
    if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        if etag_matches(inm, &etag) {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, &quoted_etag)
                .body(axum::body::Body::empty())
                .unwrap();
        }
    }

    let mut overrides = served.overrides.clone();
    if session.is_some() {
        for row in &mut overrides {
            if let Some(category) = &row.category {
                let groups: Vec<_> = category.split(',').filter_map(aurelis_group_key).collect();
                row.category = (!groups.is_empty()).then(|| groups.join(","));
            }
        }
    }
    let updated_at_ms = overrides.iter().map(|o| o.updated_at_ms).max().unwrap_or(0);
    let mut resp = lineup_reply(
        session.as_ref().map(|Extension(session)| session),
        StatusCode::OK,
        serde_json::json!({
            "overrides": overrides,
            "countryOrder": served.country_order,
            "updatedAtMs": updated_at_ms,
            "etag": etag,
        }),
    );
    if let Ok(val) = header::HeaderValue::from_str(&quoted_etag) {
        resp.headers_mut().insert(header::ETAG, val);
    }
    resp
}

pub async fn handle_save_override(
    State(state): State<Arc<AppState>>,
    session: Option<Extension<Session>>,
    body: axum::body::Bytes,
) -> Response {
    let session = session.as_ref().map(|Extension(session)| session);
    let req: SaveOverrideRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(err) => {
            return lineup_reply(
                session,
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "reason": err.to_string() }),
            );
        }
    };
    let ch_id = req.channel_id.clone();
    match state.lineup.set_override(req) {
        Ok(entry) => {
            state.push_job(&format!("lineup: channel '{}' override saved", ch_id));
            lineup_reply(session, StatusCode::OK, serde_json::json!({ "ok": true, "override": entry }))
        }
        Err(err) => lineup_reply(
            session,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "ok": false, "reason": err }),
        ),
    }
}

pub async fn handle_delete_override(
    State(state): State<Arc<AppState>>,
    session: Option<Extension<Session>>,
    UrlPath(channel_id): UrlPath<String>,
) -> Response {
    let session = session.as_ref().map(|Extension(session)| session);
    match state.lineup.delete_override(&channel_id) {
        Ok(true) => {
            state.push_job(&format!("lineup: channel '{}' override removed", channel_id));
            lineup_reply(session, StatusCode::OK, serde_json::json!({ "ok": true, "deleted": true }))
        }
        Ok(false) => lineup_reply(
            session,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "ok": false, "reason": "override not found" }),
        ),
        Err(err) => lineup_reply(
            session,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "reason": err }),
        ),
    }
}

pub async fn handle_get_channels(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> impl IntoResponse {
    let raw_query = req.uri().query().unwrap_or("");
    let query = parse_channel_query(raw_query);
    let mut channels = state.lineup.get_channels();

    if let Some(c) = query.country.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let target = dial_country(Some(c));
        channels.retain(|ch| dial_country(ch.country_code.as_deref()) == target);
    }

    if let Some(cat) = query.category.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        channels.retain(|ch| matches_category_filter(ch, cat));
    }

    if let Some(q) = query.search.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
        channels.retain(|ch| matches_search_filter(ch, &q));
    }
    if query.edited_only {
        channels.retain(|ch| state.lineup.get_override(&ch.id).is_some());
    }
    if query.hidden_only {
        channels.retain(|ch| ch.hidden == Some(true));
    }
    if query.unnumbered_only {
        channels.retain(|ch| ch.channel_number.is_none());
    }
    state.lineup.sort_channels(&mut channels, query.sort.as_deref());

    json_resp(
        StatusCode::OK,
        serde_json::json!({
            "channels": channels,
            "count": channels.len(),
        }),
    )
}

pub async fn handle_report_channels(
    State(state): State<Arc<AppState>>,
    session: Option<Extension<Session>>,
    body: axum::body::Bytes,
) -> Response {
    let session = session.as_ref().map(|Extension(session)| session);
    let req: ChannelReportRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(err) => {
            return lineup_reply(
                session,
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "reason": err.to_string() }),
            );
        }
    };
    let device_id = req.device_id.clone();
    let device_name = req.device_name.clone();
    let device_label = device_name.as_deref().filter(|name| !name.trim().is_empty())
        .or(device_id.as_deref()).unwrap_or("unknown");
    let dev: String = device_label.chars().filter(|ch| !ch.is_control()).take(80).collect();
    match state.lineup.report_channels_from(req.channels, Some(&dev)) {
        Ok(count) => {
            state.record_device_channel_report(device_id.as_deref(), device_name.as_deref(), count);
            state.push_job(&format!("lineup: device '{}' reported {} channels", dev, count));
            lineup_reply(
                session,
                StatusCode::OK,
                serde_json::json!({ "ok": true, "reported": count, "saved": count, "totalKnown": state.lineup.channels_count() }),
            )
        }
        Err(err) => lineup_reply(
            session,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "ok": false, "reason": err }),
        ),
    }
}

pub async fn handle_get_arbiter(
    State(state): State<Arc<AppState>>,
    UrlPath(key): UrlPath<String>,
    Extension(session): Extension<Session>,
) -> Response {
    match state.lineup.get_arbiter_blob(&key) {
        Some(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/octet-stream")
            .body(axum::body::Body::from(companion_protocol::seal::seal(&bytes, &session.secret)))
            .unwrap(),
        None => session.sealed_reason(StatusCode::NOT_FOUND, "arbiter snapshot not found"),
    }
}

pub async fn handle_put_arbiter(
    State(state): State<Arc<AppState>>,
    UrlPath(key): UrlPath<String>,
    body: axum::body::Bytes,
) -> Response {
    match state.lineup.put_arbiter_blob(&key, &body) {
        Ok(()) => {
            state.push_job(&format!("lineup: stored shared arbiter snapshot ({} bytes)", body.len()));
            json_resp(StatusCode::OK, serde_json::json!({ "ok": true, "bytes": body.len() }))
        }
        Err(err) => json_resp(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "ok": false, "reason": err }),
        ),
    }
}

pub async fn handle_assign_numbers(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    let req: AssignNumbersRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(err) => {
            return json_resp(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "reason": err.to_string() }),
            );
        }
    };
    match state.lineup.assign_channel_numbers(req) {
        Ok(count) => {
            state.push_job(&format!("lineup: assigned numbers to {} channels", count));
            json_resp(StatusCode::OK, serde_json::json!({ "ok": true, "assigned": count }))
        }
        Err(err) => json_resp(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "ok": false, "reason": err }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_test_dir() -> TestDir {
        let p = std::env::temp_dir().join(format!("aurelis-lineup-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&p).expect("temp dir");
        TestDir(p)
    }

    fn test_clock(ms: i64) -> WallClock {
        Arc::new(move || ms)
    }

    #[tokio::test]
    async fn paired_tv_receives_sealed_group_edits_resets_and_deletions() {
        use crate::auth::{b64, unb64};
        use crate::household::Household;
        use crate::state::{test_config, test_state};
        use axum::body::{to_bytes, Body};
        use axum::http::{header, Request};
        use companion_protocol::{proof, seal};
        use serde_json::{json, Value};
        use tower::ServiceExt;

        let dir = temp_test_dir();
        let secret = [9; 32];
        Household::create(&dir.0, secret).unwrap();
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let state = Arc::new(test_state(config));
        let app = crate::router(state.clone());
        let save = |group: &str| {
            let request = Request::builder()
                .method("POST")
                .uri("/ui/save_channel_override")
                .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<std::net::SocketAddr>().unwrap()))
                .body(Body::from(format!("channel_id=bbc_one&display_name=BBC+One+HD&aurelis_group={group}&redirect_to=lineup")))
                .unwrap();
            app.clone().oneshot(request)
        };
        let saved = save("SPORT_EXTRA").await.unwrap();
        assert_eq!(saved.status(), StatusCode::SEE_OTHER);
        let loc = saved.headers()[header::LOCATION].to_str().unwrap();
        assert!(loc.starts_with("../lineup?f="), "unexpected redirect: {loc}");
        assert!(loc.ends_with(&format!("#{}", crate::status_page::row_anchor_id("bbc_one"))));
        assert_eq!(state.lineup.get_override("bbc_one").unwrap().category.as_deref(), Some("SPORT_EXTRA"));

        async fn tv_call(
            app: &axum::Router, secret: &[u8; 32], method: &str, path: &str, payload: Vec<u8>,
        ) -> Vec<u8> {
            let challenge = Request::builder()
                .uri("/v1/challenge")
                .header(crate::VERSION_HEADER, "1")
                .body(Body::empty()).unwrap();
            let reply = app.clone().oneshot(challenge).await.unwrap();
            let challenge: Value = serde_json::from_slice(&to_bytes(reply.into_body(), 4096).await.unwrap()).unwrap();
            let nonce = challenge["nonce"].as_str().unwrap();
            let sealed = if payload.is_empty() { payload } else { seal::seal(&payload, secret) };
            let signature = proof::proof(secret, &unb64(nonce).unwrap(), method, path, &sealed);
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header(crate::VERSION_HEADER, "1")
                .header("x-aurelis-nonce", nonce)
                .header("x-aurelis-proof", b64(&signature))
                .body(Body::from(sealed)).unwrap();
            let reply = app.clone().oneshot(request).await.unwrap();
            assert_eq!(reply.status(), StatusCode::OK);
            assert_eq!(reply.headers()[header::CONTENT_TYPE], "application/octet-stream");
            let bytes = to_bytes(reply.into_body(), 4096).await.unwrap();
            seal::open(&bytes, secret).expect("the household can open the reply")
        }

        let reported = tv_call(&app, &secret, "POST", "/v1/lineup/tv/channels/report", json!({
            "deviceId": "tv-1",
            "deviceName": "Lounge TV",
            "channels": [{"id": "bbc_one", "name": "BBC One", "genre": "UK | Main", "countryCode": "GB"}]
        }).to_string().into_bytes()).await;
        assert_eq!(serde_json::from_slice::<Value>(&reported).unwrap()["saved"], 1);
        assert!(state.page_view().jobs.iter().any(|(_, line)| line == "lineup: device 'Lounge TV' reported 1 channels"));

        let html = crate::status_page::render_lineup(
            &state.page_view(), &state.lineup, true, &ChannelQuery::default(),
        );
        assert!(html.contains("name=\"aurelis_group\""));
        assert!(html.contains("value=\"SPORT_EXTRA\" selected"));
        assert!(html.contains("Provider: UK | Main"));

        let snapshot = b"gzipped lineup bytes";
        state.lineup.put_arbiter_blob("snapshot-key", snapshot).unwrap();
        assert_eq!(tv_call(&app, &secret, "GET", "/v1/lineup/tv/arbiter/snapshot-key", vec![]).await, snapshot);

        let path = "/v1/lineup/tv/overrides";
        for (index, expected) in [
            json!([{"channelId": "bbc_one", "displayName": "BBC One HD", "category": "SPORT_EXTRA"}]),
            json!([{"channelId": "bbc_one", "displayName": "BBC One HD", "category": null}]),
            json!([]),
        ].into_iter().enumerate() {
            let bytes = tv_call(&app, &secret, "GET", path, vec![]).await;
            let decoded: Value = serde_json::from_slice(&bytes).unwrap();
            let rows: Vec<_> = decoded["overrides"].as_array().unwrap().iter().map(|row| {
                json!({"channelId": row["channelId"], "displayName": row["displayName"], "category": row["category"]})
            }).collect();
            assert_eq!(json!(rows), expected);
            if index == 0 {
                assert_eq!(save("").await.unwrap().status(), StatusCode::SEE_OTHER);
            } else if index == 1 {
                state.lineup.delete_override("bbc_one").unwrap();
            }
        }

        let request = Request::builder()
            .uri("/v1/lineup/overrides")
            .body(Body::empty()).unwrap();
        let reply = app.oneshot(request).await.unwrap();
        assert_eq!(reply.status(), StatusCode::OK);
        assert_eq!(reply.headers()[header::CONTENT_TYPE], "application/json");
    }

    #[test]
    fn manual_and_auto_dials_and_position_moves_are_scoped_to_a_country_group() {
        use serde_json::{json, Value};
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));
        store.report_channels(serde_json::from_value(json!([
            {"id":"bbc", "name":"BBC One", "countryCode":"GB", "channelNumber":101, "aurelisCategory":"OTHERS"},
            {"id":"itv", "name":"ITV", "countryCode":"UK", "channelNumber":103, "aurelisCategory":"OTHERS"},
            {"id":"four", "name":"Channel Four", "countryCode":"GB", "aurelisCategory":"OTHERS"},
            {"id":"sports", "name":"Sports", "countryCode":"GB", "aurelisCategory":"SPORT"},
            {"id":"nbc", "name":"NBC", "countryCode":"US", "channelNumber":101, "aurelisCategory":"OTHERS"}
        ])).unwrap()).unwrap();
        let save = |request: Value| store.set_override(serde_json::from_value(request).unwrap());
        let error = save(json!({"channelId":"four", "channelNumber":101})).unwrap_err();
        assert!(error.contains("already assigned to BBC One in GB / Others"));
        assert!(store.get_override("four").is_none());
        save(json!({"channelId":"sports", "channelNumber":101})).unwrap();
        assert!(save(json!({"channelId":"nbc", "countryCode":"GB"})).is_err());

        assert_eq!(store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("GB".into()), group: None, start_number: Some(101), overwrite_existing: None, order: None,
        }).unwrap(), 1);
        assert_eq!(store.get_override("four").unwrap().channel_number, Some(102));

        let moved = save(json!({"channelId":"itv", "moveToPosition":1})).unwrap();
        assert_eq!((moved.position, moved.channel_number), (Some(0), Some(101)));
        let current = store.get_channels();
        let dials: Vec<_> = current.iter().map(|channel| (channel.id.as_str(), channel.channel_number)).collect();
        assert_eq!(dials, [("itv", Some(101)), ("bbc", Some(102)), ("four", Some(103)), ("sports", Some(101)), ("nbc", Some(101))]);
        assert!(save(json!({"channelId":"itv", "moveToPosition":4})).is_err());
        assert_eq!(store.get_override("itv").unwrap(), moved);
    }

    #[test]
    fn sports_extra_folders_have_independent_dials_and_bulk_selection() {
        use serde_json::json;
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));
        store.report_channels(serde_json::from_value(json!([
            {"id":"one", "name":"One", "countryCode":"GB", "channelNumber":101,
             "aurelisCategory":"SPORT_EXTRA", "sportsExtraGroup":"EFL::League One"},
            {"id":"two", "name":"Two", "countryCode":"GB", "channelNumber":101,
             "aurelisCategory":"SPORT_EXTRA", "sportsExtraGroup":"EFL::League Two"},
            {"id":"three", "name":"Three", "countryCode":"GB",
             "aurelisCategory":"SPORT_EXTRA", "sportsExtraGroup":"EFL::League One"},
            {"id":"four", "name":"Four", "countryCode":"GB",
             "aurelisCategory":"SPORT_EXTRA", "sportsExtraGroup":"EFL::League Two"}
        ])).unwrap()).unwrap();

        assert_eq!(store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("GB".into()), group: Some("SPORT_EXTRA::EFL::League One".into()),
            start_number: Some(101), overwrite_existing: None, order: None,
        }).unwrap(), 1);
        let moved = store.set_override(serde_json::from_value(json!({
            "channelId":"three", "moveToPosition":1
        })).unwrap()).unwrap();
        assert_eq!(moved.channel_number, Some(101));
        assert_eq!(store.get_override("one").unwrap().channel_number, Some(102));
        assert!(store.get_override("two").is_none());
        assert!(store.get_override("four").is_none());
    }

    #[test]
    fn custom_logo_and_sports_folder_can_be_set_and_reset_to_provider() {
        use serde_json::{json, Value};
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));
        store.report_channels(serde_json::from_value(json!([
            {"id":"bbc", "name":"BBC", "genre":"General", "logoUrl":"https://example.test/provider.png"},
            {"id":"club", "name":"Club TV", "genre":"Football", "aurelisCategory":"SPORT_EXTRA", "sportsExtraGroup":"EFL::League One"}
        ])).unwrap()).unwrap();
        let save = |request: Value| store.set_override(serde_json::from_value(request).unwrap());
        assert!(save(json!({"channelId":"bbc", "logoUrl":"file:///tmp/logo.png"})).is_err());
        assert!(save(json!({"channelId":"bbc", "sportsExtraGroup":"Unknown bank"})).is_err());
        save(json!({"channelId":"bbc", "logoUrl":"https://example.test/new.png", "sportsExtraGroup":"EFL::League One"})).unwrap();
        let bbc = store.get_channels().into_iter().find(|ch| ch.id == "bbc").unwrap();
        assert_eq!(bbc.logo_url.as_deref(), Some("https://example.test/new.png"));
        assert!(matches_category_filter(&bbc, "SPORT_EXTRA::EFL"));
        assert!(matches_category_filter(&bbc, "AURELIS::SPORT_EXTRA"));
        assert!(!matches_category_filter(&bbc, "AURELIS::SPORT"));

        save(json!({"channelId":"bbc", "logoUrl":"", "category":"", "sportsExtraGroup":""})).unwrap();
        let bbc = store.get_channels().into_iter().find(|ch| ch.id == "bbc").unwrap();
        assert_eq!(bbc.logo_url.as_deref(), Some("https://example.test/provider.png"));
        assert!(!matches_category_filter(&bbc, "SPORT_EXTRA::EFL"));
    }

    #[test]
    fn test_override_crud_and_persistence() {
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));

        assert_eq!(store.get_overrides().len(), 0);

        let req = SaveOverrideRequest {
            channel_id: "bbc_one_london".into(),
            display_name: Some("BBC One HD".into()),
            category: Some("Entertainment".into()),
            country_code: Some("gb".into()),
            position: Some(1),
            channel_number: Some(101),
            epg_id: Some("bbc.one.london".into()),
            hidden: Some(false),
            updated_at_ms: None,
            logo_url: None,
            sports_extra_group: None,
            move_to_position: None,
            clear_channel_number: false,
            row_edit: false,
        };

        let saved = store.set_override(req).unwrap();
        assert_eq!(saved.channel_id, "bbc_one_london");
        assert_eq!(saved.country_code, Some("GB".into()));
        assert_eq!(saved.category, Some("Entertainment".into()));
        assert_eq!(saved.position, Some(1));
        assert_eq!(saved.channel_number, Some(101));
        assert_eq!(store.overrides_count(), 1);

        // Reload store from disk
        let reloaded = LineupStore::new(dir.0.clone(), test_clock(2000));
        let retrieved = reloaded.get_override("bbc_one_london").unwrap();
        assert_eq!(retrieved.display_name, Some("BBC One HD".into()));
        assert_eq!(retrieved.country_code, Some("GB".into()));
        assert_eq!(retrieved.channel_number, Some(101));

        // Delete override
        assert!(reloaded.delete_override("bbc_one_london").unwrap());
        assert_eq!(reloaded.overrides_count(), 0);
        assert!(reloaded.get_override("bbc_one_london").is_none());
    }

    #[test]
    fn test_channels_reporting_and_effective_override() {
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));

        let ch = ChannelSummary {
            id: "ch_sky_sports".into(),
            name: "Sky Sports ME UK FHD".into(),
            original_name: None,
            original_genre: None,
            genre: Some("General Sports".into()),
            country_code: Some("US".into()), // Wrong auto-country
            channel_number: None,
            source_id: Some("src1".into()),
            source_type: Some("xtream".into()),
            logo_url: None,
            hidden: None,
            updated_at_ms: 0,
            aurelis_category: None,
            sports_extra_group: None,
            reported_by: None,
        };

        store.report_channels(vec![ch]).unwrap();
        assert_eq!(store.channels_count(), 1);

        // Before override: effective is what was reported
        let list1 = store.get_channels();
        assert_eq!(list1[0].country_code, Some("US".into()));
        assert_eq!(list1[0].name, "Sky Sports ME UK FHD");

        // Set permanent override to UK and clean name, plus channel number 401
        store.set_override(SaveOverrideRequest {
            channel_id: "ch_sky_sports".into(),
            display_name: Some("Sky Sports Main Event".into()),
            category: Some("Sports".into()),
            country_code: Some("GB".into()),
            position: Some(10),
            channel_number: Some(401),
            epg_id: None,
            hidden: None,
            updated_at_ms: None,
            logo_url: None,
            sports_extra_group: None,
            move_to_position: None,
            clear_channel_number: false,
            row_edit: false,
        }).unwrap();

        // After override: effective has permanent moves applied
        let list2 = store.get_channels();
        assert_eq!(list2[0].country_code, Some("GB".into()));
        assert_eq!(list2[0].genre, Some("Sports".into()));
        assert_eq!(list2[0].name, "Sky Sports Main Event");
        assert_eq!(list2[0].channel_number, Some(401));
        assert_eq!(list2[0].original_name, Some("Sky Sports ME UK FHD".into()));
    }

    #[test]
    fn test_assign_channel_numbers_per_country() {
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));

        let ch1 = ChannelSummary {
            id: "gb_bbc1".into(),
            name: "BBC One".into(),
            original_name: None,
            original_genre: None,
            genre: Some("General".into()),
            country_code: Some("GB".into()),
            channel_number: None,
            source_id: None,
            source_type: None,
            logo_url: None,
            hidden: None,
            updated_at_ms: 0,
            aurelis_category: Some("ENTERTAINMENT".into()),
            sports_extra_group: None,
            reported_by: None,
        };
        let ch2 = ChannelSummary {
            id: "gb_itv1".into(),
            name: "ITV1".into(),
            original_name: None,
            original_genre: None,
            genre: Some("General".into()),
            country_code: Some("GB".into()),
            channel_number: None,
            source_id: None,
            source_type: None,
            logo_url: None,
            hidden: None,
            updated_at_ms: 0,
            aurelis_category: Some("ENTERTAINMENT".into()),
            sports_extra_group: None,
            reported_by: None,
        };
        let ch3 = ChannelSummary {
            id: "us_nbc".into(),
            name: "NBC East".into(),
            original_name: None,
            original_genre: None,
            genre: Some("General".into()),
            country_code: Some("US".into()),
            channel_number: None,
            source_id: None,
            source_type: None,
            logo_url: None,
            hidden: None,
            updated_at_ms: 0,
            aurelis_category: Some("ENTERTAINMENT".into()),
            sports_extra_group: None,
            reported_by: None,
        };

        store.report_channels(vec![ch1, ch2, ch3]).unwrap();

        // Assign GB channels (default start 101)
        let count_gb = store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("GB".into()),
            group: None,
            start_number: None,
            overwrite_existing: None, order: None,
        }).unwrap();
        assert_eq!(count_gb, 2);

        // Assign US channels (start 1)
        let count_us = store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("US".into()),
            group: None,
            start_number: Some(1),
            overwrite_existing: None, order: None,
        }).unwrap();
        assert_eq!(count_us, 1);

        let channels = store.get_channels();
        let bbc = channels.iter().find(|c| c.id == "gb_bbc1").unwrap();
        let itv = channels.iter().find(|c| c.id == "gb_itv1").unwrap();
        let nbc = channels.iter().find(|c| c.id == "us_nbc").unwrap();

        assert_eq!(bbc.channel_number, Some(101));
        assert_eq!(itv.channel_number, Some(102));
        assert_eq!(nbc.channel_number, Some(1));
    }

    #[tokio::test]
    async fn row_edit_unhides_clears_fields_and_skips_unchanged_defaults() {
        use crate::state::{test_config, test_state};
        use axum::body::Body;
        use axum::http::Request;
        use serde_json::json;
        use tower::ServiceExt;

        let dir = temp_test_dir();
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let state = Arc::new(test_state(config));
        state.lineup.report_channels(serde_json::from_value(json!([
            {"id":"bbc", "name":"BBC One", "countryCode":"GB", "channelNumber":101, "aurelisCategory":"ENTERTAINMENT"},
            {"id":"itv", "name":"ITV1", "countryCode":"UK", "aurelisCategory":"ENTERTAINMENT"}
        ])).unwrap()).unwrap();
        let app = crate::router(state.clone());

        let post = |body: &str| {
            let req = Request::builder()
                .method("POST")
                .uri("/ui/save_channel_override")
                .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<std::net::SocketAddr>().unwrap()))
                .body(Body::from(body.to_owned()))
                .unwrap();
            app.clone().oneshot(req)
        };

        // Saving only a dial number change on a row must not stamp display_name or country_code.
        let r = post("channel_id=bbc&row_edit=1&channel_number=105&display_name=BBC+One&country_code=GB&aurelis_group=&logo_url=&redirect_to=lineup").await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let ov = state.lineup.get_override("bbc").unwrap();
        assert_eq!(ov.channel_number, Some(105));
        assert_eq!(ov.display_name, None);
        assert_eq!(ov.country_code, None);
        assert_eq!(ov.hidden, None);

        // Checking Hide saves hidden=Some(true); unchecking Hide in a row edit clears it.
        let r = post("channel_id=bbc&row_edit=1&channel_number=105&display_name=BBC+One&country_code=GB&aurelis_group=&logo_url=&hidden=true&redirect_to=lineup").await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(state.lineup.get_override("bbc").unwrap().hidden, Some(true));

        let r = post("channel_id=bbc&row_edit=1&channel_number=105&display_name=BBC+One&country_code=GB&aurelis_group=&logo_url=&redirect_to=lineup").await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(state.lineup.get_override("bbc").unwrap().hidden, None);

        // Clearing channel_number back to blank removes the override row when nothing else differs.
        let r = post("channel_id=bbc&row_edit=1&channel_number=&display_name=BBC+One&country_code=GB&aurelis_group=&logo_url=&redirect_to=lineup").await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert!(state.lineup.get_override("bbc").is_none());

        // Non-positive or invalid dial numbers return 400 with a clear reason.
        let r = post("channel_id=bbc&row_edit=1&channel_number=0&display_name=BBC+One&country_code=GB&aurelis_group=&logo_url=&redirect_to=lineup").await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // UK and GB both match the GB country filter in the UI and API.
        let html = crate::status_page::render_lineup(
            &state.page_view(), &state.lineup, true, &ChannelQuery { country: Some("GB".into()), ..Default::default() },
        );
        assert!(html.contains("ITV1") && html.contains("BBC One"));
        assert!(html.contains("name=\"row_edit__0\" value=\"1\""));
        assert!(html.contains("action=\"ui/save_channel_rows\"") && html.contains("name=\"save_all\""));
    }

    #[test]
    fn parent_sports_folder_assign_and_overwrite_clears_stale_positions() {
        use serde_json::json;
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));
        store.report_channels(serde_json::from_value(json!([
            {"id":"l1_a", "name":"Alpha", "countryCode":"GB", "aurelisCategory":"SPORT_EXTRA", "sportsExtraGroup":"EFL::League One"},
            {"id":"l1_b", "name":"Beta", "countryCode":"GB", "aurelisCategory":"SPORT_EXTRA", "sportsExtraGroup":"EFL::League One"},
            {"id":"l2_a", "name":"Gamma", "countryCode":"GB", "aurelisCategory":"SPORT_EXTRA", "sportsExtraGroup":"EFL::League Two"},
            {"id":"other", "name":"Sky News", "countryCode":"GB", "aurelisCategory":"NEWS"}
        ])).unwrap()).unwrap();

        // Parent folder "SPORT_EXTRA::EFL" matches both EFL sub-folders, numbering each independently.
        assert_eq!(store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("GB".into()), group: Some("SPORT_EXTRA::EFL".into()),
            start_number: Some(101), overwrite_existing: None, order: None,
        }).unwrap(), 3);
        assert_eq!(store.get_override("l1_a").unwrap().channel_number, Some(101));
        assert_eq!(store.get_override("l1_b").unwrap().channel_number, Some(102));
        assert_eq!(store.get_override("l2_a").unwrap().channel_number, Some(101));
        assert!(store.get_override("other").is_none());

        // Moving sets position; overwrite auto-assign clears stale manual positions.
        store.set_override(serde_json::from_value(json!({"channelId":"l1_b", "moveToPosition":1})).unwrap()).unwrap();
        assert_eq!(store.get_override("l1_b").unwrap().position, Some(0));
        assert_eq!(store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("GB".into()), group: Some("SPORT_EXTRA::EFL::League One".into()),
            start_number: Some(201), overwrite_existing: Some(true), order: None,
        }).unwrap(), 2);
        // The default order keeps the move: Beta stays first and is renumbered 201.
        assert_eq!(store.get_override("l1_b").unwrap().channel_number, Some(201));
        assert_eq!(store.get_override("l1_b").unwrap().position, None);
        assert_eq!(store.get_override("l1_a").unwrap().channel_number, Some(202));
        assert_eq!(store.get_override("l1_a").unwrap().position, None);
        // Name A to Z re-sorts the group.
        assert_eq!(store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("GB".into()), group: Some("SPORT_EXTRA::EFL::League One".into()),
            start_number: Some(201), overwrite_existing: Some(true), order: Some("name".into()),
        }).unwrap(), 2);
        assert_eq!(store.get_override("l1_a").unwrap().channel_number, Some(201));
        assert_eq!(store.get_override("l1_b").unwrap().channel_number, Some(202));

        // Non-positive start number is rejected.
        assert!(store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("GB".into()), group: None, start_number: Some(0), overwrite_existing: None, order: None,
        }).is_err());
    }

    #[tokio::test]
    async fn bulk_actions_json_export_import_csrf_and_browser_flash_banners() {
        use axum::body::{to_bytes, Body};
        use axum::http::Request;
        use serde_json::{json, Value};
        use tower::ServiceExt;
        use crate::state::{test_config, test_state};

        let dir = temp_test_dir();
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let state = Arc::new(test_state(config));
        state.lineup.report_channels(serde_json::from_value(json!([
            {"id":"ch1", "name":"BBC One", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT", "channelNumber":101, "logoUrl":"https://cdn.example/bbc1.png"},
            {"id":"ch2", "name":"BBC Two", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT", "channelNumber":102},
            {"id":"ch3", "name":"ITV1", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"}
        ])).unwrap()).unwrap();
        let app = crate::router(state.clone());

        let post = |uri: &'static str, body: String, extra_headers: Vec<(&'static str, &'static str)>| {
            let mut b = Request::builder()
                .method("POST")
                .uri(uri)
                .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<std::net::SocketAddr>().unwrap()));
            for (k, v) in extra_headers {
                b = b.header(k, v);
            }
            app.clone().oneshot(b.body(Body::from(body)).unwrap())
        };

        // Cross-site POSTs are refused with 403 on all lineup mutation endpoints.
        for uri in ["/ui/save_channel_override", "/ui/save_channel_rows", "/ui/delete_channel_override", "/ui/assign_numbers", "/ui/bulk_channel_override", "/ui/import_overrides"] {
            let r = post(uri, "channel_id=ch1".into(), vec![("host", "ha.local:8765"), ("origin", "http://evil.example")]).await.unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{uri} must enforce same_origin");
        }

        // Quick reorder ▲/▼ button moves ch2 to position 1 via quick_move.
        let r = post(
            "/ui/save_channel_override",
            "channel_id=ch2&row_edit=1&quick_move=1&display_name=BBC+Two&country_code=GB&channel_number=102&aurelis_group=&logo_url=&redirect_to=lineup".into(),
            vec![("accept", "text/html")],
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(state.lineup.get_override("ch2").unwrap().channel_number, Some(101));
        assert_eq!(state.lineup.get_override("ch1").unwrap().channel_number, Some(102));

        // Browser validation error redirects back to /lineup with an inline error flash banner.
        let err_resp = post(
            "/ui/save_channel_override",
            "channel_id=ch3&row_edit=1&display_name=ITV1&country_code=GB&channel_number=101&aurelis_group=&logo_url=&redirect_to=lineup%3Fview%3Dtable".into(),
            vec![("accept", "text/html")],
        ).await.unwrap();
        assert_eq!(err_resp.status(), StatusCode::SEE_OTHER);
        let loc = err_resp.headers()[header::LOCATION].to_str().unwrap();
        let query = loc.strip_prefix("../lineup?").unwrap();
        let page = app.clone().oneshot(
            Request::builder()
                .uri(format!("/lineup?{query}"))
                .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<std::net::SocketAddr>().unwrap()))
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        let html = String::from_utf8(to_bytes(page.into_body(), 1024 * 1024).await.unwrap().to_vec()).unwrap();
        assert!(html.contains("class=\"notice err\""), "expected inline error banner");
        assert!(html.contains("Dial 101 is already assigned"));
        assert!(html.contains("https://cdn.example/bbc1.png"), "logo thumbnail should render");

        // Bulk hide ch1 + ch3 in a single POST.
        let bulk = post(
            "/ui/bulk_channel_override",
            "bulk_action=hide&channel_ids=ch1&channel_ids=ch3&redirect_to=lineup".into(),
            vec![("accept", "text/html")],
        ).await.unwrap();
        assert_eq!(bulk.status(), StatusCode::SEE_OTHER);
        assert_eq!(state.lineup.get_override("ch1").unwrap().hidden, Some(true));
        assert_eq!(state.lineup.get_override("ch3").unwrap().hidden, Some(true));

        // Bulk clear dial number on ch3, then verify hidden_only / unnumbered_only / dial-number search.
        let clear = post(
            "/ui/bulk_channel_override",
            "bulk_action=clear_number&channel_ids=ch3&redirect_to=lineup".into(),
            vec![],
        ).await.unwrap();
        assert_eq!(clear.status(), StatusCode::SEE_OTHER);
        let hidden_html = crate::status_page::render_lineup(
            &state.page_view(), &state.lineup, true, &ChannelQuery { hidden_only: true, ..Default::default() },
        );
        assert!(hidden_html.contains("BBC One") && hidden_html.contains("ITV1") && !hidden_html.contains("BBC Two"));
        let unnum_html = crate::status_page::render_lineup(
            &state.page_view(), &state.lineup, true, &ChannelQuery { unnumbered_only: true, ..Default::default() },
        );
        assert!(unnum_html.contains("ITV1") && !unnum_html.contains("BBC Two"));
        let dial_search = crate::status_page::render_lineup(
            &state.page_view(), &state.lineup, true, &ChannelQuery { search: Some("101".into()), ..Default::default() },
        );
        assert!(dial_search.contains("BBC Two") && !dial_search.contains("ITV1"));

        // Export JSON -> reset all -> Import JSON restores everything identically.
        let export = app.clone().oneshot(
            Request::builder()
                .uri("/ui/export_overrides")
                .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<std::net::SocketAddr>().unwrap()))
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(export.status(), StatusCode::OK);
        let exported_json = String::from_utf8(to_bytes(export.into_body(), 1024 * 1024).await.unwrap().to_vec()).unwrap();
        let parsed: Value = serde_json::from_str(&exported_json).unwrap();
        assert_eq!(parsed["overrides"].as_array().unwrap().len(), 3);

        // Bulk reset clears overrides.
        let reset = post(
            "/ui/bulk_channel_override",
            "bulk_action=reset&channel_ids=ch1&channel_ids=ch2&channel_ids=ch3&redirect_to=lineup".into(),
            vec![],
        ).await.unwrap();
        assert_eq!(reset.status(), StatusCode::SEE_OTHER);
        assert_eq!(state.lineup.overrides_count(), 0);

        // Import back in replace mode.
        let encoded_json: String = exported_json.bytes().map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        }).collect();
        let import = post(
            "/ui/import_overrides",
            format!("import_mode=replace&redirect_to=lineup&overrides_json={encoded_json}"),
            vec![],
        ).await.unwrap();
        assert_eq!(import.status(), StatusCode::SEE_OTHER);
        assert_eq!(state.lineup.overrides_count(), 3);
        assert_eq!(state.lineup.get_override("ch2").unwrap().channel_number, Some(101));
    }

    #[tokio::test]
    async fn etag_304_conditional_fetch_and_device_sync_status() {
        use axum::body::{to_bytes, Body};
        use axum::http::Request;
        use tower::ServiceExt;
        use crate::state::{test_config, test_state};

        let dir = temp_test_dir();
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let state = Arc::new(test_state(config));
        let app = crate::router(state.clone());

        // First fetch returns 200 + ETag and records device as Up to date.
        let resp1 = app.clone().oneshot(
            Request::builder()
                .uri("/v1/lineup/overrides")
                .header("x-aurelis-device-id", "abcdef0123456789")
                .header("x-aurelis-device-name", "Lounge Box")
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(resp1.status(), StatusCode::OK);
        let etag1 = resp1.headers()[header::ETAG].to_str().unwrap().to_string();
        assert!(etag1.starts_with("\"v1-"));

        // Second fetch with matching If-None-Match returns 304 Not Modified with empty body.
        let resp2 = app.clone().oneshot(
            Request::builder()
                .uri("/v1/lineup/overrides")
                .header(header::IF_NONE_MATCH, &etag1)
                .header("x-aurelis-device-id", "abcdef0123456789")
                .header("x-aurelis-device-name", "Lounge Box")
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::NOT_MODIFIED);
        assert!(to_bytes(resp2.into_body(), 1024).await.unwrap().is_empty());

        let status_html = crate::status_page::render(&state.page_view(), true);
        assert!(status_html.contains("Lounge Box") && status_html.contains("Up to date"));

        // Saving an override rotates the ETag, turning the device badge to Pending sync until it fetches again.
        state.lineup.set_override(serde_json::from_value(serde_json::json!({"channelId":"ch1", "hidden":true})).unwrap()).unwrap();
        let status_html_after = crate::status_page::render(&state.page_view(), true);
        assert!(status_html_after.contains("Pending sync"));

        let resp3 = app.clone().oneshot(
            Request::builder()
                .uri("/v1/lineup/overrides")
                .header(header::IF_NONE_MATCH, &etag1)
                .header("x-aurelis-device-id", "abcdef0123456789")
                .header("x-aurelis-device-name", "Lounge Box")
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(resp3.status(), StatusCode::OK);
        assert_ne!(resp3.headers()[header::ETAG].to_str().unwrap(), etag1);
    }

    #[test]
    fn lineup_page_uses_shared_sports_datalist_and_windowed_pager() {
        use crate::state::{test_config, test_state};

        let dir = temp_test_dir();
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let state = test_state(config);
        // 1,200 channels across 40 Sports Extra folders -> 12 pages.
        let mut channels = Vec::new();
        for i in 0..1200 {
            channels.push(ChannelSummary {
                id: format!("ch_{i:04}"),
                name: format!("Channel {i:04}"),
                original_name: None,
                genre: Some("UK | Sports".into()),
                original_genre: None,
                country_code: Some("GB".into()),
                channel_number: None,
                source_id: None,
                source_type: None,
                logo_url: None,
                hidden: None,
                updated_at_ms: 1000,
                aurelis_category: Some(if i < 40 { "SPORT_EXTRA" } else { "ENTERTAINMENT" }.into()),
                sports_extra_group: (i < 40).then(|| format!("League::Div{i:02}")),
                reported_by: None,
            });
        }
        state.lineup.report_channels(channels).unwrap();
        let html = crate::status_page::render_lineup(
            &state.page_view(), &state.lineup, true, &ChannelQuery { page: Some(6), ..Default::default() },
        );
        assert!(html.contains("<datalist id=\"sports-folder-list\">"));
        // Row dropdowns no longer repeat all 40 Sports Extra folders 100 times (was >4000 occurrences).
        let sport_opt_count = html.matches("value=\"SPORT_EXTRA::").count();
        assert!(sport_opt_count < 300, "too many repeated SPORT_EXTRA options: {sport_opt_count}");
        // Windowed pager renders ellipsis instead of dumping all 12 page links.
        assert!(html.contains("class=\"pager-ellipsis\""));
    }

    fn served_position(store: &LineupStore, id: &str) -> Option<i32> {
        store.served_lineup().overrides.iter().find(|row| row.channel_id == id).and_then(|row| row.position)
    }

    #[test]
    fn televisions_are_sent_the_editor_order_of_every_numbered_group() {
        use serde_json::json;
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));
        store.report_channels(serde_json::from_value(json!([
            {"id":"disc", "name":"Discovery", "countryCode":"GB", "aurelisCategory":"DOCUMENTARIES"},
            {"id":"anim", "name":"Animal Planet", "countryCode":"GB", "aurelisCategory":"DOCUMENTARIES"},
            {"id":"wild", "name":"WildEarth", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"news", "name":"Sky News", "countryCode":"GB", "aurelisCategory":"NEWS"}
        ])).unwrap()).unwrap();
        // Nobody has numbered anything: the televisions keep their own order.
        assert!(store.served_lineup().overrides.is_empty());

        // An A-Z run numbers Animal Planet first; the televisions are told the same.
        store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("GB".into()), group: Some("DOCUMENTARIES".into()),
            start_number: Some(600), overwrite_existing: None, order: Some("name".into()),
        }).unwrap();
        assert_eq!(served_position(&store, "anim"), Some(0));
        assert_eq!(served_position(&store, "disc"), Some(1));
        let before = store.overrides_etag();

        // Typing a dial number reorders the televisions too (it used to leave a stale position).
        store.set_override(serde_json::from_value(json!({"channelId":"disc", "channelNumber":590})).unwrap()).unwrap();
        assert_eq!(served_position(&store, "disc"), Some(0));
        assert_eq!(served_position(&store, "anim"), Some(1));
        assert_ne!(store.overrides_etag(), before);

        // A channel moved into a numbered group is sent a position after the numbered ones,
        // not left without one; an untouched group gets nothing.
        store.set_override(serde_json::from_value(json!({"channelId":"wild", "category":"DOCUMENTARIES"})).unwrap()).unwrap();
        assert_eq!(served_position(&store, "wild"), Some(2));
        assert_eq!(served_position(&store, "news"), None);
        // The stored rows (and so the JSON backup) keep no derived positions.
        assert_eq!(store.get_override("wild").unwrap().position, None);
    }

    #[test]
    fn an_unchanged_save_writes_nothing_and_the_list_sorts_on_request() {
        use serde_json::json;
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), test_clock(1000));
        store.report_channels(serde_json::from_value(json!([
            {"id":"b", "name":"beta", "countryCode":"GB", "aurelisCategory":"NEWS", "channelNumber": 3},
            {"id":"a", "name":"Alpha", "countryCode":"GB", "aurelisCategory":"NEWS", "channelNumber": 9}
        ])).unwrap()).unwrap();
        let (_, changed) = store.save_override(serde_json::from_value(json!({"channelId":"a", "displayName":"Alpha 1"})).unwrap()).unwrap();
        assert!(changed);
        let etag = store.overrides_etag();
        let (_, changed) = store.save_override(serde_json::from_value(json!({"channelId":"a", "displayName":"Alpha 1", "updatedAtMs": 5000})).unwrap()).unwrap();
        assert!(!changed);
        assert_eq!(store.overrides_etag(), etag);
        // A row that never had an override and is saved as shown writes nothing either.
        let (_, changed) = store.save_override(serde_json::from_value(json!({"channelId":"b", "displayName":"beta", "rowEdit": true, "hidden": false})).unwrap()).unwrap();
        assert!(!changed);
        assert!(store.get_override("b").is_none());

        let names = |sort: Option<&str>| {
            let mut channels = store.get_channels();
            store.sort_channels(&mut channels, sort);
            channels.into_iter().map(|ch| ch.name).collect::<Vec<_>>()
        };
        assert_eq!(names(None), ["beta", "Alpha 1"]);
        assert_eq!(names(Some("name")), ["Alpha 1", "beta"]);
        assert_eq!(names(Some("name_desc")), ["beta", "Alpha 1"]);
        assert_eq!(names(Some("edited")), ["Alpha 1", "beta"]);
        assert_eq!(parse_channel_query("sort=name_desc").sort.as_deref(), Some("name_desc"));
        assert_eq!(parse_channel_query("sort=nonsense").sort, None);
        // Provider numbering order: beta (3) then Alpha (9).
        store.assign_channel_numbers(AssignNumbersRequest {
            country_code: Some("GB".into()), group: None, start_number: Some(101),
            overwrite_existing: Some(true), order: Some("provider".into()),
        }).unwrap();
        assert_eq!(store.get_override("b").unwrap().channel_number, Some(101));
        assert_eq!(store.get_override("a").unwrap().channel_number, Some(102));
        assert!(store.assign_channel_numbers(AssignNumbersRequest {
            country_code: None, group: None, start_number: None, overwrite_existing: None, order: Some("bogus".into()),
        }).is_err());
    }

    #[tokio::test]
    async fn save_all_writes_only_changed_rows_and_names_what_failed() {
        use axum::body::Body;
        use axum::http::Request;
        use serde_json::json;
        use tower::ServiceExt;
        use crate::state::{test_config, test_state};

        let dir = temp_test_dir();
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let state = Arc::new(test_state(config));
        state.lineup.report_channels(serde_json::from_value(json!([
            {"id":"ch1", "name":"BBC One", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT", "channelNumber":101},
            {"id":"ch2", "name":"BBC Two", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT", "channelNumber":102},
            {"id":"ch3", "name":"ITV1", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT", "channelNumber":103}
        ])).unwrap()).unwrap();
        let app = crate::router(state.clone());
        let post = |body: String| {
            app.clone().oneshot(Request::builder().method("POST").uri("/ui/save_channel_rows")
                .header("accept", "text/html")
                .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<std::net::SocketAddr>().unwrap()))
                .body(Body::from(body)).unwrap())
        };
        let row = |i: usize, id: &str, name: &str, number: &str| format!(
            "&channel_id__{i}={id}&row_edit__{i}=1&channel_number__{i}={number}&display_name__{i}={name}&country_code__{i}=GB&aurelis_group__{i}=&logo_url__{i}="
        );
        let flash = |r: &axum::http::Response<Body>| {
            let location = r.headers()["location"].to_str().unwrap().to_owned();
            let token = location.split("f=").nth(1).unwrap().split('#').next().unwrap().to_owned();
            state.get_flash(&token).map(|notice| notice.message).unwrap_or_default()
        };

        // Rename one row, renumber ITV1 onto BBC Two's dial while BBC Two moves off it: the chain
        // lands whatever the row order, and the untouched row is not written.
        let body = format!("redirect_to=lineup&rows=3&save_all=1{}{}{}",
            row(0, "ch1", "BBC+One+HD", "101"), row(1, "ch3", "ITV1", "102"), row(2, "ch2", "BBC+Two", "110"));
        let r = post(body).await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(flash(&r), "Saved 3 channels.");
        assert_eq!(state.lineup.get_override("ch1").unwrap().display_name.as_deref(), Some("BBC One HD"));
        assert_eq!(state.lineup.get_override("ch3").unwrap().channel_number, Some(102));
        assert_eq!(state.lineup.get_override("ch2").unwrap().channel_number, Some(110));

        // Saving the page again as shown changes nothing.
        let body = format!("redirect_to=lineup&rows=3&save_all=1{}{}{}",
            row(0, "ch1", "BBC+One+HD", "101"), row(1, "ch3", "ITV1", "102"), row(2, "ch2", "BBC+Two", "110"));
        assert_eq!(flash(&post(body).await.unwrap()), "No changes saved.");

        // A clash is reported by name, the rest still saves.
        let body = format!("redirect_to=lineup&rows=2&save_all=1{}{}",
            row(0, "ch1", "BBC+One", "101"), row(1, "ch2", "BBC+Two", "102"));
        let message = flash(&post(body).await.unwrap());
        assert!(message.starts_with("Saved BBC One. Not saved — BBC Two: Dial 102 is already assigned to ITV1"), "{message}");

        // One row's Save button saves only that row, and the move buttons carry their row.
        let body = format!("redirect_to=lineup&rows=2&save_row=1{}{}",
            row(0, "ch1", "Renamed+but+not+saved", "101"), row(1, "ch2", "BBC+2", "110"));
        assert_eq!(flash(&post(body).await.unwrap()), "Saved BBC 2.");
        assert!(state.lineup.get_override("ch1").is_none());
        let body = format!("redirect_to=lineup&rows=1&quick_move=0:1{}", row(0, "ch2", "BBC+2", "110"));
        assert_eq!(flash(&post(body).await.unwrap()), "Moved BBC 2 to position 1.");

        // A full page posts every row: 100 rows with long logo URLs, far past the 8 KB UI limit.
        let mut body = "redirect_to=lineup&rows=100&save_row=0".to_string();
        let itv = state.lineup.get_channels().into_iter().find(|ch| ch.id == "ch3").unwrap();
        body.push_str(&row(0, "ch3", "ITV1", &itv.channel_number.unwrap().to_string()));
        for i in 1..100 {
            body.push_str(&row(i, &format!("unknown_{i:03}"), "Some+Channel+Name+Here", ""));
            body.push_str(&format!("&logo_url__{i}=https%3A%2F%2Fcdn.example.invalid%2Flogos%2F{}.png", "x".repeat(120)));
        }
        assert!(body.len() > 32 * 1024, "{}", body.len());
        let r = post(body).await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(flash(&r), "No changes to save for ITV1.");
    }

    #[test]
    fn channels_no_television_reports_for_three_weeks_leave_the_editor() {
        use serde_json::json;
        use std::sync::atomic::{AtomicI64, Ordering};
        let now = Arc::new(AtomicI64::new(1_000_000_000_000));
        let clock = now.clone();
        let dir = temp_test_dir();
        let store = LineupStore::new(dir.0.clone(), Arc::new(move || clock.load(Ordering::SeqCst)));
        store.report_channels(serde_json::from_value(json!([
            {"id":"old", "name":"Old Event 3", "countryCode":"GB", "aurelisCategory":"SPORT"},
            {"id":"kept", "name":"BBC One", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"}
        ])).unwrap()).unwrap();
        store.set_override(serde_json::from_value(json!({"channelId":"old", "displayName":"Renamed"})).unwrap()).unwrap();
        now.fetch_add(STALE_CHANNEL_MS + 1, Ordering::SeqCst);
        store.report_channels(serde_json::from_value(json!([
            {"id":"kept", "name":"BBC One", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"}
        ])).unwrap()).unwrap();
        let ids: Vec<String> = store.get_channels().into_iter().map(|ch| ch.id).collect();
        assert_eq!(ids, ["kept"]);
        assert!(store.get_override("old").is_some(), "its override waits for the channel to return");
    }

    /// 26 Nov 2026: the Spreadsheet view listed every id of a channel, and Regional had been
    /// folded into Entertainment though a television gives it a tab of its own.
    #[tokio::test]
    async fn the_spreadsheet_folds_twins_and_regional_keeps_its_own_group() {
        use axum::body::Body;
        use axum::http::Request;
        use serde_json::json;
        use tower::ServiceExt;
        use crate::state::{test_config, test_state};
        let dir = temp_test_dir();
        let mut config = test_config();
        config.data_dir = dir.0.clone();
        let state = Arc::new(test_state(config));
        state.lineup.report_channels(serde_json::from_value(json!([
            {"id":"a", "name":"BBC One", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"b", "name":"BBC ONE", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"w", "name":"BBC One Wales", "countryCode":"GB", "aurelisCategory":"LOCAL"},
            {"id":"r", "name":"BBC One Yorkshire", "countryCode":"GB", "aurelisCategory":"REGIONAL"}
        ])).unwrap()).unwrap();
        assert_eq!(dial_group(&state.lineup.get_channels().into_iter().find(|c| c.id == "w").unwrap()).category, "Entertainment");
        assert_eq!(dial_group(&state.lineup.get_channels().into_iter().find(|c| c.id == "r").unwrap()).category, "Regional");

        let html = crate::status_page::render_lineup(&state.page_view(), &state.lineup, true, &ChannelQuery::default());
        assert_eq!(html.matches("name=\"channel_ids\"").count(), 3, "BBC One's two ids are one row");
        assert!(html.contains(">2 ids</span>"));

        // Hiding the folded row hides both ids.
        let app = crate::router(state.clone());
        let lead = if html.contains("value=\"a\" aria-label=\"Select BBC One\"") || html.contains("value=\"a\"") { "a" } else { "b" };
        let r = app.oneshot(Request::builder().method("POST").uri("/ui/bulk_channel_override")
            .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<std::net::SocketAddr>().unwrap()))
            .body(Body::from(format!("bulk_action=hide&channel_ids={lead}&redirect_to=lineup"))).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(state.lineup.get_override("a").and_then(|o| o.hidden), Some(true));
        assert_eq!(state.lineup.get_override("b").and_then(|o| o.hidden), Some(true));
    }
}

