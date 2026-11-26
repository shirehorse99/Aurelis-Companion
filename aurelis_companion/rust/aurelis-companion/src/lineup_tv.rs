//! The channel editor as the televisions draw it: pick a country and a group, see that tab's
//! channels in the order a television shows them, and reorder or edit them in place.
//!
//! One way of working (owner, 25 Nov 2026: "the two differing systems of saving and altering are
//! really really confusing, list the channels as you see them on tv, allow much simpler ways of
//! ordering"): every change saves the moment it is made, and the page redraws from what the
//! Companion then holds, so what is on screen is always what the televisions are sent. The older
//! table stays reachable as the "Spreadsheet view" for bulk edits and backups.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::lineup::{self, LineupStore, SaveOverrideRequest, TabRef, TabView};
use crate::state::AppState;
use crate::status_page::{escape, PRODUCT_NAME};

fn reply(status: StatusCode, body: Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

fn refuse(reason: impl Into<String>) -> Response {
    reply(StatusCode::BAD_REQUEST, json!({ "ok": false, "reason": reason.into() }))
}

/// The guards every write here shares: the lineup editor's peer gate and the same-origin check.
fn guard(peer: Option<ConnectInfo<SocketAddr>>, headers: &HeaderMap) -> Result<(), Response> {
    if !crate::lineup_editor_request(peer, headers) {
        return Err(reply(StatusCode::FORBIDDEN, json!({ "ok": false, "reason": "Editing is only allowed from Home Assistant or this machine." })));
    }
    crate::check_same_origin(headers)
}

fn channel_json(ch: &lineup::ChannelSummary, view: &TabView) -> Value {
    let twins = view.twins.get(&ch.id).map(Vec::as_slice).unwrap_or(&[]);
    // Kept apart by one television's own merge, rather than two televisions keying it differently.
    let same_box = ch.reported_by.is_some() && twins.iter().any(|t| t.reported_by == ch.reported_by);
    json!({
        "by": ch.reported_by,
        // Local channels are listed inside Entertainment, as the televisions draw them.
        "kind": Some(lineup::resolve_aurelis_category(ch)).filter(|k| *k == "Local"),
        "twins": twins.iter().map(|t| json!({ "id": t.id, "by": t.reported_by, "provider": t.original_genre.as_deref().or(t.genre.as_deref()) })).collect::<Vec<_>>(),
        "sameBox": same_box,
        "id": ch.id,
        "name": ch.name,
        "originalName": ch.original_name,
        "number": ch.channel_number,
        "hidden": ch.hidden == Some(true),
        "logo": ch.logo_url.as_deref().filter(|url| lineup::is_valid_logo_url(url)),
        "edited": view.edited.contains(&ch.id),
        "provider": ch.original_genre.as_deref().or(ch.genre.as_deref()),
        "country": ch.country_code,
    })
}

/// The page's whole picture of one tab, plus the tab list so counts stay current.
fn tab_json(store: &LineupStore, tab: &TabRef) -> Value {
    let tabs = store.tabs();
    match store.tab_channels(tab) {
        Some(view) => json!({
            "ok": true,
            "household": store.household_groups(),
            "countryOrder": store.country_order(),
            "tab": view.tab,
            "channels": view.channels.iter().map(|ch| channel_json(ch, &view)).collect::<Vec<_>>(),
            "tabs": tabs,
        }),
        None => json!({ "ok": true, "household": store.household_groups(), "countryOrder": store.country_order(), "tab": null, "channels": [], "tabs": tabs }),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountryOrderRequest {
    #[serde(flatten)]
    tab: TabRef,
    order: Vec<String>,
}

/// The household's order of countries; every television puts its own home country first.
pub async fn post_country_order(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = guard(peer, &headers) { return resp; }
    let req: CountryOrderRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return refuse(format!("Unreadable request: {e}")),
    };
    match state.lineup.set_country_order(&req.order) {
        Ok(order) => saved(&state, &req.tab, format!("Country order saved: {}. Each television still puts its own home country first.", order.join(", "))),
        Err(e) => refuse(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupsRequest {
    #[serde(flatten)]
    tab: TabRef,
    action: String,
    /// A category key: "HC_3", or a built-in one ("NEWS") for its folders.
    #[serde(default)]
    category: Option<String>,
    /// The folder acted on, for a folder rename or delete; the parent, for a new folder.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    index: Option<usize>,
}

/// The household's own categories and folders: add, rename, move, delete. Built-in categories
/// are refused for rename, move and delete; their folders are not.
pub async fn post_groups(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = guard(peer, &headers) { return resp; }
    let req: GroupsRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return refuse(format!("Unreadable request: {e}")),
    };
    let store = &state.lineup;
    let name = req.name.as_deref().unwrap_or("");
    let category = req.category.as_deref().map(str::trim).unwrap_or("");
    let path = req.path.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let mut tab = req.tab.clone();
    let result: Result<String, String> = match req.action.as_str() {
        "addCategory" => store.add_household_category(name).map(|c| {
            tab = TabRef { country: tab.country.clone(), group: c.key.clone(), folder: None };
            format!("Added {}. Move channels into it from a channel's ⋯ menu; every television shows it once it has a channel.", c.name)
        }),
        "renameCategory" => store.rename_household_category(category, name).map(|c| format!("Renamed to {}.", c.name)),
        "moveCategory" => store.move_household_category(category, req.index.unwrap_or(0)).map(|()| "Category order saved.".to_owned()),
        "deleteCategory" => store.delete_household_category(category).map(|change| {
            if tab.group == category {
                tab = TabRef { country: tab.country.clone(), group: "ENTERTAINMENT".into(), folder: None };
            }
            change.message
        }),
        "addFolder" => store.add_household_folder(category, path, name).map(|made| {
            format!("Added the folder {}. Move channels into it from a channel's ⋯ menu.", made.replace("::", " / "))
        }),
        "renameFolder" => match path {
            None => Err("Choose a folder.".into()),
            Some(path) => store.rename_household_folder(category, path, name).map(|change| {
                if tab.group == category && tab.folder.as_deref().is_some_and(|f| crate::household_groups::folder_contains(path, f)) {
                    tab.folder = None;
                }
                change.message
            }),
        },
        "deleteFolder" => match path {
            None => Err("Choose a folder.".into()),
            Some(path) => store.delete_household_folder(category, path).map(|change| {
                if tab.group == category && tab.folder.as_deref().is_some_and(|f| crate::household_groups::folder_contains(path, f)) {
                    tab.folder = None;
                }
                change.message
            }),
        },
        other => Err(format!("Unknown action '{other}'.")),
    };
    match result {
        Ok(message) => saved(&state, &tab, message),
        Err(e) => refuse(e),
    }
}

fn saved(state: &AppState, tab: &TabRef, message: String) -> Response {
    state.push_job(&format!("lineup: {message}"));
    let mut body = tab_json(&state.lineup, tab);
    body["message"] = Value::String(message);
    reply(StatusCode::OK, body)
}

pub async fn get_tab(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let q = page_query(req.uri().query().unwrap_or(""));
    let (Some(country), Some(group)) = (q.country, q.group) else { return refuse("Name a country and a group.") };
    reply(StatusCode::OK, tab_json(&state.lineup, &TabRef { country, group, folder: q.folder }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRequest {
    #[serde(flatten)]
    tab: TabRef,
    ids: Vec<String>,
}

/// Drag and drop, ▲ ▼, to top, to bottom and "move to #": the page sends the tab's whole new order.
pub async fn post_order(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = guard(peer, &headers) { return resp; }
    let req: OrderRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return refuse(format!("Unreadable request: {e}")),
    };
    match state.lineup.set_tab_order(&req.tab, &req.ids) {
        Ok(_) => saved(&state, &req.tab, "Order saved. Televisions follow within about 10 seconds.".into()),
        Err(e) => refuse(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SortRequest {
    #[serde(flatten)]
    tab: TabRef,
    /// `name`, `provider`, `automatic` (fix the automatic order as the household's), or
    /// `release` (hand the tab back to each television's automatic order).
    sort: String,
}

pub async fn post_sort(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = guard(peer, &headers) { return resp; }
    let req: SortRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return refuse(format!("Unreadable request: {e}")),
    };
    if req.sort == "release" {
        return match state.lineup.release_tab(&req.tab) {
            Ok(_) => saved(&state, &req.tab, "Handed back to automatic order. Each television sorts this group itself again.".into()),
            Err(e) => refuse(e),
        };
    }
    let Some(view) = state.lineup.tab_channels(&req.tab) else { return refuse("Unknown group.") };
    let mut channels = view.channels;
    let message = match req.sort.as_str() {
        "name" => {
            channels.sort_by_cached_key(|ch| (ch.name.to_lowercase(), ch.id.clone()));
            "Sorted A to Z."
        }
        "provider" => {
            // The provider's own numbers, as a television reported them, before any of ours.
            let base = state.lineup.reported_numbers();
            channels.sort_by_cached_key(|ch| {
                let n = base.get(&ch.id).copied().flatten();
                (n.is_none(), n, ch.name.to_lowercase(), ch.id.clone())
            });
            "Sorted by the provider's channel numbers."
        }
        "automatic" => "Kept the automatic order as yours; move channels from here.",
        other => return refuse(format!("Unknown sort '{other}'.")),
    };
    let ids: Vec<String> = channels.into_iter().map(|ch| ch.id).collect();
    match state.lineup.set_tab_order(&req.tab, &ids) {
        Ok(_) => saved(&state, &req.tab, message.into()),
        Err(e) => refuse(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditRequest {
    /// The tab the page is showing, so the answer redraws it.
    #[serde(flatten)]
    tab: TabRef,
    channel_id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    hidden: Option<bool>,
    /// An Aurelis group key, `SPORT_EXTRA::<folder>`, or "" for automatic.
    #[serde(default)]
    move_to_group: Option<String>,
    #[serde(default)]
    country_code: Option<String>,
    #[serde(default)]
    logo_url: Option<String>,
    /// A dial number, or "" to clear it.
    #[serde(default)]
    number: Option<String>,
    #[serde(default)]
    reset: bool,
}

/// One field of one channel, saved at once.
pub async fn post_edit(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = guard(peer, &headers) { return resp; }
    let req: EditRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return refuse(format!("Unreadable request: {e}")),
    };
    let name = state.lineup.channel_display_name(&req.channel_id);
    if req.reset {
        for twin in twins_of(&state.lineup, &req.tab, &req.channel_id) {
            if let Err(e) = state.lineup.reset_details(&twin) {
                return refuse(e);
            }
        }
        return match state.lineup.reset_details(&req.channel_id) {
            Ok(true) => saved(&state, &req.tab, format!("{name} is back to the provider's details, in the same place.")),
            Ok(false) => saved(&state, &req.tab, format!("{name} had nothing changed.")),
            Err(e) => refuse(e),
        };
    }
    let mut save = SaveOverrideRequest {
        channel_id: req.channel_id.clone(),
        display_name: req.display_name.clone(),
        category: None,
        country_code: req.country_code.clone(),
        position: None,
        channel_number: None,
        epg_id: None,
        hidden: req.hidden,
        updated_at_ms: None,
        logo_url: req.logo_url.clone(),
        sports_extra_group: None,
        move_to_position: None,
        clear_channel_number: false,
        row_edit: false,
    };
    let mut message = format!("Saved {name}.");
    if let Some(group) = req.move_to_group.as_deref().map(str::trim) {
        if group.is_empty() {
            save.category = Some(String::new());
            save.sports_extra_group = Some(String::new());
            message = format!("{name} is back in its automatic group.");
        } else if let Some(folder) = group.strip_prefix("SPORT_EXTRA::") {
            save.sports_extra_group = Some(folder.to_owned());
            message = format!("Moved {name} to Sports Extra / {}.", folder.replace("::", " / "));
        } else {
            // "NEWS", "HC_3", or either with a folder: "NEWS::Rolling", "HC_3::Cartoons::Classic".
            let (key, folder) = group.split_once("::").map_or((group, ""), |(k, f)| (k, f));
            let household = state.lineup.household_groups();
            let key = if crate::household_groups::is_household_key(key) {
                key.to_owned()
            } else if let Some(key) = lineup::aurelis_group_key(key) {
                key.to_owned()
            } else {
                return refuse("Choose a group from the list.");
            };
            let Some(label) = household.label(&key) else { return refuse("That category has been deleted. Choose another.") };
            if !folder.is_empty() && !household.folders_of(&key).iter().any(|f| f == folder) {
                return refuse("That folder has been deleted or renamed. Choose another.");
            }
            save.category = Some(key);
            save.sports_extra_group = Some(folder.to_owned());
            message = if folder.is_empty() {
                format!("Moved {name} to {label}.")
            } else {
                format!("Moved {name} to {label} / {}.", folder.replace("::", " / "))
            };
        }
    }
    if let Some(cc) = save.country_code.as_deref() {
        let cc = cc.trim();
        if !cc.is_empty() && !(cc.len() == 2 && cc.chars().all(|c| c.is_ascii_alphabetic())) {
            return refuse("Enter a two-letter country code, such as GB.");
        }
        if !cc.is_empty() {
            message = format!("Moved {name} to {}.", lineup::dial_country(Some(cc)));
        }
    }
    if let Some(raw) = req.number.as_deref().map(str::trim) {
        if raw.is_empty() {
            save.clear_channel_number = true;
        } else {
            match raw.parse::<i32>() {
                Ok(n) if n > 0 => save.channel_number = Some(n),
                _ => return refuse("Enter a positive dial number, or leave it blank."),
            }
        }
        // A number given in a tab still on automatic order would sort it by numbers it does not
        // have yet; fix the order on screen first so only this channel moves.
        if let Some(view) = state.lineup.tab_channels(&req.tab) {
            if !view.tab.managed {
                let ids: Vec<String> = view.channels.iter().map(|ch| ch.id.clone()).collect();
                if let Err(e) = state.lineup.set_tab_order(&req.tab, &ids) {
                    return refuse(e);
                }
            }
        }
    }
    let twins = twins_of(&state.lineup, &req.tab, &req.channel_id);
    let first = match state.lineup.save_override(save.clone()) {
        Ok((_, changed)) => changed,
        Err(e) => return refuse(e),
    };
    let (changed, failed) = fan_out(&state.lineup, &twins, |id| {
        let mut twin = save.clone();
        twin.channel_id = id.to_owned();
        state.lineup.save_override(twin).map(|(_, changed)| changed)
    });
    if !failed.is_empty() {
        return refuse(format!("{message} Not copied to its twin: {}", failed.join("; ")));
    }
    if first || changed > 0 {
        let note = if twins.is_empty() { String::new() } else { format!(" (and its {} twin{})", twins.len(), if twins.len() == 1 { "" } else { "s" }) };
        saved(&state, &req.tab, format!("{}{note}", message.trim_end_matches('.')) + ".")
    } else {
        saved(&state, &req.tab, format!("{name} already looked like that."))
    }
}

/// The other ids of a listed channel in this tab (see `TabView::twins`).
/// Found from any of the set's ids: which id leads the row can change after a reorder.
fn twins_of(store: &LineupStore, tab: &TabRef, id: &str) -> Vec<String> {
    let Some(view) = store.tab_channels(tab) else { return Vec::new() };
    for (lead, twins) in &view.twins {
        if lead == id || twins.iter().any(|t| t.id == id) {
            return std::iter::once(lead.clone()).chain(twins.iter().map(|t| t.id.clone()))
                .filter(|other| other != id).collect();
        }
    }
    Vec::new()
}

/// Make the same change to each twin, counting changes and naming failures.
fn fan_out(store: &LineupStore, twins: &[String], mut apply: impl FnMut(&str) -> Result<bool, String>) -> (usize, Vec<String>) {
    let mut changed = 0;
    let mut failed = Vec::new();
    for id in twins {
        match apply(id) {
            Ok(true) => changed += 1,
            Ok(false) => {}
            Err(e) => failed.push(format!("{}: {e}", store.channel_display_name(id))),
        }
    }
    (changed, failed)
}

/// `?country=&group=&folder=` on the page address chooses the first tab shown.
#[derive(Deserialize, Default)]
pub struct PageQuery {
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
}

pub fn render(view: &crate::state::PageView, store: &LineupStore, editor: bool, query: &PageQuery) -> String {
    let tabs = store.tabs();
    let pick = query.country.as_deref().map(|c| lineup::dial_country(Some(c)));
    let group = query.group.as_deref().and_then(lineup::aurelis_group_key);
    let first = tabs.iter()
        .find(|t| pick.as_deref().is_none_or(|c| t.country == c) && group.is_none_or(|g| t.group == g)
            && query.folder.as_deref().is_none_or(|f| t.folder.as_deref() == Some(f)))
        .or_else(|| tabs.iter().find(|t| t.country == "GB" && t.group == "ENTERTAINMENT"))
        .or(tabs.first());
    let initial = match first {
        Some(t) => tab_json(store, &TabRef { country: t.country.clone(), group: t.group.to_string(), folder: t.folder.clone() }),
        None => json!({ "ok": true, "tab": null, "channels": [], "tabs": tabs }),
    };
    let groups: Vec<Value> = lineup::AURELIS_GROUPS.iter()
        .filter(|(key, _)| *key != "SPORT_EXTRA")
        .map(|(key, label)| json!({ "key": key, "label": label })).collect();
    let mut folders: Vec<String> = tabs.iter().filter_map(|t| t.folder.clone()).collect();
    folders.sort();
    folders.dedup();
    let boot = json!({ "editor": editor, "data": initial, "groups": groups, "folders": folders });

    let mut sync = String::new();
    let mut devices: Vec<_> = view.devices.iter().collect();
    devices.sort_by_key(|d| std::cmp::Reverse(d.last_seen_ms));
    for d in devices {
        let up = d.synced_lineup_etag.as_deref() == Some(view.lineup_etag.as_str());
        sync.push_str(&format!(
            "<div class=\"sync-row\"><span>{}</span><span class=\"badge {}\">{}</span></div>",
            escape(&d.name), if up { "done" } else { "pending" }, if up { "Up to date" } else { "Updating…" },
        ));
    }
    if sync.is_empty() {
        sync.push_str("<span class=\"job-meta\">No television has asked for the lineup yet.</span>");
    }

    let mut h = String::new();
    h.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    h.push_str(&format!("<title>Channels - {PRODUCT_NAME}</title><style>"));
    h.push_str(include_str!("status_page.css"));
    h.push_str(include_str!("lineup_tv.css"));
    h.push_str("</style></head><body><main class=\"tv-page\">");
    h.push_str(&format!("<h1>{PRODUCT_NAME}</h1>"));
    h.push_str("<nav class=\"nav-tabs\"><a href=\"./\" class=\"nav-tab\">System Status</a><a href=\"lineup\" class=\"nav-tab active\">Channels</a><a href=\"lineup?view=table\" class=\"nav-tab\">Spreadsheet view</a></nav>");
    h.push_str("<div class=\"tv-layout\">");
    // Left: where you are (country and group), and the household settings that rarely change.
    h.push_str("<aside class=\"tv-side\">");
    h.push_str("<label class=\"side-field\">Country <select id=\"country\"></select></label>");
    h.push_str("<label class=\"side-field group-select\">Group <select id=\"group-select\"></select></label>");
    h.push_str("<nav id=\"groups\" class=\"group-list\" aria-label=\"Groups\"></nav>");
    h.push_str("<details class=\"side-box\"><summary>Country order</summary><div id=\"country-order\" class=\"country-order\"></div></details>");
    h.push_str(&format!("<details class=\"side-box\"><summary>Televisions</summary><div class=\"sync-list\">{sync}</div></details>"));
    h.push_str("</aside>");
    // Right: the group as a television draws it.
    h.push_str("<section class=\"tv-main\">");
    h.push_str("<div class=\"tv-head\"><div id=\"tab-status\" class=\"tab-status\"></div>");
    if editor {
        h.push_str("<div id=\"tab-tools\" class=\"tab-tools\"><input id=\"find\" type=\"search\" placeholder=\"Find in this group\" aria-label=\"Find in this group\"><select id=\"group-action\" aria-label=\"Re-order this group\"><option value=\"\">Re-order this group…</option><option value=\"name\">Sort A to Z</option><option value=\"provider\">Sort by provider number</option><option value=\"release\">Hand back to automatic order</option></select></div>");
    } else {
        h.push_str("<p class=\"job-meta\">Read only: open this page from Home Assistant to make changes.</p>");
    }
    h.push_str("</div>");
    h.push_str("<ol id=\"list\" class=\"tv-list\"></ol>");
    if editor {
        h.push_str("<p class=\"job-meta hint\">Drag a channel or use ▲ ▼. ⋯ (or the name) opens a channel: rename, hide, move group, dial number, to top or bottom. Every change saves at once.</p>");
    }
    h.push_str("</section></div><div id=\"toast\" class=\"toast\" role=\"status\" aria-live=\"polite\"></div></main>");
    h.push_str("<script>const BOOT=");
    // Inside <script>, a "</" in data would end the element early.
    h.push_str(&script_json(&boot));
    h.push_str(";</script><script>");
    h.push_str(include_str!("lineup_tv.js"));
    h.push_str("</script></body></html>");
    h
}

/// JSON that can sit inside a `<script>` element: no `<` (so no `</script>` or `<!--`), no `>` or
/// `&`, and the two line separators escaped too. Channel names come from providers and televisions,
/// so they are data, not markup, whatever they contain.
pub fn script_json(value: &Value) -> String {
    value
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod script_json_tests {
    use super::*;

    #[test]
    fn a_channel_name_cannot_end_the_script_element() {
        let hostile = json!({ "name": "</script><script>alert(1)</script><!--\u{2028}x&y" });
        let out = script_json(&hostile);
        assert!(!out.contains('<') && !out.contains('>') && !out.contains('&'), "{out}");
        assert!(!out.contains('\u{2028}'));
        // It is still the same data once a script reads it back.
        let back: Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(back, hostile);
    }
}

pub fn page_query(raw: &str) -> PageQuery {
    let map: HashMap<String, String> = raw.split('&').filter_map(|part| part.split_once('='))
        .map(|(k, v)| (k.to_string(), lineup::urlencoding_decode(v))).collect();
    PageQuery {
        country: map.get("country").cloned().filter(|v| !v.is_empty()),
        group: map.get("group").cloned().filter(|v| !v.is_empty()),
        folder: map.get("folder").cloned().filter(|v| !v.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lineup::twin_name_key;
    use axum::body::{to_bytes, Body};
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn tab(group: &str) -> TabRef {
        TabRef { country: "GB".into(), group: group.into(), folder: None }
    }

    fn names(store: &LineupStore, group: &str) -> Vec<String> {
        store.tab_channels(&tab(group)).unwrap().channels.into_iter().map(|ch| ch.name).collect()
    }

    fn state_with(channels: Value) -> Arc<AppState> {
        let dir = crate::household::TempDir::new();
        let mut config = crate::state::test_config();
        config.data_dir = dir.0.clone();
        std::mem::forget(dir);
        let state = Arc::new(crate::state::test_state(config));
        state.lineup.report_channels(serde_json::from_value(channels).unwrap()).unwrap();
        state
    }

    fn uk_entertainment() -> Value {
        json!([
            {"id":"itv2", "name":"ITV2", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"bbc1", "name":"BBC One", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"bbc1p", "name":"BBC One +1", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"alibi", "name":"Alibi", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"news", "name":"Sky News", "countryCode":"GB", "aurelisCategory":"NEWS"}
        ])
    }

    #[test]
    fn a_tab_reads_in_the_televisions_order_and_reorders_in_one_step() {
        let state = state_with(uk_entertainment());
        let store = &state.lineup;
        // Untouched: the UK plan puts BBC One first, its +1 straight after, not A to Z.
        let auto = names(store, "ENTERTAINMENT");
        assert_eq!(&auto[..2], ["BBC One", "BBC One +1"], "{auto:?}");
        assert!(!store.tab_channels(&tab("ENTERTAINMENT")).unwrap().tab.managed);
        let tabs = store.tabs();
        assert_eq!(tabs.iter().map(|t| (t.label.as_str(), t.count)).collect::<Vec<_>>(), [("Entertainment", 4), ("News", 1)]);

        // One order for the whole tab: numbered from 101, and the televisions are sent exactly it.
        store.set_tab_order(&tab("ENTERTAINMENT"), &["alibi".into(), "bbc1".into()]).unwrap();
        let view = store.tab_channels(&tab("ENTERTAINMENT")).unwrap();
        assert!(view.tab.managed);
        let order: Vec<_> = view.channels.iter().map(|ch| (ch.name.as_str(), ch.channel_number)).collect();
        assert_eq!(order[0], ("Alibi", Some(101)));
        assert_eq!(order[1], ("BBC One", Some(102)));
        assert_eq!(order.len(), 4);
        let served = store.served_lineup();
        let pos = |id: &str| served.overrides.iter().find(|r| r.channel_id == id).and_then(|r| r.position);
        assert_eq!((pos("alibi"), pos("bbc1")), (Some(0), Some(1)));
        assert_eq!(pos("news"), None, "an untouched tab is left to the televisions");

        // Moving one channel keeps the same dial numbers, just reassigned.
        store.set_tab_order(&tab("ENTERTAINMENT"), &["bbc1".into(), "alibi".into()]).unwrap();
        assert_eq!(&names(store, "ENTERTAINMENT")[..2], ["BBC One", "Alibi"]);
        assert_eq!(store.get_override("bbc1").unwrap().channel_number, Some(101));

        // Undoing a rename keeps the channel's place.
        store.set_override(serde_json::from_value(json!({"channelId":"bbc1", "displayName":"BBC 1"})).unwrap()).unwrap();
        assert!(store.tab_channels(&tab("ENTERTAINMENT")).unwrap().edited.contains("bbc1"));
        store.reset_details("bbc1").unwrap();
        assert_eq!(names(store, "ENTERTAINMENT")[0], "BBC One");
        assert_eq!(store.get_override("bbc1").unwrap().channel_number, Some(101));
        assert!(!store.tab_channels(&tab("ENTERTAINMENT")).unwrap().edited.contains("bbc1"));

        // A channel from another tab cannot be slipped into this one.
        assert!(store.set_tab_order(&tab("ENTERTAINMENT"), &["news".into()]).is_err());

        // Handing back clears the numbers and the tab is automatic again.
        store.release_tab(&tab("ENTERTAINMENT")).unwrap();
        assert!(!store.tab_channels(&tab("ENTERTAINMENT")).unwrap().tab.managed);
        assert!(store.get_override("bbc1").is_none());
    }

    #[tokio::test]
    async fn the_page_and_its_instant_saves() {
        let state = state_with(uk_entertainment());
        let app = crate::router(state.clone());
        let send = |method: &str, uri: &str, body: Option<Value>, origin: Option<&str>| {
            let mut b = HttpRequest::builder().method(method).uri(uri).header("host", "ha.local:8765")
                .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<SocketAddr>().unwrap()));
            if let Some(o) = origin { b = b.header("origin", o); }
            if body.is_some() { b = b.header("content-type", "application/json"); }
            app.clone().oneshot(b.body(body.map_or(Body::empty(), |v| Body::from(v.to_string()))).unwrap())
        };
        let read = |r: Response| async move {
            let status = r.status();
            (status, String::from_utf8(to_bytes(r.into_body(), 4 << 20).await.unwrap().to_vec()).unwrap())
        };

        // /lineup is the television view; the old table is the spreadsheet view.
        let (status, html) = read(send("GET", "/lineup", None, None).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("id=\"list\"") && html.contains("const BOOT=") && html.contains("Sort A to Z"));
        let (_, table) = read(send("GET", "/lineup?view=table", None, None).await.unwrap()).await;
        assert!(table.contains("Spreadsheet view") && table.contains("channel-table"));

        // Every write refuses another site.
        for path in ["/ui/tab_order", "/ui/tab_sort", "/ui/channel_edit"] {
            let r = send("POST", path, Some(json!({})), Some("http://evil.example")).await.unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{path}");
        }

        // A number typed in an automatic tab moves only that channel: the rest keep their place.
        let before = names(&state.lineup, "ENTERTAINMENT");
        let (status, body) = read(send("POST", "/ui/channel_edit", Some(json!({
            "country":"GB", "group":"ENTERTAINMENT", "channelId":"alibi", "number":"150"
        })), Some("http://ha.local:8765")).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let answer: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(answer["tab"]["managed"], true);
        let after: Vec<String> = answer["channels"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_owned()).collect();
        let without = |v: &[String]| v.iter().filter(|n| *n != "Alibi").cloned().collect::<Vec<_>>();
        assert_eq!(without(&before), without(&after));
        assert_eq!(after.last().map(String::as_str), Some("Alibi"));

        // Hide and move group, each saved on its own and answered with the redrawn tab.
        let r = send("POST", "/ui/channel_edit", Some(json!({"country":"GB","group":"ENTERTAINMENT","channelId":"itv2","hidden":true})), Some("http://ha.local:8765")).await.unwrap();
        let (_, body) = read(r).await;
        assert!(body.contains("\"hidden\":true"), "{body}");
        let r = send("POST", "/ui/channel_edit", Some(json!({"country":"GB","group":"ENTERTAINMENT","channelId":"bbc1p","moveToGroup":"NEWS"})), Some("http://ha.local:8765")).await.unwrap();
        let (_, body) = read(r).await;
        assert!(body.contains("Moved BBC One +1 to News."), "{body}");
        assert!(names(&state.lineup, "NEWS").contains(&"BBC One +1".to_string()));

        // A bad value is refused with its reason, not swallowed.
        let (status, body) = read(send("POST", "/ui/channel_edit", Some(json!({"country":"GB","group":"ENTERTAINMENT","channelId":"bbc1","number":"0"})), Some("http://ha.local:8765")).await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("positive dial number"));

        // Whole-tab sorts.
        let (_, body) = read(send("POST", "/ui/tab_sort", Some(json!({"country":"GB","group":"ENTERTAINMENT","sort":"name"})), Some("http://ha.local:8765")).await.unwrap()).await;
        assert!(body.contains("Sorted A to Z."), "{body}");
        assert_eq!(names(&state.lineup, "ENTERTAINMENT"), ["Alibi", "BBC One", "ITV2"]);
        let (_, body) = read(send("POST", "/ui/tab_order", Some(json!({"country":"GB","group":"ENTERTAINMENT","ids":["itv2"]})), Some("http://ha.local:8765")).await.unwrap()).await;
        assert!(body.contains("Order saved."), "{body}");
        assert_eq!(names(&state.lineup, "ENTERTAINMENT")[0], "ITV2");
        let (_, body) = read(send("POST", "/ui/tab_sort", Some(json!({"country":"GB","group":"ENTERTAINMENT","sort":"release"})), Some("http://ha.local:8765")).await.unwrap()).await;
        assert!(body.contains("\"managed\":false"), "{body}");

        // The tab read used when switching tabs.
        let (status, body) = read(send("GET", "/ui/tab?country=GB&group=NEWS", None, None).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Sky News"));
    }

    #[tokio::test]
    async fn twins_are_one_row_and_every_change_reaches_both_ids() {
        let state = state_with(json!([
            {"id":"bbc1", "name":"BBC One", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"itv1", "name":"ITV1", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"}
        ]));
        // Another box keys BBC One differently ("BBC ONE"), and keeps a second ITV1 of its own.
        state.lineup.report_channels_from(serde_json::from_value(json!([
            {"id":"bbc1-other", "name":"BBC ONE", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"itv1-a", "name":"ITV1", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"itv1-b", "name":"ITV 1", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"}
        ])).unwrap(), Some("Box R 4K Plus")).unwrap();
        let view = state.lineup.tab_channels(&tab("ENTERTAINMENT")).unwrap();
        assert_eq!(view.channels.len(), 2, "two channels, five ids");
        assert_eq!(state.lineup.tabs()[0].count, 2);
        let bbc = view.channels.iter().find(|ch| twin_name_key(&ch.name) == "bbcone").unwrap().id.clone();
        assert_eq!(view.twins[&bbc].len(), 1);
        let itv = view.channels.iter().find(|ch| twin_name_key(&ch.name) == "itv1").unwrap();
        let itv_json = channel_json(itv, &view);
        assert_eq!(itv_json["twins"].as_array().unwrap().len(), 2);

        // Ordering numbers both BBC Ones alike: no dial clash between twins.
        state.lineup.set_tab_order(&tab("ENTERTAINMENT"), &[bbc.clone()]).unwrap();
        assert_eq!(state.lineup.get_override("bbc1").unwrap().channel_number, Some(101));
        assert_eq!(state.lineup.get_override("bbc1-other").unwrap().channel_number, Some(101));

        // A rename through the page reaches the twin.
        let app = crate::router(state.clone());
        let r = app.oneshot(HttpRequest::builder().method("POST").uri("/ui/channel_edit")
            .header("host", "ha.local:8765").header("origin", "http://ha.local:8765").header("content-type", "application/json")
            .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<SocketAddr>().unwrap()))
            .body(Body::from(json!({"country":"GB","group":"ENTERTAINMENT","channelId": bbc, "hidden": true}).to_string())).unwrap()).await.unwrap();
        let body = String::from_utf8(to_bytes(r.into_body(), 4 << 20).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("and its 1 twin"), "{body}");
        assert_eq!(state.lineup.get_override("bbc1").unwrap().hidden, Some(true));
        assert_eq!(state.lineup.get_override("bbc1-other").unwrap().hidden, Some(true));
    }

    #[test]
    fn a_double_on_one_box_is_flagged_as_that_boxs_merge() {
        let state = state_with(json!([]));
        state.lineup.report_channels_from(serde_json::from_value(json!([
            {"id":"a", "name":"BBC Earth", "countryCode":"CA", "aurelisCategory":"DOCUMENTARIES"},
            {"id":"b", "name":"BBC Earth", "countryCode":"CA", "aurelisCategory":"DOCUMENTARIES"}
        ])).unwrap(), Some(".246")).unwrap();
        let t = TabRef { country: "CA".into(), group: "DOCUMENTARIES".into(), folder: None };
        let view = state.lineup.tab_channels(&t).unwrap();
        assert_eq!(channel_json(&view.channels[0], &view)["sameBox"], true);
    }

    #[test]
    fn page_data_cannot_close_its_script() {
        let state = state_with(json!([{"id":"x", "name":"</script><b>", "countryCode":"GB", "aurelisCategory":"NEWS"}]));
        let html = render(&state.page_view(), &state.lineup, true, &PageQuery::default());
        assert!(!html.contains("</script><b>"));
    }

    #[tokio::test]
    async fn the_household_country_order_is_saved_served_and_orders_the_tabs() {
        let state = state_with(json!([
            {"id":"bbc1", "name":"BBC One", "countryCode":"GB", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"nine", "name":"Channel 9", "countryCode":"AU", "aurelisCategory":"ENTERTAINMENT"},
            {"id":"cbs", "name":"CBS", "countryCode":"US", "aurelisCategory":"ENTERTAINMENT"}
        ]));
        let before = state.lineup.overrides_etag();
        let app = crate::router(state.clone());
        let r = app.clone().oneshot(HttpRequest::builder().method("POST").uri("/ui/country_order")
            .header("host", "ha.local:8765").header("origin", "http://ha.local:8765").header("content-type", "application/json")
            .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<SocketAddr>().unwrap()))
            .body(Body::from(json!({"country":"GB","group":"ENTERTAINMENT","order":["us","AU","GB","US"]}).to_string())).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(state.lineup.country_order(), ["US", "AU", "GB"]);
        // The tabs follow it, and the televisions see a changed lineup carrying it.
        let countries: Vec<String> = state.lineup.tabs().into_iter().map(|t| t.country).collect();
        assert_eq!(countries, ["US", "AU", "GB"]);
        assert_ne!(state.lineup.overrides_etag(), before);
        assert_eq!(state.lineup.served_lineup().country_order, ["US", "AU", "GB"]);
        // Kept across a restart.
        let again = LineupStore::new(state.config.data_dir.clone(), std::sync::Arc::new(crate::state::now_ms));
        assert_eq!(again.country_order(), ["US", "AU", "GB"]);
        assert!(state.lineup.set_country_order(&["Britain".into()]).is_err());
    }
}

