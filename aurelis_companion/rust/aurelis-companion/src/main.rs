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
        return (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], status_page::render_lineup(&state.page_view(), &state.lineup, operator, &query)).into_response();
    }
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], status_page::render(&state.page_view(), operator)).into_response()
}

async fn lineup_view(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap, req: Request) -> Response {
    let editor = lineup_editor_request(peer, &headers);
    let raw_query = req.uri().query().unwrap_or("");
    let query = lineup::parse_channel_query(raw_query);
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], status_page::render_lineup(&state.page_view(), &state.lineup, editor, &query)).into_response()
}

/// The status page's Unpair button: same peer gate and same-origin check as the other form.
async fn ui_unpair(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "unpairing from the status page is only allowed from Home Assistant or this machine").into_response();
    }
    if let Err(reason) = status_page::same_origin(
        header_str(&headers, "host"),
        header_str(&headers, "x-forwarded-host"),
        header_str(&headers, "origin"),
        header_str(&headers, "referer"),
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

/// The status page's Pause/Resume form: same peer gate and same-origin check as Unpair. Pausing
/// tells every television the Companion is busy, so a stranger on the LAN must not press it.
async fn pause(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap, body: String) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "pausing from the status page is only allowed from Home Assistant or this machine").into_response();
    }
    if let Err(reason) = status_page::same_origin(
        header_str(&headers, "host"),
        header_str(&headers, "x-forwarded-host"),
        header_str(&headers, "origin"),
        header_str(&headers, "referer"),
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
    let current = state.ha.notifications_enabled();
    state.ha.set_notifications_enabled(!current);
    state.push_job(&format!("notifications master switch toggled to {}", if !current { "on" } else { "off" }));
    (StatusCode::SEE_OTHER, [(header::LOCATION, "../")], "").into_response()
}

async fn toggle_cctv_dedup(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    let current = state.ha.suppress_cctv_duplicates();
    state.ha.set_suppress_cctv_duplicates(!current);
    state.push_job(&format!("CCTV duplicate alert suppression toggled to {}", if !current { "on" } else { "off" }));
    (StatusCode::SEE_OTHER, [(header::LOCATION, "../")], "").into_response()
}

async fn toggle_catchup_ad_skip(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    let current = state.ha.catchup_ad_skip_enabled();
    state.ha.set_catchup_ad_skip_enabled(!current);
    state.push_job(&format!("catch-up ad skip master switch toggled to {}", if !current { "on" } else { "off" }));
    (StatusCode::SEE_OTHER, [(header::LOCATION, "../")], "").into_response()
}

async fn toggle_catchup_live_handoff(State(state): State<Arc<AppState>>, peer: Option<ConnectInfo<SocketAddr>>, headers: HeaderMap) -> Response {
    if !operator_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
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

async fn ui_save_channel_override(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !lineup_editor_request(peer, &headers) {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "forbidden").into_response();
    }
    let form = parse_form(&body);
    let Some(ch_id) = form.get("channel_id").cloned() else {
        return (StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "missing channel_id").into_response();
    };
    let num = form.get("channel_number").and_then(|s| s.parse::<i32>().ok());
    let hidden = form.get("hidden").map(|h| h == "true" || h == "on");
    let req = lineup::SaveOverrideRequest {
        channel_id: ch_id,
        display_name: form.get("display_name").cloned().filter(|s| !s.trim().is_empty()),
        category: form.get("category").cloned().filter(|s| !s.trim().is_empty()),
        country_code: form.get("country_code").cloned().filter(|s| !s.trim().is_empty()),
        position: None,
        channel_number: num,
        epg_id: None,
        hidden,
        updated_at_ms: None,
    };
    if let Err(e) = state.lineup.set_override(req) {
        return (StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], e).into_response();
    }
    let dest = safe_lineup_redirect(form.get("redirect_to"));
    (StatusCode::SEE_OTHER, [(header::LOCATION, dest)], "").into_response()
}

fn safe_lineup_redirect(raw: Option<&String>) -> String {
    match raw.map(|s| s.trim()) {
        Some("lineup") => "../lineup".to_string(),
        Some(s) if s.starts_with("lineup?") && !s.contains('\r') && !s.contains('\n') => format!("../{s}"),
        _ => "../".to_string(),
    }
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
    let form = parse_form(&body);
    if let Some(ch_id) = form.get("channel_id") {
        let _ = state.lineup.delete_override(ch_id);
    }
    let dest = safe_lineup_redirect(form.get("redirect_to"));
    (StatusCode::SEE_OTHER, [(header::LOCATION, dest)], "").into_response()
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
    let form = parse_form(&body);
    let start_num = form.get("start_number").and_then(|s| s.parse::<i32>().ok());
    let overwrite = form.get("overwrite").map(|o| o == "true" || o == "on");
    let req = lineup::AssignNumbersRequest {
        country_code: form.get("country_code").cloned().filter(|s| !s.trim().is_empty()),
        start_number: start_num,
        overwrite_existing: overwrite,
    };
    let _ = state.lineup.assign_channel_numbers(req);
    let dest = safe_lineup_redirect(form.get("redirect_to"));
    (StatusCode::SEE_OTHER, [(header::LOCATION, dest)], "").into_response()
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
        .route("/ui/delete_channel_override", post(ui_delete_channel_override))
        .route("/ui/assign_numbers", post(ui_assign_numbers))
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
