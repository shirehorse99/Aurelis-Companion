mod ai_jobs;
mod ai_runner;
mod ai_store;
mod ai_failure;
mod auth;
mod betfair_relay;
mod bundle_store;
mod config;
mod dvr;
mod glm_jwt;
mod guide_parse;
mod guide_store;
mod host;
mod household;
mod mdns;
mod pairing_routes;
mod picture_store;
mod state;
mod status_page;
mod store;
pub mod stream_relay;
pub mod timeshift;
pub mod stream_probe;
pub mod transcode;
pub mod stage;
pub mod tuner;
pub mod homeassistant;
pub mod lineup;
pub mod lineup_tv;

use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use std::net::SocketAddr;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Router;
use config::Config;
use state::{AppState, PROTOCOL_VERSION};
use std::sync::Arc;
use std::time::Duration;

/// `EX_CONFIG`: the machine or its settings cannot run the Companion.
const EXIT_REFUSED: i32 = 78;
const VERSION_HEADER: &str = "x-aurelis-companion";
const LOAD_SAMPLE_EVERY: Duration = Duration::from_secs(30);
const BENCH_EVERY: Duration = Duration::from_secs(15 * 60);

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

/// A request naming a version we do not speak gets 426 and the versions we do.
async fn require_known_version(req: Request, next: Next) -> Response {
    if let Some(v) = req.headers().get(VERSION_HEADER) {
        let ok = v.to_str().map(|s| s.trim() == PROTOCOL_VERSION.to_string()).unwrap_or(false);
        if !ok {
            let seen = String::from_utf8_lossy(v.as_bytes()).chars().take(40).collect::<String>();
            return json_response(
                StatusCode::UPGRADE_REQUIRED,
                serde_json::json!({
                    "reason": format!("protocol version '{seen}' is not spoken here"),
                    "companionSpeaks": [PROTOCOL_VERSION],
                }),
            );
        }
    }
    next.run(req).await
}

async fn card(State(state): State<Arc<AppState>>) -> Response {
    match state.card_json() {
        Ok(body) => (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(e) => {
            eprintln!("card could not be written: {e}");
            json_response(StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({"reason": format!("card could not be written: {e}")}))
        }
    }
}

/// May this request see the pairing code and press Unpair? See `status_page::is_operator`.
fn operator_request(peer: Option<ConnectInfo<SocketAddr>>, headers: &HeaderMap) -> bool {
    status_page::is_operator(peer.map(|ConnectInfo(a)| a.ip()), headers.contains_key("x-ingress-path"))
}

fn lineup_editor_request(peer: Option<ConnectInfo<SocketAddr>>, headers: &HeaderMap) -> bool {
    status_page::is_lineup_editor(peer.map(|ConnectInfo(a)| a.ip()), headers.contains_key("x-ingress-path"))
}

async fn status(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap, req: Request) -> Response {
    let operator = operator_request(peer, &headers);
    let raw_query = req.uri().query().unwrap_or("");
    if raw_query.contains("tab=lineup") {
        let query = lineup::parse_channel_query(raw_query);
        let flash = query.flash.as_deref().and_then(|tok| state.get_flash(tok));
        return (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], status_page::render_lineup_with_flash(&state.page_view(), &state.lineup, operator, &query, flash.as_ref())).into_response();
    }
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], status_page::render(&state.page_view(), operator)).into_response()
}

async fn lineup_view(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap, req: Request) -> Response {
    let editor = lineup_editor_request(peer, &headers);
    let raw_query = req.uri().query().unwrap_or("");
    if !raw_query.split('&').any(|part| part == "view=table") {
        let page = lineup_tv::render(&state.page_view(), &state.lineup, editor, &lineup_tv::page_query(raw_query));
        return (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], page).into_response();
    }
    let query = lineup::parse_channel_query(raw_query);
    let flash = query.flash.as_deref().and_then(|tok| state.get_flash(tok));
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], status_page::render_lineup_with_flash(&state.page_view(), &state.lineup, editor, &query, flash.as_ref())).into_response()
}

/// The status page's Unpair button: same peer gate and same-origin check as the other form.
async fn ui_unpair(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "unpairing from the status page is only allowed from Home Assistant or this machine").into_response();
    }
    if let Err(reason) = status_page::same_origin_checked(
        header_str(&headers, "host"),
        header_str(&headers, "x-forwarded-host"),
        header_str(&headers, "origin"),
        header_str(&headers, "referer"),
        header_str(&headers, "sec-fetch-site"),
    ) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], reason).into_response();
    }
    let result = state::unpair(&state).await;
    match result {
        Ok(()) => {
            state.push_job("unpaired from the status page; household data wiped");
            (StatusCode::SEE_OTHER, [(header::LOCATION, "../")], "").into_response()
        }
        Err(r) => json_response(r.status, serde_json::json!({ "reason": r.reason })),
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn check_same_origin(headers: &HeaderMap) -> Result<(), Response> {
    status_page::same_origin_checked(
        header_str(headers, "host"),
        header_str(headers, "x-forwarded-host"),
        header_str(headers, "origin"),
        header_str(headers, "referer"),
        header_str(headers, "sec-fetch-site"),
    )
    .map_err(|reason| {
        (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], reason).into_response()
    })
}

fn wants_html(headers: &HeaderMap) -> bool {
    header_str(headers, "accept").is_some_and(|accept| accept.contains("text/html"))
}

fn append_flash_and_anchor(dest: &str, flash_token: &str, anchor: Option<&str>) -> String {
    let mut out = if dest.contains('?') {
        format!("{dest}&f={flash_token}")
    } else if dest.ends_with("/lineup") || dest == "lineup" {
        format!("{dest}?f={flash_token}")
    } else {
        dest.to_string()
    };
    if let Some(a) = anchor {
        out.push('#');
        out.push_str(a);
    }
    out
}

fn redirect_with_flash(
    state: &AppState,
    dest: &str,
    kind: state::FlashKind,
    message: impl Into<String>,
    anchor: Option<&str>,
) -> Response {
    let token = state.push_flash(kind, message);
    let target = append_flash_and_anchor(dest, &token, anchor);
    (StatusCode::SEE_OTHER, [(header::LOCATION, target)], "").into_response()
}

fn ui_error_or_redirect(
    state: &AppState,
    headers: &HeaderMap,
    dest: &str,
    message: impl Into<String>,
) -> Response {
    let msg = message.into();
    if wants_html(headers) && dest.starts_with("../lineup") {
        redirect_with_flash(state, dest, state::FlashKind::Error, msg, None)
    } else {
        (StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], msg).into_response()
    }
}

/// The status page's Pause/Resume form: same peer gate and same-origin check as Unpair. Pausing
/// tells every television the Companion is busy, so a stranger on the LAN must not press it.
async fn pause(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap, body: String) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "pausing from the status page is only allowed from Home Assistant or this machine").into_response();
    }
    if let Err(reason) = status_page::same_origin_checked(
        header_str(&headers, "host"),
        header_str(&headers, "x-forwarded-host"),
        header_str(&headers, "origin"),
        header_str(&headers, "referer"),
        header_str(&headers, "sec-fetch-site"),
    ) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], reason).into_response();
    }
    let to = body.split('&').find_map(|kv| kv.strip_prefix("to="));
    match to {
        Some("pause") => state.set_paused(true),
        Some("resume") => state.set_paused(false),
        _ => {
            return (StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "expected to=pause or to=resume").into_response()
        }
    }
    // The form posts to <base>/ui/pause, so "../" is <base>/, the status page, under any ingress prefix.
    (StatusCode::SEE_OTHER, [(header::LOCATION, "../")], "").into_response()
}

async fn toggle_notifications(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let current = state.ha.notifications_enabled();
    state.ha.set_notifications_enabled(!current);
    state.push_job(&format!("notifications master switch toggled to {}", if !current { "on" } else { "off" }));
    (StatusCode::SEE_OTHER, [(header::LOCATION, "../")], "").into_response()
}

async fn toggle_cctv_dedup(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let current = state.ha.suppress_cctv_duplicates();
    state.ha.set_suppress_cctv_duplicates(!current);
    state.push_job(&format!("CCTV duplicate alert suppression toggled to {}", if !current { "on" } else { "off" }));
    (StatusCode::SEE_OTHER, [(header::LOCATION, "../")], "").into_response()
}

async fn toggle_catchup_ad_skip(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let current = state.ha.catchup_ad_skip_enabled();
    state.ha.set_catchup_ad_skip_enabled(!current);
    state.push_job(&format!("catch-up ad skip master switch toggled to {}", if !current { "on" } else { "off" }));
    (StatusCode::SEE_OTHER, [(header::LOCATION, "../")], "").into_response()
}

async fn toggle_catchup_live_handoff(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let current = state.ha.catchup_live_handoff_enabled();
    state.ha.set_catchup_live_handoff_enabled(!current);
    state.push_job(&format!("catch-up live handoff master switch toggled to {}", if !current { "on" } else { "off" }));
    (StatusCode::SEE_OTHER, [(header::LOCATION, "../")], "").into_response()
}

fn parse_form(body: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for part in body.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            map.insert(k.to_string(), lineup::urlencoding_decode(v));
        }
    }
    map
}

fn parse_form_multi(body: &str) -> std::collections::HashMap<String, Vec<String>> {
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for part in body.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            map.entry(k.to_string()).or_default().push(lineup::urlencoding_decode(v));
        }
    }
    map
}

/// One editor row's fields as a save request. `suffix` is `""` for the single-row form and
/// `"__N"` for row N of the page-wide form, whose fields all carry their row's index.
fn row_save_request(form: &std::collections::HashMap<String, String>, suffix: &str) -> Result<lineup::SaveOverrideRequest, String> {
    let get = |key: &str| form.get(&format!("{key}{suffix}"));
    let Some(ch_id) = get("channel_id").cloned() else {
        return Err("missing channel_id".into());
    };
    let row_edit = get("row_edit").is_some_and(|value| value == "1" || value == "true");
    let (num, clear_channel_number) = match get("channel_number").map(|s| s.trim()) {
        Some("") => (None, row_edit),
        Some(raw) => match raw.parse::<i32>() {
            Ok(n) if n > 0 => (Some(n), false),
            _ => return Err("Enter a positive dial number, or leave it blank.".into()),
        },
        None => (None, false),
    };
    let hidden = if row_edit {
        Some(get("hidden").is_some_and(|h| h == "true" || h == "on"))
    } else {
        get("hidden").map(|h| h == "true" || h == "on")
    };
    let folder_input = get("sports_extra_group").map(|s| s.trim());
    let prev_folder = get("prev_sports_extra_group").map(|s| s.trim()).unwrap_or("");
    let mut sports_extra_group = None;
    let category = match get("aurelis_group") {
        Some(value) if value.is_empty() => {
            if let Some(folder) = folder_input.filter(|f| !f.is_empty() && *f != prev_folder) {
                sports_extra_group = Some(folder.to_owned());
                Some("SPORT_EXTRA".into())
            } else {
                sports_extra_group = Some(String::new());
                Some(String::new())
            }
        }
        Some(value) => {
            if let Some(dropdown_folder) = value.strip_prefix("SPORT_EXTRA::") {
                let effective_folder = match folder_input {
                    Some(typed) => typed.to_owned(),
                    None => dropdown_folder.to_owned(),
                };
                sports_extra_group = Some(effective_folder);
                Some("SPORT_EXTRA".into())
            } else if let Some(groups) = value.split(',').map(lineup::aurelis_group_key).collect::<Option<Vec<_>>>() {
                let joined = groups.join(",");
                if joined == "SPORT_EXTRA" {
                    sports_extra_group = Some(folder_input.unwrap_or("").to_owned());
                } else {
                    sports_extra_group = Some(String::new());
                }
                Some(joined)
            } else {
                return Err("invalid Aurelis group".into());
            }
        }
        None => get("category").cloned().filter(|s| !s.trim().is_empty()),
    };
    let quick_move = get("quick_move").map(|s| s.trim()).filter(|s| !s.is_empty());
    let raw_move = quick_move.or_else(|| get("move_to_position").map(|s| s.trim()).filter(|s| !s.is_empty()));
    let move_to_position = match raw_move {
        Some(value) => match value.parse::<usize>() {
            Ok(position) => Some(position),
            Err(_) => return Err("Enter a position as a whole number.".into()),
        },
        None => None,
    };
    let display_name = if row_edit {
        get("display_name").cloned()
    } else {
        get("display_name").cloned().filter(|s| !s.trim().is_empty())
    };
    let country_code = if row_edit {
        get("country_code").cloned()
    } else {
        get("country_code").cloned().filter(|s| !s.trim().is_empty())
    };
    Ok(lineup::SaveOverrideRequest {
        channel_id: ch_id,
        display_name,
        category,
        country_code,
        position: None,
        channel_number: num,
        epg_id: None,
        hidden,
        updated_at_ms: None,
        logo_url: get("logo_url").cloned(),
        sports_extra_group,
        move_to_position,
        clear_channel_number,
        row_edit,
    })
}

async fn ui_save_channel_override(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !lineup_editor_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let form = parse_form(&body);
    let dest = safe_lineup_redirect(form.get("redirect_to"));
    let req = match row_save_request(&form, "") {
        Ok(req) => req,
        Err(error) => return ui_error_or_redirect(&state, &headers, &dest, error),
    };
    let ch_id = req.channel_id.clone();
    let move_to_position = req.move_to_position;
    let row_edit = req.row_edit;
    if let Err(e) = state.lineup.set_override(req) {
        return ui_error_or_redirect(&state, &headers, &dest, e);
    }
    let ch_name = state.lineup.channel_display_name(&ch_id);
    let anchor = status_page::row_anchor_id(&ch_id);
    let notice = if let Some(pos) = move_to_position {
        format!("Moved {ch_name} to position {pos}.")
    } else if row_edit && state.lineup.get_override(&ch_id).is_none() {
        format!("Cleared overrides for {ch_name}.")
    } else {
        format!("Saved {ch_name}.")
    };
    redirect_with_flash(&state, &dest, state::FlashKind::Success, notice, Some(&anchor))
}

/// The editor page's one form: a row's own Save or move buttons save that row, "Save all
/// changes" (or Enter in any field) saves every row on the page that was changed. Rows are
/// retried while any save succeeds, so a chain of renumbers (A to 102 while B leaves 102)
/// lands whatever order the rows were in; a straight swap still needs a spare number.
async fn ui_save_channel_rows(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !lineup_editor_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let mut form = parse_form(&body);
    let dest = safe_lineup_redirect(form.get("redirect_to"));
    let rows = form.get("rows").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0).min(1000);
    let single = if let Some((row, position)) = form.get("quick_move").and_then(|v| v.split_once(':')) {
        let (row, position) = (row.trim().to_owned(), position.trim().to_owned());
        form.insert(format!("quick_move__{row}"), position);
        row.parse::<usize>().ok()
    } else {
        form.get("save_row").and_then(|v| v.trim().parse::<usize>().ok())
    };
    let selected: Vec<usize> = match single {
        Some(row) if row < rows => vec![row],
        Some(_) => return ui_error_or_redirect(&state, &headers, &dest, "That row is no longer on the page. Reload and try again."),
        None => (0..rows).collect(),
    };

    let mut pending = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for row in selected {
        match row_save_request(&form, &format!("__{row}")) {
            Ok(req) => pending.push(req),
            Err(error) => {
                let id = form.get(&format!("channel_id__{row}")).cloned().unwrap_or_default();
                failed.push((id, error));
            }
        }
    }
    let single_row = single.is_some();
    let mut saved: Vec<(String, Option<usize>)> = Vec::new();
    let mut last_errors: Vec<(String, String)> = Vec::new();
    while !pending.is_empty() {
        let mut progress = false;
        last_errors.clear();
        let mut retry = Vec::new();
        for req in pending {
            let id = req.channel_id.clone();
            let moved = req.move_to_position;
            match state.lineup.save_override(req.clone()) {
                Ok((_, changed)) => {
                    progress = true;
                    if changed {
                        saved.push((id, moved));
                    }
                }
                Err(error) => {
                    last_errors.push((id, error));
                    retry.push(req);
                }
            }
        }
        pending = if progress { retry } else { Vec::new() };
    }
    failed.extend(last_errors);

    let name = |id: &str| state.lineup.channel_display_name(id);
    if !saved.is_empty() {
        state.push_job(&format!("lineup: saved {} channels from the editor", saved.len()));
    }
    if single_row && failed.is_empty() {
        let id = form.get(&format!("channel_id__{}", single.unwrap_or(0))).cloned().unwrap_or_default();
        let anchor = status_page::row_anchor_id(&id);
        let notice = match saved.first() {
            Some((id, Some(pos))) => format!("Moved {} to position {pos}.", name(id)),
            Some((id, None)) if state.lineup.get_override(id).is_none() => format!("Cleared overrides for {}.", name(id)),
            Some((id, None)) => format!("Saved {}.", name(id)),
            None => format!("No changes to save for {}.", name(&id)),
        };
        return redirect_with_flash(&state, &dest, state::FlashKind::Success, notice, Some(&anchor));
    }
    let saved_part = match saved.len() {
        0 => "No changes saved.".to_string(),
        1 => format!("Saved {}.", name(&saved[0].0)),
        n => format!("Saved {n} channels."),
    };
    if failed.is_empty() {
        let anchor = saved.last().map(|(id, _)| status_page::row_anchor_id(id));
        return redirect_with_flash(&state, &dest, state::FlashKind::Success, saved_part, anchor.as_deref());
    }
    let reasons = failed.iter().take(5)
        .map(|(id, why)| format!("{}: {why}", if id.is_empty() { "a row".to_string() } else { name(id) }))
        .collect::<Vec<_>>().join(" · ");
    let more = if failed.len() > 5 { format!(" (and {} more)", failed.len() - 5) } else { String::new() };
    let anchor = failed.first().filter(|(id, _)| !id.is_empty()).map(|(id, _)| status_page::row_anchor_id(id));
    let message = format!("{saved_part} Not saved — {reasons}{more}");
    if wants_html(&headers) && dest.starts_with("../lineup") {
        redirect_with_flash(&state, &dest, state::FlashKind::Error, message, anchor.as_deref())
    } else {
        (StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], message).into_response()
    }
}

fn safe_lineup_redirect(raw: Option<&String>) -> String {
    match raw.map(|s| s.trim()) {
        Some("lineup") => "../lineup".to_string(),
        Some(s) if s.starts_with("lineup?") && !s.contains('\r') && !s.contains('\n') => format!("../{s}"),
        _ => "../".to_string(),
    }
}

/// The status page's Delete / Cancel / Stop on a DVR recording: same gate as Unpair.
async fn ui_dvr_delete(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let form = parse_form(&body);
    let Some(id) = form.get("id").map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "missing recording id").into_response();
    };
    let title = state.dvr.get(&id).map(|r| r.title).unwrap_or_else(|| id.clone());
    match state.dvr.delete(&id) {
        Ok(()) => state.push_job(&format!("dvr: '{}' removed from the status page", title.chars().take(80).collect::<String>())),
        Err((status, reason)) => {
            state.push_job(&format!("dvr: could not remove '{}': {reason}", title.chars().take(80).collect::<String>()));
            return (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], format!("Could not remove the recording: {reason}")).into_response();
        }
    }
    (StatusCode::SEE_OTHER, [(header::LOCATION, "../")], "").into_response()
}

async fn ui_delete_channel_override(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !lineup_editor_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let form = parse_form(&body);
    let dest = safe_lineup_redirect(form.get("redirect_to"));
    let mut notice = "No override removed.".to_string();
    let mut anchor = None;
    if let Some(ch_id) = form.get("channel_id") {
        let name = state.lineup.channel_display_name(ch_id);
        anchor = Some(status_page::row_anchor_id(ch_id));
        if matches!(state.lineup.delete_override(ch_id), Ok(true)) {
            notice = format!("Reset overrides for {name}.");
        }
    }
    redirect_with_flash(&state, &dest, state::FlashKind::Success, notice, anchor.as_deref())
}

async fn ui_assign_numbers(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !lineup_editor_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let form = parse_form(&body);
    let dest = safe_lineup_redirect(form.get("redirect_to"));
    let start_num = match form.get("start_number").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(raw) => match raw.parse::<i32>() {
            Ok(n) if n > 0 => Some(n),
            _ => return ui_error_or_redirect(&state, &headers, &dest, "Enter a positive starting dial number, or leave it blank."),
        },
        None => None,
    };
    let overwrite = form.get("overwrite").map(|o| o == "true" || o == "on");
    let req = lineup::AssignNumbersRequest {
        country_code: form.get("country_code").cloned().filter(|s| !s.trim().is_empty()),
        group: form.get("aurelis_group").cloned().filter(|s| !s.trim().is_empty()),
        start_number: start_num,
        overwrite_existing: overwrite,
        order: form.get("order").cloned().filter(|s| !s.trim().is_empty()),
    };
    match state.lineup.assign_channel_numbers(req) {
        Ok(0) => redirect_with_flash(&state, &dest, state::FlashKind::Success, "All matching channels already had dial numbers.", None),
        Ok(count) => redirect_with_flash(&state, &dest, state::FlashKind::Success, format!("Assigned dial numbers to {count} channels."), None),
        Err(error) => ui_error_or_redirect(&state, &headers, &dest, error),
    }
}

async fn ui_bulk_channel_override(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !lineup_editor_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let form = parse_form_multi(&body);
    let dest = safe_lineup_redirect(form.get("redirect_to").and_then(|v| v.last()));
    let channel_ids = form.get("channel_ids").cloned().unwrap_or_default();
    let action_name = form.get("bulk_action").and_then(|v| v.last()).map(|s| s.as_str()).unwrap_or("");
    let (action, label) = match action_name {
        "hide" => (lineup::BulkAction::Hide, "hidden"),
        "unhide" => (lineup::BulkAction::Unhide, "unhidden"),
        "clear_number" => (lineup::BulkAction::ClearNumber, "dial numbers cleared"),
        "reset" => (lineup::BulkAction::Reset, "overrides reset"),
        "set_country" => {
            let cc = form.get("country_code").and_then(|v| v.last()).cloned().unwrap_or_default();
            (lineup::BulkAction::SetCountry(cc), "country updated")
        }
        "set_group" => {
            let val = form.get("aurelis_group").and_then(|v| v.last()).map(|s| s.trim()).unwrap_or("");
            if val.is_empty() {
                (lineup::BulkAction::SetGroup { category: None, sports_extra_group: None }, "group reset to Automatic")
            } else if let Some(folder) = val.strip_prefix("SPORT_EXTRA::") {
                (
                    lineup::BulkAction::SetGroup {
                        category: Some("SPORT_EXTRA".into()),
                        sports_extra_group: Some(folder.to_owned()),
                    },
                    "moved to Sports Extra folder",
                )
            } else if let Some(key) = lineup::aurelis_group_key(val) {
                (
                    lineup::BulkAction::SetGroup {
                        category: Some(key.to_owned()),
                        sports_extra_group: None,
                    },
                    "Aurelis group updated",
                )
            } else {
                return ui_error_or_redirect(&state, &headers, &dest, "Invalid Aurelis group selected.");
            }
        }
        _ => return ui_error_or_redirect(&state, &headers, &dest, "Choose a valid bulk action."),
    };
    match state.lineup.bulk_update(lineup::BulkOverrideRequest { channel_ids, action }) {
        Ok(0) => redirect_with_flash(&state, &dest, state::FlashKind::Success, "Selected channels already matched that state.", None),
        Ok(count) => {
            state.push_job(&format!("lineup: bulk updated {count} channels ({label})"));
            redirect_with_flash(&state, &dest, state::FlashKind::Success, format!("Updated {count} channels ({label})."), None)
        }
        Err(err) => ui_error_or_redirect(&state, &headers, &dest, err),
    }
}

async fn ui_export_overrides(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
) -> Response {
    if !lineup_editor_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    match state.lineup.export_overrides_json() {
        Ok(json) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .header(header::CONTENT_DISPOSITION, "attachment; filename=\"aurelis-lineup-overrides.json\"")
            .body(axum::body::Body::from(json))
            .unwrap(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], err).into_response(),
    }
}

async fn ui_import_overrides(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !lineup_editor_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    if let Err(resp) = check_same_origin(&headers) { return resp; }
    let form = parse_form(&body);
    let dest = safe_lineup_redirect(form.get("redirect_to"));
    let raw = form.get("overrides_json").cloned().unwrap_or_default();
    let replace = form.get("import_mode").is_some_and(|m| m.eq_ignore_ascii_case("replace"));
    match state.lineup.import_overrides_json(&raw, replace) {
        Ok(count) => {
            let mode = if replace { "replaced existing" } else { "merged" };
            state.push_job(&format!("lineup: imported {count} overrides ({mode})"));
            redirect_with_flash(&state, &dest, state::FlashKind::Success, format!("Imported {count} overrides ({mode})."), None)
        }
        Err(err) => ui_error_or_redirect(&state, &headers, &dest, err),
    }
}

#[derive(serde::Deserialize)]
struct DeviceSeen {
    id: String,
    name: String,
}

async fn device_seen(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    body: axum::body::Bytes,
) -> StatusCode {
    let Ok(device) = serde_json::from_slice::<DeviceSeen>(&body) else { return StatusCode::BAD_REQUEST };
    let name = device.name.trim();
    if device.id.len() != 16 || !device.id.bytes().all(|b| b.is_ascii_hexdigit()) ||
        name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return StatusCode::BAD_REQUEST;
    }
    let address = peer.and_then(|ConnectInfo(peer)| match peer.ip() {
        std::net::IpAddr::V4(address) => Some(address),
        std::net::IpAddr::V6(address) => address.to_ipv4_mapped(),
    })
        .filter(|address| address.is_private());
    state.see_device(device.id.to_ascii_lowercase(), name.to_owned(), address);
    StatusCode::NO_CONTENT
}

async fn monitor_devices(state: Arc<AppState>) {
    use state::DeviceReachability;
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        for (id, address, last_seen_ms) in state.device_probe_targets() {
            let result = tokio::task::spawn_blocking(move || {
                match std::process::Command::new("ping")
                    .args(["-4", "-n", "-q", "-c", "1", "-w", "1"])
                    .arg(address.to_string())
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status().ok().and_then(|status| status.code())
                {
                    Some(0) => DeviceReachability::Online,
                    Some(1) => DeviceReachability::NoReply,
                    _ => DeviceReachability::Unknown,
                }
            }).await.unwrap_or(DeviceReachability::Unknown);
            state.record_device_probe(&id, address, last_seen_ms, result);
        }
    }
}

fn router(state: Arc<AppState>) -> Router {
    // Everything here needs the household secret: nonce, proof, version header, sealed body.
    // The inner body limit replaces the global 8 KiB, which is for the pause form.
    let authed = Router::new()
        .route("/v1/device/seen", post(device_seen))
        .route("/v1/unpair", post(pairing_routes::unpair))
        .route("/v1/ai/get", post(ai_store::get_route))
        .route("/v1/ai/put", post(ai_store::put_route))
        .route("/v1/guide/register", post(guide_store::register_route))
        .route("/v1/guide/get", post(guide_store::get_route))
        .route("/v1/picture/get", post(picture_store::get_route))
        .route(ai_jobs::PATH, post(ai_jobs::submit_route))
        .route("/v1/ai/jobs/:id", get(ai_jobs::get_route).delete(ai_jobs::delete_route))
        .route(bundle_store::PATH, put(bundle_store::put_route).get(bundle_store::get_route))
        .route(bundle_store::OWNERSHIP_PATH, put(bundle_store::put_route).get(bundle_store::settings_route))
        .route(betfair_relay::PATH, post(betfair_relay::relay_route))
        .route("/v1/dvr/schedule", post(dvr::schedule_route))
        .route("/v1/dvr/recordings", get(dvr::list_route))
        .route("/v1/dvr/recordings/:id", axum::routing::delete(dvr::delete_route))
        .route("/v1/dvr/recordings/:id/protect", post(dvr::protect_route))
        .route("/v1/stage/schedule", post(stage::schedule_route))
        .route("/v1/stage/items", get(stage::list_route))
        .route("/v1/stage/items/:id", axum::routing::delete(stage::delete_route))
        .route("/v1/tuner/acquire", post(tuner::acquire_route))
        .route("/v1/tuner/heartbeat", post(tuner::heartbeat_route))
        .route("/v1/tuner/release", post(tuner::release_route))
        .route("/v1/tuner/status", get(tuner::status_route))
        .route("/v1/tv/state", post(homeassistant::handle_report_tv_state))
        .route("/v1/tv/states", get(homeassistant::handle_get_tv_states))
        .route("/v1/notifications/poll", get(homeassistant::handle_poll_notifications))
        .route("/v1/notifications/active", get(homeassistant::handle_get_active_notifications))
        .route("/v1/lineup/tv/overrides", get(lineup::handle_get_overrides))
        .route("/v1/lineup/tv/override", post(lineup::handle_save_override))
        .route("/v1/lineup/tv/override/:channel_id", axum::routing::delete(lineup::handle_delete_override))
        .route("/v1/lineup/tv/channels/report", post(lineup::handle_report_channels))
        .route("/v1/lineup/tv/arbiter/:key", get(lineup::handle_get_arbiter).put(lineup::handle_put_arbiter))
        // The middleware caps what it reads per route; this inner limit only has to admit the largest.
        .layer(DefaultBodyLimit::max(bundle_store::MAX_REQUEST_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_household));
    let v1 = Router::new()
        .route("/v1/card", get(card))
        .route("/v1/challenge", get(pairing_routes::challenge))
        .route("/v1/pair/start", post(pairing_routes::pair_start))
        .route("/v1/pair/finish", post(pairing_routes::pair_finish))
        .route("/v1/dvr/stream/:id", get(dvr::stream_route))
        .route("/v1/stage/stream/:id", get(stage::stream_route))
        .route("/v1/relay/live", get(stream_relay::live_relay_route))
        .route("/v1/relay/status", get(stream_relay::status_route))
        .route("/v1/relay/timeshift", get(stream_relay::timeshift_stream_route))
        .route("/v1/relay/timeshift/info", get(stream_relay::timeshift_info_route))
        .route("/v1/relay/pause", post(stream_relay::pause_route))
        .route("/v1/relay/transcode", get(transcode::transcode_stream_route))
        .route("/v1/relay/probe", get(transcode::probe_route))
        .route("/v1/notifications/push", post(homeassistant::handle_push_notification))
        .route("/v1/notifications/config", get(homeassistant::handle_get_notifications_config).post(homeassistant::handle_set_notifications_config))
        .route("/v1/catchup/config", get(homeassistant::handle_get_catchup_config).post(homeassistant::handle_set_catchup_config))
        .route("/v1/tv/command", post(homeassistant::handle_post_tv_command))
        .route("/v1/tv/commands", get(homeassistant::handle_poll_tv_commands))
        .route("/v1/lineup/overrides", get(lineup::handle_get_overrides))
        .route("/v1/lineup/override", post(lineup::handle_save_override))
        .route("/v1/lineup/override/:channel_id", axum::routing::delete(lineup::handle_delete_override))
        .route("/v1/lineup/channels", get(lineup::handle_get_channels))
        .route("/v1/lineup/channels/report", post(lineup::handle_report_channels))
        .route("/v1/lineup/assign_numbers", post(lineup::handle_assign_numbers))
        .merge(authed)
        .layer(middleware::from_fn(require_known_version));
    Router::new()
        .route("/", get(status))
        .route("/lineup", get(lineup_view))
        .route("/ui/pause", post(pause))
        .route("/ui/unpair", post(ui_unpair))
        .route("/ui/toggle_notifications", post(toggle_notifications))
        .route("/ui/toggle_cctv_dedup", post(toggle_cctv_dedup))
        .route("/ui/toggle_catchup_ad_skip", post(toggle_catchup_ad_skip))
        .route("/ui/toggle_catchup_live_handoff", post(toggle_catchup_live_handoff))
        .route("/ui/save_channel_override", post(ui_save_channel_override))
        // A whole editor page (100 rows of ids, names, logo URLs and folders) posts at once;
        // the 8 KB default below refused every save ("length limit exceeded", 25 Nov 2026).
        .route(
            "/ui/save_channel_rows",
            post(ui_save_channel_rows).layer(DefaultBodyLimit::max(1024 * 1024)),
        )
        .route("/ui/delete_channel_override", post(ui_delete_channel_override))
        .route("/ui/dvr_delete", post(ui_dvr_delete))
        .route("/ui/tab", get(lineup_tv::get_tab))
        .route("/ui/tab_order", post(lineup_tv::post_order).layer(DefaultBodyLimit::max(1024 * 1024)))
        .route("/ui/tab_sort", post(lineup_tv::post_sort))
        .route("/ui/channel_edit", post(lineup_tv::post_edit).layer(DefaultBodyLimit::max(64 * 1024)))
        .route("/ui/assign_numbers", post(ui_assign_numbers))
        .route(
            "/ui/bulk_channel_override",
            post(ui_bulk_channel_override).layer(DefaultBodyLimit::max(128 * 1024)),
        )
        .route("/ui/export_overrides", get(ui_export_overrides))
        .route(
            "/ui/import_overrides",
            post(ui_import_overrides).layer(DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .merge(v1)
        .layer(DefaultBodyLimit::max(8 * 1024))
        .with_state(state)
}

/// Ctrl-C, or SIGTERM (what `docker stop` sends to PID 1).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                eprintln!("cannot listen for SIGTERM ({e}); only Ctrl-C will stop gracefully");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn refuse(why: &str) -> ! {
    eprintln!("Aurelis Companion will not start: {why}");
    std::process::exit(EXIT_REFUSED);
}

#[tokio::main]
async fn main() {
    let config = Config::load().unwrap_or_else(|e| refuse(&e));
    if let Err(e) = std::fs::create_dir_all(&config.data_dir) {
        refuse(&format!("cannot create the data folder {}: {e}", config.data_dir.display()));
    }
    timeshift::cleanup_stale_timeshift(&config.data_dir);
    let facts = host::gather_facts(&config.data_dir, config.picture_dir.as_deref(), 0).unwrap_or_else(|e| refuse(&e));
    if let Err(why) = host::check_floor(&facts) {
        refuse(&why);
    }
    println!(
        "Aurelis Companion {}: {} ({}), {} MB RAM, {} MB free in {}",
        env!("CARGO_PKG_VERSION"),
        facts.label,
        facts.arch,
        facts.ram_mb,
        facts.disk_free_mb,
        config.data_dir.display()
    );

    let port = config.port;
    let data_dir = config.data_dir.clone();
    let picture_dir = config.picture_dir.clone();
    let stores = match store::Stores::open(&config, facts.disk_free_mb) {
        Ok(s) => Arc::new(s),
        Err(e) => refuse(&format!("cannot open the stores: {e}")),
    };
    println!("disk budget {} MB", stores.budget.limit_mb());
    let state = Arc::new(AppState::new(config, facts, stores.clone()));

    // Index files are written every few seconds when something changed (last-read stamps alone:
    // once a minute), and once more on the way out.
    let flusher = stores.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            flusher.budget.maintain(state::now_ms()).await;
        }
    });

    // Pictures are kept at most 90 days (TMDB allows six months): checked at start and once a day.
    if stores.picture.is_some() {
        let aging = stores.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(picture_store::AGE_SWEEP_EVERY);
            loop {
                tick.tick().await;
                if let Some(p) = aging.picture.as_ref() {
                    picture_store::sweep_expired(p, state::now_ms()).await;
                }
            }
        });
    }

    // Handed-over AI jobs: a pass every 15 s, free providers first, paid only after the wait.
    if state.config.ai_runner {
        ai_runner::spawn(state.clone());
        println!("AI runner on: a pass every {} s", ai_runner::PASS_EVERY.as_secs());
    } else {
        println!("AI runner off: {}", ai_jobs::RUNNER_OFF_REASON);
    }

    if state.config.betfair_relay {
        println!("Betfair relay on: {} and {} only, nothing kept", betfair_relay::BETFAIR_HOSTS[0], betfair_relay::BETFAIR_HOSTS[1]);
    } else {
        println!("Betfair relay off: {}", betfair_relay::RELAY_OFF_REASON);
    }

    if state.config.dvr_enabled {
        tokio::spawn(dvr::run_dvr_scheduler(state.clone()));
        println!("DVR scheduler on: captures saved to <data_dir>/dvr/files");
    } else {
        println!("DVR scheduler off: {}", state::DVR_OFF_REASON);
    }

    // Registered guides are downloaded on their own schedule, only while the card says available.
    tokio::spawn(guide_store::run_scheduler(state.clone()));
    // Once: clear a stale "parsing" and parse what is held but unparsed, out of date or missing its file.
    tokio::spawn(guide_store::startup_sweep(state.clone()));
    tokio::spawn(monitor_devices(state.clone()));
    let ha_sync = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            ha_sync.ha.republish_known_devices().await;
        }
    });

    // Load and memory every 30 s, so "busy" shows within a minute; the benchmark every 15 min.
    let sampler = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(LOAD_SAMPLE_EVERY);
        tick.tick().await;
        loop {
            tick.tick().await;
            let (d, p, bench) = (data_dir.clone(), picture_dir.clone(), sampler.facts().bench_score);
            match tokio::task::spawn_blocking(move || host::gather_facts(&d, p.as_deref(), bench)).await {
                Ok(Ok(facts)) => sampler.update_facts(facts),
                Ok(Err(why)) => eprintln!("host measurement failed, keeping the last figures: {why}"),
                Err(e) => eprintln!("host measurement failed: {e}"),
            }
        }
    });
    // An unused code is replaced when its ten minutes are up, and the new one is printed.
    let renewer = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            if renewer.auth.tick() {
                renewer.sync_pairing();
            }
        }
    });
    let bencher = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(BENCH_EVERY);
        loop {
            tick.tick().await;
            match tokio::task::spawn_blocking(host::run_benchmark).await {
                Ok(score) => {
                    bencher.set_bench_score(score);
                    bencher.push_job(&format!("speed benchmark: {score} MB/s"));
                }
                Err(e) => eprintln!("benchmark failed: {e}"),
            }
        }
    });

    // The LAN advert. It follows the pairing state; if multicast is unavailable it is one line and
    // the Companion serves on by address.
    let advert = match mdns::Advert::start(port, state.card().household_fingerprint.as_deref()) {
        Ok(a) => {
            println!("announcing {} on the LAN ({})", mdns::INSTANCE_NAME, mdns::SERVICE_TYPE.trim_end_matches('.'));
            Some(Arc::new(a))
        }
        Err(why) => {
            println!("not announcing on the LAN ({why}); television boxes must be given this machine's address instead");
            None
        }
    };
    if let Some(advert) = advert.clone() {
        let watched = state.clone();
        tokio::spawn(async move {
            let mut announced = watched.card().household_fingerprint;
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                let now = watched.card().household_fingerprint;
                if now != announced {
                    match advert.update(now.as_deref()) {
                        Ok(()) => announced = now,
                        Err(why) => eprintln!("LAN advert not updated: {why}"),
                    }
                }
            }
        });
    }

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Aurelis Companion cannot listen on port {port}: {e}");
            std::process::exit(1);
        }
    };
    println!("listening on port {port}");
    let served = axum::serve(listener, router(state).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Some(advert) = &advert {
        advert.stop();
    }
    stores.budget.flush_all().await;
    if let Err(e) = served {
        eprintln!("server stopped: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{test_config, test_state};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    async fn send(req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let app = router(Arc::new(test_state(test_config())));
        let res = app.oneshot(req).await.expect("router is infallible");
        let (status, headers) = (res.status(), res.headers().clone());
        let body = to_bytes(res.into_body(), 1 << 20).await.expect("body");
        (status, headers, String::from_utf8_lossy(&body).to_string())
    }

    fn get_req(path: &str, version: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(path).header("host", "nuc.local:8765");
        if let Some(v) = version {
            b = b.header(VERSION_HEADER, v);
        }
        b.body(Body::empty()).expect("request")
    }

    #[test]
    fn device_reachability_changes_without_advancing_the_app_check_in() {
        use state::DeviceReachability;
        let state = test_state(test_config());
        let old_address = "192.168.3.203".parse().unwrap();
        let new_address = "192.168.3.204".parse().unwrap();
        state.see_device("test".into(), "Boxroom & TV".into(), Some(old_address));
        let (_, _, checked_in) = state.device_probe_targets()[0].clone();
        state.record_device_probe("test", old_address, checked_in, DeviceReachability::NoReply);
        let view = state.page_view();
        assert_eq!(view.devices[0].last_seen_ms, checked_in);
        assert_eq!(view.devices[0].reachability, DeviceReachability::NoReply);
        let html = status_page::render(&view, true);
        assert!(html.contains("Boxroom &amp; TV"));
        assert!(html.contains("No ping reply"));
        assert!(!status_page::render(&view, false).contains("Boxroom"));

        state.see_device("test".into(), "Boxroom & TV".into(), Some(new_address));
        state.record_device_probe("test", old_address, checked_in, DeviceReachability::NoReply);
        assert_eq!(state.page_view().devices[0].reachability, DeviceReachability::Online);
    }

    #[tokio::test]
    async fn card_is_served_for_version_1() {
        let (status, _, body) = send(get_req("/v1/card", Some("1"))).await;
        assert_eq!(status, StatusCode::OK);
        let card: companion_protocol::card::Card = serde_json::from_str(&body).expect("a card");
        assert_eq!(card.protocol, vec![1]);
    }

    #[tokio::test]
    async fn other_versions_get_426_with_what_is_spoken() {
        let (status, _, body) = send(get_req("/v1/card", Some("2"))).await;
        assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(v["companionSpeaks"], serde_json::json!([1]));
        assert!(v["reason"].as_str().is_some());
    }

    #[tokio::test]
    async fn status_page_is_served_without_the_header() {
        let (status, headers, body) = send(get_req("/", None)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_TYPE].to_str().unwrap_or("").starts_with("text/html"));
        assert!(body.contains("<title>Aurelis Companion</title>"));
    }

    fn pause_req_from(peer: Option<&str>, origin: Option<&str>, body: &'static str) -> Request<Body> {
        let mut b = Request::builder().method("POST").uri("/ui/pause").header("host", "nuc.local:8765");
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        let mut req = b.body(Body::from(body)).expect("request");
        if let Some(p) = peer {
            let addr: SocketAddr = p.parse().expect("socket address");
            req.extensions_mut().insert(ConnectInfo(addr));
        }
        req
    }

    /// From this machine, as the operator's own browser would be.
    fn pause_req(origin: Option<&str>, body: &'static str) -> Request<Body> {
        pause_req_from(Some("127.0.0.1:50000"), origin, body)
    }

    #[tokio::test]
    async fn pause_from_the_lan_is_refused_and_does_nothing() {
        let state = Arc::new(test_state(test_config()));
        let req = pause_req_from(Some("192.168.3.50:50000"), Some("http://nuc.local:8765"), "to=pause");
        let res = router(state.clone()).oneshot(req).await.expect("infallible");
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(!state.is_paused());
    }

    #[tokio::test]
    async fn pause_with_no_known_peer_is_refused() {
        let state = Arc::new(test_state(test_config()));
        let res = router(state.clone()).oneshot(pause_req_from(None, None, "to=pause")).await.expect("infallible");
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(!state.is_paused());
    }

    #[tokio::test]
    async fn pause_from_home_assistant_ingress_is_allowed() {
        let state = Arc::new(test_state(test_config()));
        let mut req = pause_req_from(Some("172.30.32.2:50000"), None, "to=pause");
        req.headers_mut().insert("x-ingress-path", "/api/hassio_ingress/abc".parse().expect("header"));
        let res = router(state.clone()).oneshot(req).await.expect("infallible");
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(state.is_paused());
    }

    #[tokio::test]
    async fn pause_from_the_same_origin_redirects_back_relatively() {
        let (status, headers, _) = send(pause_req(Some("http://nuc.local:8765"), "to=pause")).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "../");
    }

    #[tokio::test]
    async fn pause_from_another_site_is_refused_and_does_nothing() {
        let state = Arc::new(test_state(test_config()));
        let req = pause_req(Some("http://evil.example"), "to=pause");
        let res = router(state.clone()).oneshot(req).await.expect("infallible");
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(!state.is_paused());
    }

    #[tokio::test]
    async fn garbage_pause_body_is_a_400_not_a_panic() {
        let (status, _, _) = send(pause_req(None, "\u{0}&&=&to=")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
