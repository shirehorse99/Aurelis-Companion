//! The status page: plain server-rendered HTML, no script. Every URL is relative so it works
//! under Home Assistant's ingress path prefix.

use crate::state::PageView;

pub const PRODUCT_NAME: &str = "Aurelis Companion";

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}

/// Whether a request may see the pairing code and press Unpair: it came through Home Assistant's
/// ingress (peer 172.30.32.2 AND an `X-Ingress-Path` header, which a LAN client could forge but
/// cannot make come from that address) or from this machine itself.
pub fn is_operator(peer: Option<std::net::IpAddr>, has_ingress_header: bool) -> bool {
    let Some(ip) = peer.map(|p| p.to_canonical()) else { return false };
    ip.is_loopback() || (has_ingress_header && ip == std::net::IpAddr::V4(std::net::Ipv4Addr::new(172, 30, 32, 2)))
}

/// Allows editing channel lineups from Home Assistant Ingress or the local private network.
pub fn is_lineup_editor(peer: Option<std::net::IpAddr>, has_ingress_header: bool) -> bool {
    if is_operator(peer, has_ingress_header) {
        return true;
    }
    match peer.map(|p| p.to_canonical()) {
        Some(std::net::IpAddr::V4(ip)) => ip.is_private() || ip.is_loopback(),
        Some(std::net::IpAddr::V6(ip)) => ip.is_loopback(),
        None => false,
    }
}

pub const CODE_HIDDEN: &str = "Pairing code hidden on the network. Open this page from Home Assistant, or read it with `docker logs`.";

pub fn render(view: &PageView, operator: bool) -> String {
    let c = &view.card;
    let mut h = String::new();
    h.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    h.push_str(&format!("<title>{}</title>", PRODUCT_NAME));
    h.push_str("<style>");
    h.push_str(include_str!("status_page.css"));
    h.push_str("</style></head><body><main>");
    h.push_str(&format!("<h1>{}</h1>", PRODUCT_NAME));

    h.push_str("<nav class=\"nav-tabs\">");
    h.push_str("<a href=\"./\" class=\"nav-tab active\">System Status</a>");
    h.push_str("<a href=\"lineup\" class=\"nav-tab\">Channels</a>");
    h.push_str("</nav>");

    let state_line = match &c.busy_reason {
        Some(r) => format!("{} ({})", escape(&c.state), escape(r)),
        None => escape(&c.state),
    };
    h.push_str(&format!("<p><strong>Status:</strong> {state_line}</p>"));

    h.push_str("<section aria-labelledby=\"ai-jobs\"><h2 id=\"ai-jobs\">Recent AI jobs</h2>");
    if view.ai_jobs.is_empty() {
        h.push_str("<p>No AI jobs held. Jobs leave this list after collection.</p>");
    } else {
        h.push_str("<ul class=\"jobs\">");
        for j in view.ai_jobs.iter().take(crate::state::AI_JOBS_SHOWN) {
            let label = if j.label.is_empty() { "Unlabelled job" } else { j.label.as_str() };
            let state = state_name(j.state);
            h.push_str(&format!(
                "<li><div class=\"job-heading\"><strong>{}</strong><span class=\"badge {state}\">{state}</span></div><div class=\"job-meta\">Queued {} ago",
                escape(label),
                age(view.now_ms, j.queued_at_ms),
            ));
            if let Some(provider) = &j.provider {
                h.push_str(&format!(" · {}", escape(provider)));
            }
            h.push_str("</div>");
            if let Some(reason) = j.reason.as_deref().filter(|reason| !reason.is_empty()) {
                h.push_str(&format!("<p class=\"job-reason\">{}</p>", escape(reason)));
            }
            h.push_str("</li>");
        }
        h.push_str("</ul>");
    }
    h.push_str("</section>");

    h.push_str("<section aria-labelledby=\"activity\"><h2 id=\"activity\">Recent activity</h2>");
    if view.jobs.is_empty() {
        h.push_str("<p>No activity recorded yet.</p>");
    } else {
        const ACTIVITY_PREVIEW: usize = 8;
        let mut jobs = view.jobs.iter().collect::<Vec<_>>();
        jobs.sort_by_key(|(at, _)| *at);
        let mut newest = jobs.into_iter().rev();
        h.push_str("<ul class=\"activity\">");
        for (at, line) in newest.by_ref().take(ACTIVITY_PREVIEW) {
            h.push_str(&format!("<li><time>{}</time> · {}</li>", crate::guide_store::iso_utc(*at), escape(line)));
        }
        h.push_str("</ul>");
        if newest.len() > 0 {
            h.push_str(&format!("<details><summary>Show {} older entries</summary><ul class=\"activity\">", newest.len()));
            for (at, line) in newest {
                h.push_str(&format!("<li><time>{}</time> · {}</li>", crate::guide_store::iso_utc(*at), escape(line)));
            }
            h.push_str("</ul></details>");
        }
    }
    h.push_str("</section>");
    if operator {
        h.push_str("<section><h2>Devices</h2>");
        if view.devices.is_empty() {
            h.push_str("<p>No devices have checked in since Companion started.</p>");
        } else {
            h.push_str("<table><tr><th>Device</th><th>Network</th><th>Lineup sync</th><th>Last Aurelis check-in (UTC)</th></tr>");
            let mut devices = view.devices.iter().collect::<Vec<_>>();
            devices.sort_by_key(|device| std::cmp::Reverse(device.last_seen_ms));
            for device in devices {
                let network = match device.reachability {
                    crate::state::DeviceReachability::Online => "Online",
                    crate::state::DeviceReachability::NoReply => "No ping reply",
                    crate::state::DeviceReachability::Unknown => "Unknown",
                };
                let sync_cell = match (device.synced_lineup_etag.as_deref(), device.last_lineup_sync_ms) {
                    (Some(etag), Some(ts)) if etag == view.lineup_etag => format!(
                        "<span class=\"badge done\">Up to date</span> <span class=\"job-meta\">{} ago</span>",
                        age(view.now_ms, ts.max(0) as u64)
                    ),
                    (Some(_), Some(ts)) => format!(
                        "<span class=\"badge pending\">Pending sync</span> <span class=\"job-meta\">{} ago</span>",
                        age(view.now_ms, ts.max(0) as u64)
                    ),
                    _ => "<span class=\"job-meta\">Not synced yet</span>".to_string(),
                };
                h.push_str(&format!("<tr><td>{}</td><td>{network}</td><td>{sync_cell}</td><td><time>{}</time></td></tr>",
                    escape(&device.name), crate::guide_store::iso_utc(device.last_seen_ms)));
            }
            h.push_str("</table>");
        }
        h.push_str("</section>");
    }
    h.push_str("<div class=\"overview\">");

    h.push_str("<section><h2>Pairing</h2>");
    if c.paired {
        let fp = c.household_fingerprint.as_deref().unwrap_or("unknown");
        h.push_str(&format!("<p>Paired with a household (fingerprint <code>{}</code>).</p>", escape(fp)));
    } else if !operator {
        h.push_str(&format!("<p>Not paired. {}</p>", escape(CODE_HIDDEN)));
    } else if let Some(code) = &view.pairing_code {
        h.push_str(&format!("<p>Not paired. Enter this code on the television: <code>{}</code></p>", escape(code)));
    } else {
        h.push_str("<p>Not paired. No pairing code is available yet.</p>");
    }

    if c.paired && operator {
        h.push_str("<p>Unpairing wipes everything stored for this household and makes a new code.</p>");
        h.push_str("<form method=\"post\" action=\"ui/unpair\"><button type=\"submit\">Unpair</button></form>");
    }

    let held = match c.household_bundle_at_ms {
        Some(ms) => format!("captured {}", crate::guide_store::iso_utc(ms as i64)),
        None => "none yet".to_string(),
    };
    h.push_str(&format!("<p>Household settings held: {}</p>", escape(&held)));

    h.push_str("</section><section><h2>This machine</h2><table>");
    let row = |k: &str, v: String| format!("<tr><th>{}</th><td>{}</td></tr>", escape(k), escape(&v));
    h.push_str(&row("Machine", c.host.label.clone()));
    h.push_str(&row("Architecture", c.host.arch.clone()));
    h.push_str(&row("Memory", format!("{} MB free of {} MB", c.host.free_ram_mb, c.host.ram_mb)));
    h.push_str(&row("Disk free", format!("{} MB", c.host.disk_free_mb)));
    h.push_str(&row("On an SD card", yes_no(c.host.on_sd_card).to_string()));
    h.push_str(&row("Speed score", format!("{} MB/s", c.host.bench_score)));
    h.push_str(&row("Version", c.version.clone()));
    h.push_str("</table>");

    h.push_str("</section><section><h2>What it offers</h2><table>");
    let caps = [
        ("aiStore", "AI answer store", c.capabilities.ai_store),
        ("guideStore", "Guide store", c.capabilities.guide_store),
        ("pictureStore", "Picture store", c.capabilities.picture_store),
        ("aiRunner", "AI runner", c.capabilities.ai_runner),
        ("heavyPass", "Heavy passes", c.capabilities.heavy_pass),
        ("dvr", "Household DVR", c.capabilities.dvr),
        ("tuner", "Tuner Arbiter", c.capabilities.tuner),
    ];
    for (key, name, on) in caps {
        let note = if on { "on".to_string() } else { format!("off: {}", c.declined.get(key).map(String::as_str).unwrap_or("no reason recorded")) };
        h.push_str(&row(name, note));
    }
    h.push_str("</table>");
    h.push_str(&format!("<p>{}</p>", escape(&betfair_line(view))));

    h.push_str("</section><section><h2>Disk use</h2><table>");
    h.push_str(&row("Budget", format!("{} MB", c.disk.budget_mb)));
    h.push_str(&row("AI answers", format!("{} MB", c.disk.used_mb.ai)));
    h.push_str(&row("Guides", format!("{} MB", c.disk.used_mb.guide)));
    h.push_str(&row("Pictures", format!("{} MB", c.disk.used_mb.picture)));
    h.push_str(&row("DVR", format!("{} MB", c.disk.used_mb.dvr)));
    h.push_str("</table>");

    h.push_str("</section><section><h2>DVR Recordings</h2>");
    if view.dvr_recordings.is_empty() {
        h.push_str("<p>No recordings scheduled or saved.</p>");
    } else {
        h.push_str(if operator {
            "<table><tr><th>Title</th><th>Channel</th><th>Status</th><th>Size</th><th></th></tr>"
        } else {
            "<table><tr><th>Title</th><th>Channel</th><th>Status</th><th>Size</th></tr>"
        });
        for r in &view.dvr_recordings {
            let size_str = if r.bytes_written > 0 {
                format!("{:.1} MB", r.bytes_written as f64 / (1024.0 * 1024.0))
            } else {
                "-".to_string()
            };
            let status_str = if r.protected {
                format!("{:?} (protected)", r.status)
            } else {
                format!("{:?}", r.status)
            };
            // Why a recording stopped, so a Partial or Failed one says what happened.
            let why = r.failure_reason.as_deref().map(|w| format!("<div class=\"job-meta\">{}</div>", escape(w))).unwrap_or_default();
            let action = if !operator {
                String::new()
            } else {
                let (label, ask) = match r.status {
                    crate::dvr::RecordingStatus::Scheduled => ("Cancel", "Cancel this scheduled recording?"),
                    crate::dvr::RecordingStatus::Recording => ("Stop", "Stop this recording? What has been recorded is kept, marked Cancelled."),
                    _ => ("Delete", "Delete this recording and its file? This cannot be undone."),
                };
                format!(
                    "<td><form method=\"post\" action=\"ui/dvr_delete\" data-ask=\"{}\" onsubmit=\"return confirm(this.dataset.ask)\"><input type=\"hidden\" name=\"id\" value=\"{}\"><button type=\"submit\">{label}</button></form></td>",
                    escape(&format!("{ask} ({})", r.title)), escape(&r.id),
                )
            };
            h.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}{why}</td><td>{}</td>{action}</tr>",
                escape(&r.title),
                escape(&r.channel_name),
                escape(&status_str),
                size_str
            ));
        }
        h.push_str("</table>");
    }

    h.push_str("</section><section><h2>Stream Relays</h2>");
    if view.active_relays.is_empty() {
        h.push_str("<p>No live stream relays active.</p>");
    } else {
        h.push_str("<table><tr><th>Channel</th><th>Active TVs</th><th>Saved Streams</th><th>Relayed</th></tr>");
        for r in &view.active_relays {
            let ch_label = r.channel_name.as_deref().or(r.channel_id.as_deref()).unwrap_or(&r.sanitized_url);
            let mb_relayed = (r.total_bytes_relayed as f64) / (1024.0 * 1024.0);
            h.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.1} MB</td></tr>",
                escape(ch_label),
                r.active_subscribers,
                r.upstream_streams_saved,
                mb_relayed
            ));
        }
        h.push_str("</table>");
    }

    h.push_str("</section><section><h2>Guides</h2>");
    if view.guides.is_empty() {
        h.push_str("<p>No guides are registered.</p>");
    } else {
        // 12 characters, as the television prints: a guide id is an unsalted hash of a URL that can carry a login.
        h.push_str("<table><tr><th>Guide</th><th>Parsed</th></tr>");
        for (id, name, status) in &view.guides {
            let short_id: String = id.chars().take(12).collect();
            let label = if name.is_empty() { short_id.as_str() } else { name.as_str() };
            h.push_str(&format!("<tr><td>{}</td><td>{}</td></tr>", escape(label), escape(status)));
        }
        h.push_str("</table>");
    }

    h.push_str("</section><section><h2>Master Switches</h2>");
    h.push_str("<table><tr><th>Master Switch</th><th>State</th><th>Action</th></tr>");
    
    let notif_label = if view.notifications_enabled { "Turn Off" } else { "Turn On" };
    let notif_state = if view.notifications_enabled { "ON" } else { "OFF" };
    let notif_form = if operator {
        format!("<form method=\"post\" action=\"ui/toggle_notifications\"><button type=\"submit\">{notif_label}</button></form>")
    } else {
        notif_state.to_string()
    };
    h.push_str(&format!("<tr><td>Camera & Doorbell Notifications</td><td><strong>{}</strong></td><td>{}</td></tr>", notif_state, notif_form));

    let dedup_label = if view.suppress_cctv_duplicates { "Turn Off" } else { "Turn On" };
    let dedup_state = if view.suppress_cctv_duplicates { "ON" } else { "OFF" };
    let dedup_form = if operator {
        format!("<form method=\"post\" action=\"ui/toggle_cctv_dedup\"><button type=\"submit\">{dedup_label}</button></form>")
    } else {
        dedup_state.to_string()
    };
    h.push_str(&format!("<tr><td>CCTV Duplicate Alert Suppression</td><td><strong>{}</strong></td><td>{}</td></tr>", dedup_state, dedup_form));

    let skip_label = if view.catchup_ad_skip_enabled { "Turn Off" } else { "Turn On" };
    let skip_state = if view.catchup_ad_skip_enabled { "ON" } else { "OFF" };
    let skip_form = if operator {
        format!("<form method=\"post\" action=\"ui/toggle_catchup_ad_skip\"><button type=\"submit\">{skip_label}</button></form>")
    } else {
        skip_state.to_string()
    };
    h.push_str(&format!("<tr><td>Catch-Up Advert Skipping Master</td><td><strong>{}</strong></td><td>{}</td></tr>", skip_state, skip_form));

    let handoff_label = if view.catchup_live_handoff_enabled { "Turn Off" } else { "Turn On" };
    let handoff_state = if view.catchup_live_handoff_enabled { "ON" } else { "OFF" };
    let handoff_form = if operator {
        format!("<form method=\"post\" action=\"ui/toggle_catchup_live_handoff\"><button type=\"submit\">{handoff_label}</button></form>")
    } else {
        handoff_state.to_string()
    };
    h.push_str(&format!("<tr><td>Catch-Up Live Handoff Master</td><td><strong>{}</strong></td><td>{}</td></tr>", handoff_state, handoff_form));
    h.push_str("</table>");

    h.push_str("</section><section><h2>Channel Lineup & Household Overrides</h2>");
    h.push_str(&format!("<p>Total channels known: <strong>{}</strong> · Active overrides: <strong>{}</strong></p>", view.lineup_channels_count, view.lineup_overrides_count));
    h.push_str("<p><a href=\"lineup\" class=\"nav-tab active\" style=\"display:inline-block; font-size: 0.95rem;\">Open the channel editor →</a></p>");

    h.push_str("</section><section><h2>Pause</h2>");
    let (to, label, note) = if view.paused {
        ("resume", "Resume", "Paused: the Companion tells televisions it is busy.")
    } else {
        ("pause", "Pause", "Pausing tells televisions the Companion is busy, until you resume.")
    };
    h.push_str(&format!("<p>{}</p>", escape(note)));
    if operator {
        h.push_str(&format!("<form method=\"post\" action=\"ui/pause\"><input type=\"hidden\" name=\"to\" value=\"{to}\"><button type=\"submit\">{label}</button></form>"));
    } else {
        h.push_str("<p>Pause and resume only from Home Assistant or this machine.</p>");
    }

    h.push_str("</section></div></main></body></html>");
    h
}

fn sports_folder_label(folder: &str) -> String {
    folder.replace("::", " / ")
}

/// Stable, HTML-safe anchor id for a channel row (used to scroll back after Save/Move).
pub fn row_anchor_id(channel_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(channel_id.as_bytes());
    let mut out = String::with_capacity(19);
    out.push_str("ch-");
    for b in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Compact pagination window: always includes first, last, and `current - 2 ..= current + 2`.
fn windowed_pages(current: usize, total: usize) -> Vec<Option<usize>> {
    if total <= 9 {
        return (1..=total).map(Some).collect();
    }
    let mut pages = Vec::new();
    let start = current.saturating_sub(2).max(2);
    let end = (current + 2).min(total - 1);
    pages.push(Some(1));
    if start > 2 {
        pages.push(None);
    }
    for p in start..=end {
        pages.push(Some(p));
    }
    if end < total - 1 {
        pages.push(None);
    }
    pages.push(Some(total));
    pages
}

fn render_group_dropdown(
    out: &mut String,
    form: &str,
    name_suffix: &str,
    selected: Option<&str>,
    selected_folder: Option<&str>,
    automatic: &str,
    sports_folders: &std::collections::BTreeSet<String>,
    include_sports_folders: bool,
) {
    let keys = selected.map(|value| {
        value.split(',').filter_map(crate::lineup::aurelis_group_key).collect::<Vec<_>>().join(",")
    }).unwrap_or_default();
    out.push_str(&format!("<select {form} name=\"aurelis_group{name_suffix}\" aria-label=\"Aurelis group\">"));
    out.push_str(&format!("<option value=\"\"{}>{}</option>",
        if keys.is_empty() && selected_folder.is_none() { " selected" } else { "" }, escape(automatic)));
    if keys.contains(',') {
        out.push_str(&format!("<option value=\"{}\" selected>Keep current groups</option>", escape(&keys)));
    }
    for &(key, label) in crate::lineup::AURELIS_GROUPS {
        let selected = if key == keys && selected_folder.is_none() { " selected" } else { "" };
        out.push_str(&format!("<option value=\"{key}\"{selected}>{label}</option>"));
    }
    if include_sports_folders && !sports_folders.is_empty() {
        out.push_str("<optgroup label=\"Sports Extra folders\">");
        for folder in sports_folders {
            let selected = if selected_folder == Some(folder.as_str()) { " selected" } else { "" };
            out.push_str(&format!("<option value=\"SPORT_EXTRA::{}\"{selected}>{}</option>",
                escape(folder), escape(&sports_folder_label(folder))));
        }
        out.push_str("</optgroup>");
    } else if let Some(folder) = selected_folder {
        // Keep the channel's currently assigned Sports Extra folder selected without repeating all folders per row.
        out.push_str(&format!(
            "<option value=\"SPORT_EXTRA::{}\" selected>Sports Extra / {}</option>",
            escape(folder), escape(&sports_folder_label(folder))
        ));
    }
    out.push_str("</select>");
}

pub fn render_lineup(
    view: &PageView,
    lineup: &crate::lineup::LineupStore,
    operator: bool,
    query: &crate::lineup::ChannelQuery,
) -> String {
    render_lineup_with_flash(view, lineup, operator, query, None)
}

pub fn render_lineup_with_flash(
    view: &PageView,
    lineup: &crate::lineup::LineupStore,
    operator: bool,
    query: &crate::lineup::ChannelQuery,
    flash: Option<&crate::state::FlashNotice>,
) -> String {
    let mut h = String::new();
    h.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    h.push_str(&format!("<title>Channel Lineup - {}</title>", PRODUCT_NAME));
    h.push_str("<style>");
    h.push_str(include_str!("status_page.css"));
    h.push_str("</style></head><body><main>");
    h.push_str(&format!("<h1>{}</h1>", PRODUCT_NAME));

    h.push_str("<nav class=\"nav-tabs\">");
    h.push_str("<a href=\"./\" class=\"nav-tab\">System Status</a>");
    h.push_str("<a href=\"lineup\" class=\"nav-tab\">Channels</a>");
    h.push_str("<a href=\"lineup?view=table\" class=\"nav-tab active\">Spreadsheet view</a>");
    h.push_str("</nav>");

    if let Some(notice) = flash {
        let (cls, role) = match notice.kind {
            crate::state::FlashKind::Success => ("notice ok", "status"),
            crate::state::FlashKind::Error => ("notice err", "alert"),
        };
        h.push_str(&format!("<div class=\"{cls}\" role=\"{role}\">{}</div>", escape(&notice.message)));
    }

    let mut channels = lineup.get_channels();
    let total_count = channels.len();
    let overrides_count = lineup.overrides_count();

    let mut countries = std::collections::BTreeSet::new();
    let mut categories = std::collections::BTreeSet::new();
    let mut sports_folders = std::collections::BTreeSet::new();
    let mut group_counts = std::collections::HashMap::<crate::lineup::DialGroup, usize>::new();
    let mut group_positions = std::collections::HashMap::new();
    for ch in &channels {
        if let Some(cc) = ch.country_code.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let normalized = crate::lineup::dial_country(Some(cc));
            if normalized != "UNKNOWN" {
                countries.insert(normalized);
            }
        }
        if let Some(g) = ch.original_genre.as_deref().or(ch.genre.as_deref()).map(str::trim).filter(|s| !s.is_empty()) {
            categories.insert(g.to_string());
        }
        if let Some(folder) = ch.sports_extra_group.as_deref() {
            sports_folders.insert(folder.to_owned());
            if let Some((parent, _)) = folder.split_once("::") {
                sports_folders.insert(parent.to_owned());
            }
        }
        let group = crate::lineup::dial_group(ch);
        let count = group_counts.entry(group).or_default();
        *count += 1;
        group_positions.insert(ch.id.clone(), *count);
    }

    if let Some(c) = query.country.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let target = crate::lineup::dial_country(Some(c));
        channels.retain(|ch| crate::lineup::dial_country(ch.country_code.as_deref()) == target);
    }
    if let Some(cat) = query.category.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        channels.retain(|ch| crate::lineup::matches_category_filter(ch, cat));
    }
    if let Some(q) = query.search.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
        channels.retain(|ch| crate::lineup::matches_search_filter(ch, &q));
    }
    if query.edited_only {
        channels.retain(|ch| lineup.get_override(&ch.id).is_some());
    }
    if query.hidden_only {
        channels.retain(|ch| ch.hidden == Some(true));
    }
    if query.unnumbered_only {
        channels.retain(|ch| ch.channel_number.is_none());
    }
    lineup.sort_channels(&mut channels, query.sort.as_deref());

    let page_href = |p: Option<usize>| -> String {
        let mut params: Vec<String> = vec!["view=table".to_string()];
        if let Some(s) = query.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            params.push(format!("search={}", url_encode_query_val(s)));
        }
        if let Some(c) = query.country.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            params.push(format!("country={}", url_encode_query_val(c)));
        }
        if let Some(cat) = query.category.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            params.push(format!("category={}", url_encode_query_val(cat)));
        }
        if query.edited_only {
            params.push("edited=true".to_string());
        }
        if query.hidden_only {
            params.push("hidden=true".to_string());
        }
        if query.unnumbered_only {
            params.push("unnumbered=true".to_string());
        }
        if let Some(sort) = query.sort.as_deref().filter(|s| !s.is_empty()) {
            params.push(format!("sort={}", url_encode_query_val(sort)));
        }
        if let Some(page_num) = p {
            params.push(format!("page={page_num}"));
        }
        format!("lineup?{}", params.join("&"))
    };

    const PER_PAGE: usize = 100;
    let total = channels.len();
    let total_pages = total.div_ceil(PER_PAGE).max(1);
    let current_page = query.page.unwrap_or(1).clamp(1, total_pages);
    let current_redirect = page_href(if current_page > 1 { Some(current_page) } else { None });

    h.push_str("<section class=\"lineup-tools\"><h2>Spreadsheet view</h2>");
    h.push_str("<p><strong>For bulk changes and backups.</strong> To order channels and see them as the televisions do, use <a href=\"lineup\">Channels</a>.</p>");
    h.push_str("<p>Manage household channel assignments, permanent country/category moves, and dial numbers across all Aurelis TV boxes.</p>");
    h.push_str("<div class=\"stats-row\">");
    h.push_str(&format!("<span class=\"stat-chip\">Channels reported: <strong>{total_count}</strong></span>"));
    h.push_str(&format!("<span class=\"stat-chip\">Active overrides: <strong>{overrides_count}</strong></span>"));
    h.push_str(&format!("<span class=\"stat-chip\">Matching filter: <strong>{total}</strong></span>"));
    h.push_str("</div>");

    if !view.devices.is_empty() {
        let mut devices = view.devices.iter().collect::<Vec<_>>();
        devices.sort_by_key(|device| std::cmp::Reverse(device.last_seen_ms));
        h.push_str("<div class=\"sync-strip\" aria-label=\"TV lineup sync status\">");
        h.push_str("<span class=\"job-meta\">TV sync:</span>");
        for device in devices {
            let (cls, state_label, suffix) = match (device.synced_lineup_etag.as_deref(), device.last_lineup_sync_ms) {
                (Some(etag), Some(ts)) if etag == view.lineup_etag => (
                    "done",
                    "Up to date",
                    format!(" · {} ago", age(view.now_ms, ts.max(0) as u64)),
                ),
                (Some(_), Some(ts)) => (
                    "pending",
                    "Pending",
                    format!(" · {} ago", age(view.now_ms, ts.max(0) as u64)),
                ),
                _ => ("pending", "Not synced", String::new()),
            };
            h.push_str(&format!(
                "<span class=\"sync-chip\"><strong>{}</strong><span class=\"badge {cls}\">{state_label}</span><span class=\"job-meta\">{suffix}</span></span>",
                escape(&device.name),
            ));
        }
        h.push_str("</div>");
    }

    if operator {
        h.push_str("<datalist id=\"country-list\">");
        for c in &countries {
            h.push_str(&format!("<option value=\"{}\">", escape(c)));
        }
        h.push_str("</datalist>");
        h.push_str("<datalist id=\"sports-folder-list\">");
        for folder in &sports_folders {
            h.push_str(&format!("<option value=\"{}\">", escape(folder)));
        }
        h.push_str("</datalist>");
        h.push_str("<details style=\"margin-top: 0.75rem;\"><summary>Auto-Assign Dial Numbers by Aurelis Group</summary>");
        h.push_str("<form method=\"post\" action=\"ui/assign_numbers\" style=\"margin-top: 0.5rem;\">");
        h.push_str(&format!("<input type=\"hidden\" name=\"redirect_to\" value=\"{}\">", escape(&current_redirect)));
        h.push_str("<fieldset><legend>Group Numbering Plan</legend>");
        let country = query.country.as_deref().unwrap_or_default();
        h.push_str(&format!("<label>Country <input type=\"text\" name=\"country_code\" list=\"country-list\" value=\"{}\" placeholder=\"All countries\"></label>", escape(country)));
        let filter = query.category.as_deref().unwrap_or_default();
        let dial_folder = filter.strip_prefix("SPORT_EXTRA::").filter(|folder| sports_folders.contains(*folder));
        let dial_group = if dial_folder.is_some() {
            Some("SPORT_EXTRA")
        } else {
            crate::lineup::aurelis_group_key(filter.strip_prefix("AURELIS::").unwrap_or(filter))
        };
        h.push_str("<label>Aurelis group");
        render_group_dropdown(&mut h, "", "", dial_group, dial_folder, "Each group separately", &sports_folders, true);
        h.push_str("</label>");
        h.push_str("<label>Order <select name=\"order\">");
        for &(key, label) in crate::lineup::ASSIGN_ORDERS {
            h.push_str(&format!("<option value=\"{key}\">{}</option>", escape(label)));
        }
        h.push_str("</select></label>");
        h.push_str("<label>Start #: <input type=\"number\" name=\"start_number\" placeholder=\"101\" size=\"5\"></label> ");
        h.push_str("<label><input type=\"checkbox\" name=\"overwrite\" value=\"true\"> Overwrite existing numbers</label> ");
        h.push_str("<button type=\"submit\" class=\"btn-save\">Auto-Assign Numbers</button>");
        h.push_str("<small>Keep the current order renumbers without moving anything. Name A to Z re-sorts the whole group.</small>");
        h.push_str("</fieldset></form></details>");

        h.push_str("<details style=\"margin-top: 0.5rem;\"><summary>Add / Move Channel Override by ID</summary>");
        h.push_str("<form method=\"post\" action=\"ui/save_channel_override\" style=\"margin-top: 0.5rem;\">");
        h.push_str(&format!("<input type=\"hidden\" name=\"redirect_to\" value=\"{}\">", escape(&current_redirect)));
        h.push_str("<fieldset><legend>Manual Placement Override</legend>");
        h.push_str("<label>Channel ID: <input type=\"text\" name=\"channel_id\" required placeholder=\"channel id\"></label> ");
        h.push_str("<label>Name: <input type=\"text\" name=\"display_name\" placeholder=\"Display Name\"></label> ");
        h.push_str("<label>Country: <input type=\"text\" name=\"country_code\" list=\"country-list\" placeholder=\"GB\" size=\"5\"></label> ");
        h.push_str("<label>Aurelis group ");
        render_group_dropdown(&mut h, "", "", None, None, "Automatic", &sports_folders, true);
        h.push_str("</label>");
        h.push_str("<label>Number: <input type=\"number\" name=\"channel_number\" placeholder=\"101\" size=\"5\"></label> ");
        h.push_str("<label>Channel logo URL <input type=\"url\" name=\"logo_url\" placeholder=\"Provider logo\"></label>");
        h.push_str("<label><input type=\"checkbox\" name=\"hidden\" value=\"true\"> Hide</label> ");
        h.push_str("<button type=\"submit\" class=\"btn-save\">Save Override</button>");
        h.push_str("</fieldset></form></details>");

        h.push_str("<details style=\"margin-top: 0.5rem;\"><summary>Backup &amp; Restore Overrides (JSON)</summary>");
        h.push_str("<div class=\"import-box\">");
        h.push_str(&format!(
            "<div><a href=\"ui/export_overrides\" class=\"btn-secondary\">Export Overrides JSON ({overrides_count})</a></div>"
        ));
        h.push_str("<form method=\"post\" action=\"ui/import_overrides\" class=\"import-box\">");
        h.push_str(&format!("<input type=\"hidden\" name=\"redirect_to\" value=\"{}\">", escape(&current_redirect)));
        h.push_str("<label>Paste exported overrides JSON<textarea name=\"overrides_json\" rows=\"4\" placeholder='{ \"version\": 1, \"overrides\": [...] }'></textarea></label>");
        h.push_str("<div class=\"import-controls\">");
        h.push_str("<label>Import mode <select name=\"import_mode\"><option value=\"merge\">Merge with existing overrides</option><option value=\"replace\">Replace all existing overrides</option></select></label>");
        h.push_str("<button type=\"submit\" class=\"btn-save\">Import Overrides</button>");
        h.push_str("</div></form></div></details>");
    }

    h.push_str("</section>");

    // Filter bar
    let search_val = query.search.as_deref().unwrap_or("");
    let country_val = query.country.as_deref().map(str::trim).filter(|s| !s.is_empty())
        .map(|s| crate::lineup::dial_country(Some(s)))
        .unwrap_or_default();
    let cat_val = query.category.as_deref().unwrap_or("").trim();

    h.push_str("<form method=\"get\" action=\"lineup\" class=\"filter-bar\"><input type=\"hidden\" name=\"view\" value=\"table\">");
    h.push_str(&format!("<label>Search: <input type=\"text\" name=\"search\" value=\"{}\" placeholder=\"Name, dial #, ID, or folder\"></label>", escape(search_val)));

    h.push_str("<label>Country: <select name=\"country\">");
    h.push_str("<option value=\"\">All Countries</option>");
    for c in &countries {
        let sel = if *c == country_val { " selected" } else { "" };
        h.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", escape(c), escape(c)));
    }
    h.push_str("</select></label>");

    h.push_str("<label>Category: <select name=\"category\">");
    h.push_str("<option value=\"\">All Categories</option>");
    h.push_str("<optgroup label=\"Aurelis Categories\">");
    for &(key, label) in crate::lineup::AURELIS_GROUPS {
        let value = format!("AURELIS::{key}");
        let selected = if value == cat_val || label.eq_ignore_ascii_case(cat_val) { " selected" } else { "" };
        h.push_str(&format!("<option value=\"{value}\"{selected}>{}</option>", escape(label)));
    }
    h.push_str("</optgroup>");
    if !sports_folders.is_empty() {
        h.push_str("<optgroup label=\"Sports Extra folders\">");
        for folder in &sports_folders {
            let value = format!("SPORT_EXTRA::{folder}");
            let selected = if value == cat_val { " selected" } else { "" };
            h.push_str(&format!("<option value=\"{}\"{selected}>{}</option>", escape(&value), escape(&sports_folder_label(folder))));
        }
        h.push_str("</optgroup>");
    }
    h.push_str("<optgroup label=\"Provider Categories\">");
    for cat in &categories {
        if crate::lineup::AURELIS_GROUPS.iter().any(|&(_, label)| label.eq_ignore_ascii_case(cat)) {
            continue;
        }
        let sel = if cat.eq_ignore_ascii_case(cat_val) { " selected" } else { "" };
        h.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", escape(cat), escape(cat)));
    }
    h.push_str("</optgroup>");
    h.push_str("</select></label>");

    let edited_checked = if query.edited_only { " checked" } else { "" };
    let hidden_checked = if query.hidden_only { " checked" } else { "" };
    let unnumbered_checked = if query.unnumbered_only { " checked" } else { "" };
    h.push_str("<label>Sort: <select name=\"sort\">");
    let sort_val = query.sort.as_deref().unwrap_or("");
    for &(key, label) in crate::lineup::LIST_SORTS {
        let selected = if key == sort_val { " selected" } else { "" };
        h.push_str(&format!("<option value=\"{key}\"{selected}>{}</option>", escape(label)));
    }
    h.push_str("</select></label>");
    h.push_str(&format!("<label><input type=\"checkbox\" name=\"edited\" value=\"true\"{edited_checked}> Edited only</label>"));
    h.push_str(&format!("<label><input type=\"checkbox\" name=\"hidden\" value=\"true\"{hidden_checked}> Hidden only</label>"));
    h.push_str(&format!("<label><input type=\"checkbox\" name=\"unnumbered\" value=\"true\"{unnumbered_checked}> Unnumbered only</label>"));
    h.push_str("<button type=\"submit\" class=\"btn-save\">Filter</button>");
    h.push_str("<a href=\"lineup?view=table\" class=\"nav-tab\" style=\"padding: 0.3rem 0.6rem;\">Clear</a>");
    h.push_str("</form>");

    h.push_str("<section>");
    if channels.is_empty() {
        h.push_str("<p>No channels match the current filter or no channels have been reported by Aurelis TV boxes yet.</p>");
    } else {
        let start_idx = (current_page - 1) * PER_PAGE;
        let end_idx = (start_idx + PER_PAGE).min(total);

        let render_pager = |buf: &mut String| {
            buf.push_str("<div class=\"pager\">");
            buf.push_str(&format!(
                "<span class=\"job-meta\">Showing <strong>{}–{}</strong> of <strong>{}</strong> (Page {} of {})</span>",
                start_idx + 1,
                end_idx,
                total,
                current_page,
                total_pages
            ));
            if total_pages > 1 {
                buf.push_str("<nav class=\"pager-links\" aria-label=\"Pagination\">");
                if current_page > 1 {
                    buf.push_str(&format!("<a href=\"{}\" class=\"nav-tab\">← Prev</a>", escape(&page_href(Some(current_page - 1)))));
                }
                for item in windowed_pages(current_page, total_pages) {
                    match item {
                        Some(p) => {
                            let cls = if p == current_page { "nav-tab active" } else { "nav-tab" };
                            buf.push_str(&format!("<a href=\"{}\" class=\"{cls}\">{p}</a>", escape(&page_href(Some(p)))));
                        }
                        None => buf.push_str("<span class=\"pager-ellipsis\" aria-hidden=\"true\">…</span>"),
                    }
                }
                if current_page < total_pages {
                    buf.push_str(&format!("<a href=\"{}\" class=\"nav-tab\">Next →</a>", escape(&page_href(Some(current_page + 1)))));
                }
                buf.push_str("</nav>");
            }
            buf.push_str("</div>");
        };

        render_pager(&mut h);

        if operator {
            h.push_str("<form id=\"bulk_form\" method=\"post\" action=\"ui/bulk_channel_override\" class=\"bulk-bar\">");
            h.push_str(&format!("<input type=\"hidden\" name=\"redirect_to\" value=\"{}\">", escape(&current_redirect)));
            h.push_str("<label>Bulk action <select name=\"bulk_action\" aria-label=\"Bulk action\">");
            h.push_str("<option value=\"hide\">Hide selected</option>");
            h.push_str("<option value=\"unhide\">Unhide selected</option>");
            h.push_str("<option value=\"set_country\">Set country</option>");
            h.push_str("<option value=\"set_group\">Set Aurelis group</option>");
            h.push_str("<option value=\"clear_number\">Clear dial numbers</option>");
            h.push_str("<option value=\"reset\">Reset overrides</option>");
            h.push_str("</select></label>");
            h.push_str("<label>Country <input type=\"text\" name=\"country_code\" list=\"country-list\" placeholder=\"e.g. GB\" size=\"5\"></label>");
            h.push_str("<label>Group ");
            render_group_dropdown(&mut h, "", "", None, None, "Automatic", &sports_folders, true);
            h.push_str("</label>");
            h.push_str("<button type=\"submit\" class=\"btn-save\">Apply to selected</button>");
            h.push_str("</form>");
        }

        if operator {
            let shown = channels.len().saturating_sub(start_idx).min(PER_PAGE);
            h.push_str(&format!(
                "<form id=\"rows_form\" method=\"post\" action=\"ui/save_channel_rows\"><input type=\"hidden\" name=\"redirect_to\" value=\"{}\"><input type=\"hidden\" name=\"rows\" value=\"{shown}\"></form>",
                escape(&current_redirect)
            ));
            h.push_str("<div class=\"save-all-bar\"><button form=\"rows_form\" type=\"submit\" name=\"save_all\" value=\"1\" class=\"btn-save\">Save all changes</button><span class=\"job-meta\">Edit as many rows as you like, then save once. Only rows you changed are written; each row's Save still saves just that row.</span></div>");
        }
        if query.sort.as_deref().is_some_and(|s| !s.is_empty()) {
            h.push_str("<p class=\"job-meta\">Sorted for editing only. Televisions always show each group in TV order (dial number, then name).</p>");
        }
        h.push_str("<div class=\"table-wrap\"><table class=\"channel-table\">");
        h.push_str("<thead><tr>");
        if operator {
            h.push_str("<th>Sel</th>");
        }
        h.push_str("<th>Dial #</th><th>Channel</th><th>Country</th><th>Aurelis group</th><th>Hide</th>");
        if operator {
            h.push_str("<th>Actions</th>");
        }
        h.push_str("</tr></thead><tbody>");

        let mut row_forms = String::new();
        for (idx, ch) in channels.iter().skip(start_idx).take(PER_PAGE).enumerate() {
            let form_id = format!("f_{idx}");
            let del_id = format!("del_{idx}");
            let anchor_id = row_anchor_id(&ch.id);
            let num_str = ch.channel_number.map(|n| n.to_string()).unwrap_or_default();
            let is_hidden = ch.hidden == Some(true);
            let channel_override = lineup.get_override(&ch.id);
            let has_override = channel_override.is_some();

            if operator {
                row_forms.push_str(&format!(
                    "<input form=\"rows_form\" type=\"hidden\" name=\"channel_id__{idx}\" value=\"{}\"><input form=\"rows_form\" type=\"hidden\" name=\"row_edit__{idx}\" value=\"1\">",
                    escape(&ch.id)
                ));
                if has_override {
                    row_forms.push_str(&format!(
                        "<form id=\"{}\" method=\"post\" action=\"ui/delete_channel_override\"><input type=\"hidden\" name=\"channel_id\" value=\"{}\"><input type=\"hidden\" name=\"redirect_to\" value=\"{}\"></form>",
                        del_id, escape(&ch.id), escape(&current_redirect)
                    ));
                }
            }

            let mut row_classes: Vec<&str> = Vec::new();
            if has_override { row_classes.push("is-overridden"); }
            if is_hidden { row_classes.push("is-hidden"); }
            let row_class_attr = if row_classes.is_empty() {
                String::new()
            } else {
                format!(" class=\"{}\"", row_classes.join(" "))
            };

            let logo_img = ch.logo_url.as_deref()
                .filter(|url| crate::lineup::is_valid_logo_url(url))
                .map(|url| format!(
                    "<img class=\"ch-logo\" src=\"{}\" alt=\"\" loading=\"lazy\" decoding=\"async\" referrerpolicy=\"no-referrer\">",
                    escape(url.trim())
                ))
                .unwrap_or_default();

            h.push_str(&format!("<tr id=\"{anchor_id}\"{row_class_attr}>"));
            if operator {
                h.push_str(&format!(
                    "<td class=\"channel-select\" data-label=\"Select\"><input type=\"checkbox\" form=\"bulk_form\" name=\"channel_ids\" value=\"{}\" aria-label=\"Select {}\"></td>",
                    escape(&ch.id), escape(&ch.name)
                ));
                let group = crate::lineup::dial_group(ch);
                let current_position = group_positions[&ch.id];
                let group_size = group_counts[&group];
                let scope = escape(&group.to_string());
                let up_pos = current_position.saturating_sub(1).max(1);
                let down_pos = (current_position + 1).min(group_size);
                let up_disabled = if current_position <= 1 { " disabled" } else { "" };
                let down_disabled = if current_position >= group_size { " disabled" } else { "" };
                h.push_str(&format!(
                    "<td data-label=\"Dial number\"><div class=\"dial-controls\"><input aria-label=\"Dial number\" form=\"rows_form\" type=\"number\" name=\"channel_number__{idx}\" value=\"{}\" placeholder=\"–\"><button form=\"rows_form\" type=\"submit\" name=\"quick_move\" value=\"{idx}:{up_pos}\" class=\"btn-move\" title=\"Move up\" aria-label=\"Move up\"{up_disabled}>▲</button><button form=\"rows_form\" type=\"submit\" name=\"quick_move\" value=\"{idx}:{down_pos}\" class=\"btn-move\" title=\"Move down\" aria-label=\"Move down\"{down_disabled}>▼</button></div>",
                    escape(&num_str),
                ));
                h.push_str(&format!(
                    "<label class=\"dial-position\">Move to position <input form=\"rows_form\" type=\"number\" name=\"move_to_position__{idx}\" min=\"1\" placeholder=\"{current_position}\" aria-describedby=\"position-{form_id}\"></label><small id=\"position-{form_id}\">{current_position} of {group_size} · {scope}</small></td>",
                ));
                let orig_note = if let Some(ref orig) = ch.original_name {
                    format!(" · Auto: {}", escape(orig))
                } else {
                    String::new()
                };
                let edited_badge = if has_override {
                    "<span class=\"badge edited\">Edited</span>"
                } else {
                    ""
                };
                let logo_override = channel_override.as_ref().and_then(|row| row.logo_url.as_deref()).unwrap_or("");
                let logo_summary = if logo_override.is_empty() {
                    "Channel logo"
                } else {
                    "Channel logo (custom)"
                };
                h.push_str(&format!(
                    "<td class=\"channel-name\" data-label=\"Channel\"><div class=\"ch-title-row\">{logo_img}<input aria-label=\"Channel name\" form=\"rows_form\" type=\"text\" name=\"display_name__{idx}\" value=\"{}\"></div><div class=\"channel-id-pill\">{edited_badge}<code>{}</code>{orig_note}</div>",
                    escape(&ch.name), escape(&ch.id)
                ));
                let automatic_group = crate::lineup::resolve_automatic_aurelis_category(ch);
                let effective_group = crate::lineup::resolve_aurelis_category(ch);
                let folder_override = channel_override.as_ref().and_then(|row| row.sports_extra_group.as_deref()).unwrap_or("");
                let show_folder_inline = effective_group == "Sports Extra" || automatic_group == "Sports Extra" || !folder_override.is_empty();
                h.push_str(&format!(
                    "<details class=\"channel-logo\"><summary>{logo_summary}</summary><input aria-label=\"Channel logo URL\" form=\"rows_form\" type=\"url\" name=\"logo_url__{idx}\" value=\"{}\" placeholder=\"Provider logo\"><small>Leave blank to use the provider logo.</small>",
                    escape(logo_override),
                ));
                if !show_folder_inline && !sports_folders.is_empty() {
                    h.push_str(&format!(
                        "<input aria-label=\"Sports Extra folder\" form=\"rows_form\" class=\"sports-folder-input\" type=\"text\" name=\"sports_extra_group__{idx}\" list=\"sports-folder-list\" value=\"\" placeholder=\"Move to Sports Extra folder\"><small>Optional: moves this channel into a Sports Extra folder.</small>"
                    ));
                }
                h.push_str("</details></td>");
                h.push_str(&format!(
                    "<td data-label=\"Country\"><input aria-label=\"Country\" form=\"rows_form\" class=\"input-country\" type=\"text\" name=\"country_code__{idx}\" list=\"country-list\" value=\"{}\" size=\"4\"></td>",
                    escape(ch.country_code.as_deref().unwrap_or_default())
                ));
                let automatic = match ch.sports_extra_group.as_deref() {
                    Some(folder) if automatic_group == "Sports Extra" && channel_override.as_ref().is_none_or(|row| row.sports_extra_group.is_none()) =>
                        format!("Automatic · {automatic_group} / {}", sports_folder_label(folder)),
                    _ => format!("Automatic · {automatic_group}"),
                };
                h.push_str("<td class=\"channel-group\" data-label=\"Aurelis group\">");
                render_group_dropdown(
                    &mut h, &format!("form=\"rows_form\""), &format!("__{idx}"),
                    channel_override.as_ref().and_then(|row| row.category.as_deref()),
                    channel_override.as_ref().and_then(|row| row.sports_extra_group.as_deref()),
                    &automatic, &sports_folders, false,
                );
                if show_folder_inline && !sports_folders.is_empty() {
                    let placeholder = ch.sports_extra_group.as_deref()
                        .filter(|_| folder_override.is_empty())
                        .unwrap_or("Sports Extra folder");
                    if !folder_override.is_empty() {
                        h.push_str(&format!(
                            "<input form=\"rows_form\" type=\"hidden\" name=\"prev_sports_extra_group__{idx}\" value=\"{}\">",
                            escape(folder_override)
                        ));
                    }
                    h.push_str(&format!(
                        "<input aria-label=\"Sports Extra folder\" form=\"rows_form\" class=\"sports-folder-input\" type=\"text\" name=\"sports_extra_group__{idx}\" list=\"sports-folder-list\" value=\"{}\" placeholder=\"{}\">",
                        escape(folder_override), escape(placeholder)
                    ));
                }
                if let Some(provider) = ch.original_genre.as_deref().or(ch.genre.as_deref()).filter(|s| !s.is_empty()) {
                    h.push_str(&format!("<div class=\"channel-id-pill\">Provider: {}</div>", escape(provider)));
                }
                h.push_str("</td>");
                let checked_attr = if is_hidden { "checked" } else { "" };
                h.push_str(&format!(
                    "<td class=\"channel-hidden\" data-label=\"Hide\"><input aria-label=\"Hide channel\" form=\"rows_form\" type=\"checkbox\" name=\"hidden__{idx}\" value=\"true\" {}></td>",
                    checked_attr
                ));
                h.push_str("<td class=\"channel-actions\">");
                h.push_str(&format!("<button form=\"rows_form\" type=\"submit\" name=\"save_row\" value=\"{idx}\" class=\"btn-save\">Save</button> "));
                if has_override {
                    h.push_str(&format!("<button form=\"{}\" type=\"submit\" class=\"btn-reset\">Reset</button>", del_id));
                }
                h.push_str("</td>");
            } else {
                h.push_str(&format!("<td data-label=\"Dial number\">{}</td>", escape(&num_str)));
                h.push_str(&format!(
                    "<td class=\"channel-name\" data-label=\"Channel\"><div class=\"ch-title-row\">{logo_img}<span>{}</span></div><div class=\"channel-id-pill\"><code>{}</code></div></td>",
                    escape(&ch.name), escape(&ch.id)
                ));
                h.push_str(&format!("<td data-label=\"Country\">{}</td>", escape(ch.country_code.as_deref().unwrap_or("-"))));
                let aurelis_group = crate::lineup::resolve_aurelis_category(ch);
                h.push_str(&format!("<td class=\"channel-group\" data-label=\"Aurelis group\">{}", escape(aurelis_group)));
                if aurelis_group == "Sports Extra" {
                    if let Some(folder) = ch.sports_extra_group.as_deref() {
                        h.push_str(&format!("<div class=\"channel-id-pill\">{}</div>", escape(&sports_folder_label(folder))));
                    }
                }
                if let Some(provider) = ch.original_genre.as_deref().or(ch.genre.as_deref()).filter(|s| !s.is_empty()) {
                    h.push_str(&format!("<div class=\"channel-id-pill\">Provider: {}</div>", escape(provider)));
                }
                h.push_str("</td>");
                h.push_str(&format!("<td class=\"channel-hidden\" data-label=\"Hidden\">{}</td>", if is_hidden { "Yes" } else { "No" }));
            }
            h.push_str("</tr>");
        }
        h.push_str("</tbody></table></div>");
        h.push_str(&row_forms);
        render_pager(&mut h);
    }
    h.push_str("</section>");

    h.push_str("</main></body></html>");
    h
}

fn url_encode_query_val(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Summarises the transport result using the call name, HTTP status and any relay fault.
pub fn betfair_line(view: &PageView) -> String {
    let c = &view.card;
    let state = match c.declined.get(crate::betfair_relay::CAPABILITY) {
        None => "on".to_string(),
        Some(why) => format!("off ({why})"),
    };
    let last = match &view.betfair_last {
        None => "no calls yet".to_string(),
        Some(l) => {
            let ago = match age(view.now_ms, l.at_ms.max(0) as u64).as_str() {
                "under a minute" => "under a minute ago".to_string(),
                a => format!("{a} ago"),
            };
            let outcome = match &l.fault {
                Some(why) => format!("Relay failed: {why}"),
                None if l.http_code >= 400 => "Betfair returned an HTTP error".to_string(),
                None => "Betfair replied".to_string(),
            };
            format!("{outcome} · {} · {ago} (HTTP {})", l.call, l.http_code)
        }
    };
    format!("Betfair relay: {state} \u{b7} {last}")
}

fn state_name(state: crate::ai_jobs::JobState) -> &'static str {
    use crate::ai_jobs::JobState::*;
    match state {
        Queued => "queued",
        Running => "running",
        Done => "done",
        Failed => "failed",
        Expired => "expired",
    }
}

/// How long ago a job was handed over: `under a minute`, `12 min`, `3 h`, `2 d`.
fn age(now_ms: i64, then_ms: u64) -> String {
    let mins = (now_ms.max(0) as u64).saturating_sub(then_ms) / 60_000;
    match mins {
        0 => "under a minute".to_string(),
        m if m < 60 => format!("{m} min"),
        m if m < 48 * 60 => format!("{} h", m / 60),
        m => format!("{} d", m / (24 * 60)),
    }
}

/// The host part of an Origin or Referer value: `https://nuc.local:8765/x` -> `nuc.local:8765`.
fn authority(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?;
    if auth.is_empty() {
        None
    } else {
        Some(auth.to_ascii_lowercase())
    }
}

/// Cross-site refusal for the pause form. A post is allowed when neither `Origin` nor
/// `Referer` is present (not a browser) or when whichever is present names the same host the
/// request was addressed to. `forwarded_host` is the proxy's name for it (ingress); a
/// cross-site page cannot set that header, so accepting it opens nothing.
/// [`same_origin`], also reading the browser's own `Sec-Fetch-Site`, which a page cannot set.
///
/// Inside Home Assistant's panel the browser sends `Origin: null` with no referrer (a
/// no-referrer policy turns the origin of a form post or fetch into "null"), so every edit and
/// delete was refused with "the request's origin 'null' does not name a host" (25 Nov 2026).
/// A browser that says `same-origin` is believed; one that says `cross-site` or `same-site` is
/// refused whatever the origin; one that sends nothing falls back to the origin check.
pub fn same_origin_checked(
    host: Option<&str>,
    forwarded_host: Option<&str>,
    origin: Option<&str>,
    referer: Option<&str>,
    sec_fetch_site: Option<&str>,
) -> Result<(), String> {
    match sec_fetch_site.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("same-origin") => Ok(()),
        Some(site @ ("cross-site" | "same-site")) => Err(format!("cross-site request refused: the browser marked it {site}")),
        _ => same_origin(host, forwarded_host, origin, referer),
    }
}

pub fn same_origin(host: Option<&str>, forwarded_host: Option<&str>, origin: Option<&str>, referer: Option<&str>) -> Result<(), String> {
    let claimed = match (origin, referer) {
        (None, None) => return Ok(()),
        (Some(o), _) => o,
        (None, Some(r)) => r,
    };
    let Some(claimed_host) = authority(claimed) else {
        return Err(format!("the request's origin '{claimed}' does not name a host"));
    };
    let ours = [host, forwarded_host];
    if ours.iter().flatten().any(|h| h.trim().eq_ignore_ascii_case(&claimed_host)) {
        Ok(())
    } else {
        Err(format!("cross-site request refused: origin host {claimed_host} is not this server"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{test_config, test_state};

    #[test]
    fn page_is_titled_and_uses_only_relative_urls() {
        let s = test_state(test_config());
        let html = render(&s.page_view(), true);
        assert!(html.contains("<title>Aurelis Companion</title>"));
        assert!(html.contains("action=\"ui/pause\""));
        assert!(!html.contains("href=\"/") && !html.contains("action=\"/") && !html.contains("src="));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn page_renders_dvr_recordings_and_disk_usage() {
        let s = test_state(test_config());
        let html = render(&s.page_view(), true);
        assert!(html.contains("<h2>DVR Recordings</h2>"));
        assert!(html.contains("No recordings scheduled or saved."));
        assert!(html.contains("<tr><th>DVR</th><td>0 MB</td></tr>"));
        assert!(html.contains("<tr><th>Household DVR</th><td>on</td></tr>"));
    }

    #[test]
    fn page_renders_stream_relays_section() {
        let s = test_state(test_config());
        let html = render(&s.page_view(), true);
        assert!(html.contains("<h2>Stream Relays</h2>"));
        assert!(html.contains("No live stream relays active."));
    }

    #[test]
    fn page_says_why_a_capability_is_off_and_shows_busy_reason() {
        let s = test_state(test_config());
        s.set_paused(true);
        s.push_job("<b>x</b>");
        let html = render(&s.page_view(), true);
        assert!(html.contains("switched off in the Companion&#39;s options (pictures)"));
        assert!(html.contains("paused on the status page"));
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt;"), "job lines are escaped");
        assert!(html.contains("value=\"resume\""));
    }

    #[test]
    fn guide_ids_are_cut_to_twelve_characters() {
        // A guide id is an unsalted SHA-256 prefix of a URL that can carry an Xtream login: any LAN viewer
        // sees this page, so it shows only the 12 characters the television prints.
        let s = test_state(test_config());
        let mut view = s.page_view();
        let id = format!("g{}", "0123456789abcdef".repeat(2));
        view.guides = vec![(id.clone(), String::new(), "parsed 3 programmes in 1 s".to_string())];
        let html = render(&view, false);
        assert!(html.contains(&format!("<td>{}</td>", &id[..12])), "{html}");
        assert!(!html.contains(&id[..13]), "{html}");
    }

    #[test]
    fn pairing_code_shows_only_while_unpaired() {
        let s = test_state(test_config());
        s.set_pairing(false, None, Some("ABCD2345".into()));
        assert!(render(&s.page_view(), true).contains("ABCD2345"));
        s.set_pairing(true, Some("a1b2c3".into()), Some("ABCD2345".into()));
        let html = render(&s.page_view(), true);
        assert!(!html.contains("ABCD2345") && html.contains("a1b2c3"));
    }

    #[test]
    fn operator_peer_test() {
        use std::net::IpAddr;
        let ip = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        assert!(is_operator(ip("127.0.0.1"), false));
        assert!(is_operator(ip("::1"), false));
        assert!(is_operator(ip("::ffff:127.0.0.1"), false));
        assert!(is_operator(ip("172.30.32.2"), true));
        assert!(!is_operator(ip("172.30.32.2"), false));
        assert!(!is_operator(ip("192.168.3.50"), true), "a forged X-Ingress-Path from the LAN");
        assert!(!is_operator(ip("192.168.3.50"), false));
        assert!(!is_operator(None, true));
    }

    #[test]
    fn off_host_page_hides_the_code_and_the_unpair_button() {
        let s = test_state(test_config());
        s.set_pairing(false, None, Some("ABCD2345".into()));
        let html = render(&s.page_view(), false);
        assert!(!html.contains("ABCD2345") && html.contains("Pairing code hidden on the network"));
        s.set_pairing(true, Some("a1b2c3".into()), None);
        assert!(!render(&s.page_view(), false).contains("ui/unpair"));
        assert!(render(&s.page_view(), true).contains("action=\"ui/unpair\""));
        s.set_pairing(false, None, None);
        assert!(!render(&s.page_view(), true).contains("ui/unpair"));
    }

    #[test]
    fn off_host_page_hides_the_pause_form() {
        let s = test_state(test_config());
        assert!(!render(&s.page_view(), false).contains("ui/pause"));
        assert!(render(&s.page_view(), true).contains("action=\"ui/pause\""));
    }

    #[test]
    fn betfair_line_names_the_last_call_and_nothing_else() {
        use crate::betfair_relay::LastCall;
        let s = test_state(test_config());
        let mut view = s.page_view();
        assert_eq!(betfair_line(&view), "Betfair relay: on \u{b7} no calls yet");
        view.betfair_last = Some(LastCall { at_ms: view.now_ms - 3 * 60_000, host: "api.betfair.com".into(), call: "listMarketBook".into(), http_code: 200, fault: None, took_ms: 312 });
        assert_eq!(betfair_line(&view), "Betfair relay: on \u{b7} Betfair replied · listMarketBook · 3 min ago (HTTP 200)");
        view.betfair_last = Some(LastCall { at_ms: view.now_ms, host: "identitysso.betfair.com".into(), call: "login".into(), http_code: 502, fault: Some("no reply from Betfair within 14 s".into()), took_ms: 14_000 });
        assert_eq!(betfair_line(&view), "Betfair relay: on \u{b7} Relay failed: no reply from Betfair within 14 s · login · under a minute ago (HTTP 502)");
        let last = view.betfair_last.as_mut().unwrap();
        last.http_code = 403;
        last.fault = None;
        assert_eq!(betfair_line(&view), "Betfair relay: on · Betfair returned an HTTP error · login · under a minute ago (HTTP 403)");
        assert!(render(&view, false).contains("Betfair relay: on"), "shown off the host too: it holds nothing private");
    }

    #[test]
    fn lineup_page_is_titled_and_uses_only_relative_urls() {
        let s = test_state(test_config());
        let q = crate::lineup::ChannelQuery::default();
        let html = render_lineup(&s.page_view(), &s.lineup, true, &q);
        assert!(html.contains("<title>Channel Lineup - Aurelis Companion</title>"));
        assert!(html.contains("class=\"nav-tab active\""));
        assert!(html.contains("action=\"ui/save_channel_override\""));
        assert!(!html.contains("href=\"/") && !html.contains("action=\"/") && !html.contains("src="));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn origin_rules() {
        let host = Some("nuc.local:8765");
        assert!(same_origin(host, None, None, None).is_ok());
        assert!(same_origin(host, None, Some("http://NUC.local:8765"), None).is_ok());
        assert!(same_origin(host, None, None, Some("http://nuc.local:8765/")).is_ok());
        assert!(same_origin(host, None, Some("http://evil.example"), None).is_err());
        assert!(same_origin(host, None, Some("null"), None).is_err());
        // Home Assistant's panel: Origin null, but the browser attests the request is same-origin.
        assert!(same_origin_checked(host, None, Some("null"), None, Some("same-origin")).is_ok());
        assert!(same_origin_checked(host, None, Some("null"), None, None).is_err());
        assert!(same_origin_checked(host, None, Some("http://nuc.local:8765"), None, Some("cross-site")).is_err());
        assert!(same_origin_checked(host, None, Some("http://nuc.local:8765"), None, Some("none")).is_ok());
        // Origin outranks a friendly Referer
        assert!(same_origin(host, None, Some("http://evil.example"), Some("http://nuc.local:8765/")).is_err());
        // ingress: the browser's host is the proxy's, named in X-Forwarded-Host
        assert!(same_origin(Some("172.30.32.2:8099"), Some("ha.local:8123"), Some("http://ha.local:8123"), None).is_ok());
        assert!(same_origin(host, None, Some("http://nuc.local:9999"), None).is_err());
    }

    #[tokio::test]
    async fn a_recording_can_be_removed_from_the_status_page() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let dir = crate::household::TempDir::new();
        let mut config = crate::state::test_config();
        config.data_dir = dir.0.clone();
        let state = std::sync::Arc::new(crate::state::test_state(config));
        let now = crate::state::now_ms();
        let rec = state.dvr.schedule(crate::dvr::ScheduleRequest {
            channel_id: "c4".into(), channel_name: "Channel 4".into(), title: "Help! We Bought a Village".into(), description: None,
            stream_url: "http://example.invalid/x.ts".into(), start_ms: now + 3_600_000, end_ms: now + 7_200_000, device_name: "tv".into(),
        }, 100_000).unwrap();
        let html = render(&state.page_view(), true);
        assert!(html.contains("action=\"ui/dvr_delete\"") && html.contains("Cancel this scheduled recording?"));
        assert!(!render(&state.page_view(), false).contains("ui/dvr_delete"), "read-only viewers get no button");
        let app = crate::router(state.clone());
        let send = |origin: &'static str| app.clone().oneshot(Request::builder().method("POST").uri("/ui/dvr_delete")
            .header("host", "ha.local:8765").header("origin", origin)
            .extension(axum::extract::ConnectInfo("127.0.0.1:8765".parse::<std::net::SocketAddr>().unwrap()))
            .body(Body::from(format!("id={}", rec.id))).unwrap());
        assert_eq!(send("http://evil.example").await.unwrap().status(), axum::http::StatusCode::FORBIDDEN);
        assert_eq!(send("http://ha.local:8765").await.unwrap().status(), axum::http::StatusCode::SEE_OTHER);
        assert!(state.dvr.get(&rec.id).is_none());
    }
}

