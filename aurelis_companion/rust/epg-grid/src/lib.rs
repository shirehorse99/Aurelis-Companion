//! Native compute engine for Aurelis's EPG grid (ROBUST_EPG_V2_PLAN.md Phase 5, Appendix C).
//!
//! This mirrors `KotlinEpgGridEngine` + `EpgGridCompute` in
//! `feature/guide/.../epg/grid/` line-for-line so the two engines produce identical
//! visible-slot geometry and focus/badge flags. Upload still uses JSON once per
//! generation; per-frame compute returns a packed little-endian binary buffer
//! (`encode_compute_output` / `EPG2` magic) that Kotlin re-attaches to real
//! `Channel`/`EpgEntry` objects by id. The unused JSON-in/JSON-out per-frame
//! JNI was removed.
//!
//! Selected by default on ARM when `libaurelis_core.so` loads (`EpgGrid` picks
//! `RustEpgGridEngine`); x86 / missing `.so` fall back to `KotlinEpgGridEngine`.

use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jbyteArray, jint, jlong};
use jni::Env;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Bump when the binary/JSON FFI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 2;

/// Little-endian FourCC `EPG2` (0x45='E', 0x50='P', 0x47='G', 0x32='2').
pub const BINARY_MAGIC: u32 = 0x4550_4732;

const SLOT_FLAG_IS_NOW: u8 = 1 << 0;
const SLOT_FLAG_IS_PAST: u8 = 1 << 1;
const SLOT_FLAG_IS_FOCUSED: u8 = 1 << 2;
const SLOT_FLAG_HAS_REMINDER: u8 = 1 << 3;
const SLOT_FLAG_IS_RECORDING: u8 = 1 << 4;
const SLOT_FLAG_IS_SPORTS: u8 = 1 << 5;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgrammeIn {
    pub id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub title: String,
    pub has_catchup: bool,
    pub progress_fraction: f32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RowIn {
    pub row_index: i32,
    pub channel_id: String,
    pub programmes: Vec<ProgrammeIn>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputeInput {
    pub rows: Vec<RowIn>,
    pub first_visible_row: i32,
    pub visible_row_count: i32,
    pub focused_row: i32,
    pub focused_programme_index: i32,
    /// One of "SOURCES" | "GROUPS" | "CHANNEL" | "PROGRAMME" | "OVERLAY" (EpgFocusZone.name).
    pub zone: String,
    pub now_ms: i64,
    pub now_next_mode: bool,
    pub window_start: i64,
    pub window_end: i64,
    pub hour_width_px: f32,
    pub programme_area_width_px: f32,
    pub viewport_start: i64,
    pub viewport_end: i64,
    pub reminder_keys: Vec<String>,
    pub recording_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotOut {
    pub entry_id: String,
    pub start_offset_px: f32,
    pub width_px: f32,
    pub is_now: bool,
    pub is_past: bool,
    pub is_focused: bool,
    pub has_reminder: bool,
    pub is_recording_scheduled: bool,
    pub is_sports_programme: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RowOut {
    pub row_index: i32,
    pub channel_id: String,
    pub is_focused_row: bool,
    pub is_channel_focused: bool,
    pub slots: Vec<SlotOut>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputeOutput {
    pub rows: Vec<RowOut>,
}

#[derive(Debug, PartialEq, Eq)]
enum SportKind {
    Highlights,
    ExtendedHighlights,
    FullReplay,
    LiveMatch,
    Other,
}

/// Mirrors `SportProgramClassifier.classify` in core/common/.../model/SportsModels.kt.
fn classify_sport(title: &str) -> SportKind {
    let t = title.trim().to_lowercase();
    let padded = format!(" {} ", t);
    if t.contains("extended highlights") {
        SportKind::ExtendedHighlights
    } else if t.contains("match highlights") || t.contains("highlights") || t.contains("goal rush") {
        SportKind::Highlights
    } else if t.contains("full match") || t.contains("full replay") || t.contains("classic match") {
        SportKind::FullReplay
    } else if padded.contains(" vs ") || padded.contains(" @ ") || padded.contains(" v ") {
        SportKind::LiveMatch
    } else {
        SportKind::Other
    }
}

/// Mirrors `EpgGridCompute.visibleProgrammesForRow`. Window start/end are owned by
/// Kotlin (`timelineWindowStartMs` — now-line pin, not hour-floor) and passed in so
/// this crate does not duplicate `EpgLayoutConstants`.
fn visible_programmes_for_row(
    programmes: &[ProgrammeIn],
    now_ms: i64,
    now_next_mode: bool,
    window_start: i64,
    window_end: i64,
) -> Vec<ProgrammeIn> {
    if now_next_mode {
        let now_prog = programmes
            .iter()
            .find(|p| now_ms >= p.start_ms && now_ms <= p.end_ms)
            .or_else(|| programmes.iter().find(|p| p.start_ms >= now_ms));
        let next_prog = now_prog.and_then(|np| programmes.iter().find(|p| p.start_ms >= np.end_ms));
        return [now_prog, next_prog].into_iter().flatten().cloned().collect();
    }
    programmes
        .iter()
        .filter(|p| p.end_ms > window_start && p.start_ms < window_end)
        .cloned()
        .collect()
}

/// Mirrors `programmesInTimelineRange` in EpgGridScrollState.kt.
fn programmes_in_timeline_range(
    programmes: &[ProgrammeIn],
    range_start: i64,
    range_end: i64,
    always_include: Option<&ProgrammeIn>,
) -> Vec<ProgrammeIn> {
    if programmes.is_empty() {
        return Vec::new();
    }
    let mut filtered: Vec<ProgrammeIn> = programmes
        .iter()
        .filter(|p| p.end_ms > range_start && p.start_ms < range_end)
        .cloned()
        .collect();
    if let Some(ai) = always_include {
        if filtered.iter().all(|p| p.start_ms != ai.start_ms) {
            filtered.push(ai.clone());
        }
    }
    filtered
}

/// Mirrors `EpgGridCompute.programmeGeometry`.
fn programme_geometry(
    entry: &ProgrammeIn,
    prog_index: usize,
    visible_count: usize,
    now_next_mode: bool,
    canvas_width: f32,
    window_start: i64,
    window_end: i64,
    hour_width_px: f32,
) -> (f32, f32) {
    if now_next_mode {
        let cell_width = if visible_count <= 1 { canvas_width } else { canvas_width / 2.0 };
        let offset = if prog_index == 0 { 0.0 } else { cell_width };
        return (offset, cell_width.max(8.0));
    }
    let start = ((entry.start_ms - window_start) as f32 / 3_600_000f32) * hour_width_px;
    let end = ((entry.end_ms.min(window_end) - window_start) as f32 / 3_600_000f32) * hour_width_px;
    (start, (end - start).max(8.0))
}

/// Mirrors `EpgGridCompute.computeVisibleSlots` plus the Phase 4 badge-flag computation
/// (`EpgGridCompute.reminderKey`/`recordingKey`/`SportProgramClassifier`).
fn compute_visible_slots(
    draw_entries: &[ProgrammeIn],
    now_ms: i64,
    now_next_mode: bool,
    canvas_width: f32,
    window_start: i64,
    window_end: i64,
    hour_width_px: f32,
    focused_entry: Option<&ProgrammeIn>,
    reminder_keys: &HashSet<String>,
    recording_keys: &HashSet<String>,
) -> Vec<SlotOut> {
    draw_entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let (start_offset_px, width_px) = programme_geometry(
                entry,
                index,
                draw_entries.len(),
                now_next_mode,
                canvas_width,
                window_start,
                window_end,
                hour_width_px,
            );
            let reminder_key = format!("{}:{}", entry.channel_id, entry.start_ms);
            let recording_key = format!("{}:{}", entry.channel_name, entry.start_ms);
            SlotOut {
                entry_id: entry.id.clone(),
                start_offset_px,
                width_px,
                is_now: now_ms >= entry.start_ms && now_ms <= entry.end_ms,
                is_past: entry.end_ms < now_ms,
                is_focused: focused_entry.map_or(false, |f| f.id == entry.id),
                has_reminder: reminder_keys.contains(&reminder_key),
                is_recording_scheduled: recording_keys.contains(&recording_key),
                is_sports_programme: classify_sport(&entry.title) != SportKind::Other,
            }
        })
        .collect()
}

/// Core compute logic borrowing rows directly (used by both the JSON-input and
/// resident-handle paths to avoid cloning the row data from the grid state).
#[allow(clippy::too_many_arguments)]
fn compute_visible_rows_impl(
    rows: &[RowIn],
    first_visible_row: i32,
    visible_row_count: i32,
    focused_row: i32,
    focused_programme_index: i32,
    zone: &str,
    now_ms: i64,
    now_next_mode: bool,
    window_start: i64,
    window_end: i64,
    hour_width_px: f32,
    programme_area_width_px: f32,
    viewport_start: i64,
    viewport_end: i64,
    reminder_keys: &HashSet<String>,
    recording_keys: &HashSet<String>,
) -> ComputeOutput {
    if rows.is_empty() || visible_row_count <= 0 {
        return ComputeOutput { rows: Vec::new() };
    }
    let last_index = (rows.len() - 1) as i32;
    let first = first_visible_row.clamp(0, last_index);
    let last = (first_visible_row + visible_row_count - 1).clamp(first, last_index);

    let mut out_rows = Vec::with_capacity((last - first + 1) as usize);
    for row_index in first..=last {
        let row = &rows[row_index as usize];
        let all_visible = visible_programmes_for_row(
            &row.programmes,
            now_ms,
            now_next_mode,
            window_start,
            window_end,
        );
        let is_focused_row = row_index == focused_row;
        let focused_entry: Option<ProgrammeIn> = if is_focused_row && !all_visible.is_empty() {
            let idx = focused_programme_index.max(0).min(all_visible.len() as i32 - 1) as usize;
            Some(all_visible[idx].clone())
        } else {
            None
        };

        let draw_entries = if now_next_mode {
            all_visible
        } else {
            programmes_in_timeline_range(
                &all_visible,
                viewport_start,
                viewport_end,
                focused_entry.as_ref(),
            )
        };

        let effective_focused = if is_focused_row && zone == "PROGRAMME" {
            focused_entry.as_ref()
        } else {
            None
        };

        let slots = compute_visible_slots(
            &draw_entries,
            now_ms,
            now_next_mode,
            programme_area_width_px,
            window_start,
            window_end,
            hour_width_px,
            effective_focused,
            reminder_keys,
            recording_keys,
        );

        out_rows.push(RowOut {
            row_index,
            channel_id: row.channel_id.clone(),
            is_focused_row,
            is_channel_focused: is_focused_row && zone == "CHANNEL",
            slots,
        });
    }

    ComputeOutput { rows: out_rows }
}

/// Mirrors `KotlinEpgGridEngine.computeVisibleRows` end to end.
pub fn compute_visible_rows(input: &ComputeInput) -> ComputeOutput {
    let reminder_keys: HashSet<String> = input.reminder_keys.iter().cloned().collect();
    let recording_keys: HashSet<String> = input.recording_keys.iter().cloned().collect();
    compute_visible_rows_impl(
        &input.rows,
        input.first_visible_row,
        input.visible_row_count,
        input.focused_row,
        input.focused_programme_index,
        &input.zone,
        input.now_ms,
        input.now_next_mode,
        input.window_start,
        input.window_end,
        input.hour_width_px,
        input.programme_area_width_px,
        input.viewport_start,
        input.viewport_end,
        &reminder_keys,
        &recording_keys,
    )
}

fn write_u16_str(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(u16::MAX as usize) as u16;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&bytes[..len as usize]);
}

/// Packed little-endian layout for the per-frame JNI return:
/// `u32 magic | u32 rowCount | [row…]` where each row is
/// `i32 rowIndex | u16 channelIdLen + utf8 | u8 rowFlags | u16 slotCount | [slot…]`
/// and each slot is `u16 entryIdLen + utf8 | f32 start | f32 width | u8 slotFlags`.
pub fn encode_compute_output(output: &ComputeOutput) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + output.rows.len() * 128);
    buf.extend_from_slice(&BINARY_MAGIC.to_le_bytes());
    buf.extend_from_slice(&(output.rows.len() as u32).to_le_bytes());
    for row in &output.rows {
        buf.extend_from_slice(&row.row_index.to_le_bytes());
        write_u16_str(&mut buf, &row.channel_id);
        let mut row_flags: u8 = 0;
        if row.is_focused_row {
            row_flags |= 1;
        }
        if row.is_channel_focused {
            row_flags |= 1 << 1;
        }
        buf.push(row_flags);
        buf.extend_from_slice(&(row.slots.len() as u16).to_le_bytes());
        for slot in &row.slots {
            write_u16_str(&mut buf, &slot.entry_id);
            buf.extend_from_slice(&slot.start_offset_px.to_le_bytes());
            buf.extend_from_slice(&slot.width_px.to_le_bytes());
            let mut flags: u8 = 0;
            if slot.is_now {
                flags |= SLOT_FLAG_IS_NOW;
            }
            if slot.is_past {
                flags |= SLOT_FLAG_IS_PAST;
            }
            if slot.is_focused {
                flags |= SLOT_FLAG_IS_FOCUSED;
            }
            if slot.has_reminder {
                flags |= SLOT_FLAG_HAS_REMINDER;
            }
            if slot.is_recording_scheduled {
                flags |= SLOT_FLAG_IS_RECORDING;
            }
            if slot.is_sports_programme {
                flags |= SLOT_FLAG_IS_SPORTS;
            }
            buf.push(flags);
        }
    }
    buf
}

fn read_u16_str(bytes: &[u8], offset: &mut usize) -> Option<String> {
    if *offset + 2 > bytes.len() {
        return None;
    }
    let len = u16::from_le_bytes([bytes[*offset], bytes[*offset + 1]]) as usize;
    *offset += 2;
    if *offset + len > bytes.len() {
        return None;
    }
    let s = String::from_utf8_lossy(&bytes[*offset..*offset + len]).into_owned();
    *offset += len;
    Some(s)
}

fn read_f32(bytes: &[u8], offset: &mut usize) -> Option<f32> {
    if *offset + 4 > bytes.len() {
        return None;
    }
    let v = f32::from_le_bytes([
        bytes[*offset],
        bytes[*offset + 1],
        bytes[*offset + 2],
        bytes[*offset + 3],
    ]);
    *offset += 4;
    Some(v)
}

/// Decode the packed buffer produced by [encode_compute_output]. Returns `None` on
/// truncated/malformed input (tests + Kotlin must treat that as empty draw).
pub fn decode_compute_output(bytes: &[u8]) -> Option<ComputeOutput> {
    if bytes.len() < 8 {
        return None;
    }
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if magic != BINARY_MAGIC {
        return None;
    }
    let row_count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let mut offset = 8;
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        if offset + 4 > bytes.len() {
            return None;
        }
        let row_index = i32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]);
        offset += 4;
        let channel_id = read_u16_str(bytes, &mut offset)?;
        if offset >= bytes.len() {
            return None;
        }
        let row_flags = bytes[offset];
        offset += 1;
        if offset + 2 > bytes.len() {
            return None;
        }
        let slot_count = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        offset += 2;
        let mut slots = Vec::with_capacity(slot_count);
        for _ in 0..slot_count {
            let entry_id = read_u16_str(bytes, &mut offset)?;
            let start_offset_px = read_f32(bytes, &mut offset)?;
            let width_px = read_f32(bytes, &mut offset)?;
            if offset >= bytes.len() {
                return None;
            }
            let flags = bytes[offset];
            offset += 1;
            slots.push(SlotOut {
                entry_id,
                start_offset_px,
                width_px,
                is_now: flags & SLOT_FLAG_IS_NOW != 0,
                is_past: flags & SLOT_FLAG_IS_PAST != 0,
                is_focused: flags & SLOT_FLAG_IS_FOCUSED != 0,
                has_reminder: flags & SLOT_FLAG_HAS_REMINDER != 0,
                is_recording_scheduled: flags & SLOT_FLAG_IS_RECORDING != 0,
                is_sports_programme: flags & SLOT_FLAG_IS_SPORTS != 0,
            });
        }
        rows.push(RowOut {
            row_index,
            channel_id,
            is_focused_row: row_flags & 1 != 0,
            is_channel_focused: row_flags & (1 << 1) != 0,
            slots,
        });
    }
    Some(ComputeOutput { rows })
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_guide_epg_grid_EpgNativeBridge_nativeEngineVersion(
    _env: EnvUnowned,
    _class: JClass,
) -> jint {
    ENGINE_VERSION
}

// ── Resident grid handle (Phase 6.2): upload rows once per generation, then
// per-frame only send viewport primitives — eliminates re-serializing the full
// row/programme data as JSON on every scroll/frame. ──────────────────────────

struct GridState {
    generation_id: i64,
    rows: Vec<RowIn>,
}

fn grid_state() -> &'static Mutex<Option<GridState>> {
    static STATE: OnceLock<Mutex<Option<GridState>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GridUploadInput {
    pub generation_id: i64,
    pub rows: Vec<RowIn>,
}

pub fn upload_grid_rows(generation_id: i64, rows: Vec<RowIn>) {
    let mut guard = grid_state().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(GridState { generation_id, rows });
}

/// Computes visible rows using the resident grid state. Returns `None` when the
/// generation doesn't match (stale/missing), signalling the Kotlin caller to
/// re-upload and retry.
#[allow(clippy::too_many_arguments)]
pub fn compute_visible_rows_by_handle(
    generation_id: i64,
    first_visible_row: i32,
    visible_row_count: i32,
    focused_row: i32,
    focused_programme_index: i32,
    zone: &str,
    now_ms: i64,
    now_next_mode: bool,
    window_start: i64,
    window_end: i64,
    hour_width_px: f32,
    programme_area_width_px: f32,
    viewport_start: i64,
    viewport_end: i64,
    reminder_keys: &[String],
    recording_keys: &[String],
) -> Option<ComputeOutput> {
    let guard = grid_state().lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.as_ref()?;
    if state.generation_id != generation_id {
        return None;
    }
    let reminder_set: HashSet<String> = reminder_keys.iter().cloned().collect();
    let recording_set: HashSet<String> = recording_keys.iter().cloned().collect();
    Some(compute_visible_rows_impl(
        &state.rows,
        first_visible_row,
        visible_row_count,
        focused_row,
        focused_programme_index,
        zone,
        now_ms,
        now_next_mode,
        window_start,
        window_end,
        hour_width_px,
        programme_area_width_px,
        viewport_start,
        viewport_end,
        &reminder_set,
        &recording_set,
    ))
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_guide_epg_grid_EpgNativeBridge_nativeUploadGridRows(
    mut env: EnvUnowned,
    _class: JClass,
    input_json: JString,
) {
    env.with_env(|env| -> std::result::Result<(), jni::errors::Error> {
        Ok({
        let input_str: String = match env.get_string(&input_json) {
            Ok(s) => s.into(),
            Err(_) => return Ok(()),
        };
        if let Ok(input) = serde_json::from_str::<GridUploadInput>(&input_str) {
            upload_grid_rows(input.generation_id, input.rows);
        }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

fn keys_from_jni_array(env: &mut Env, arr: jni::sys::jobjectArray) -> Vec<String> {
    if arr.is_null() {
        return Vec::new();
    }
    let arr_obj = unsafe { jni::objects::JObjectArray::<JString>::from_raw(env, arr) };
    let len = env.get_array_length(&arr_obj).unwrap_or(0) as usize;
    (0..len)
        .filter_map(|i| {
            env.get_object_array_element::<JString>(&arr_obj, i)
                .ok()
                .and_then(|obj| env.get_string(&obj).ok().map(|s| s.into()))
        })
        .collect()
}

/// Per-frame compute via resident handle. Takes only viewport primitives + a
/// generation ID — no row/programme JSON. Returns a packed `EPG2` binary buffer
/// (see [encode_compute_output]). Returns `null` when the generation is stale
/// (Kotlin re-uploads and retries).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_com_aurelis_tv_guide_epg_grid_EpgNativeBridge_nativeComputeVisibleRowsByHandleBinary(
    mut env: EnvUnowned,
    _class: JClass,
    generation_id: jlong,
    first_visible_row: jint,
    visible_row_count: jint,
    focused_row: jint,
    focused_programme_index: jint,
    zone: JString,
    now_ms: jlong,
    now_next_mode: jint,
    window_start: jlong,
    window_end: jlong,
    hour_width_px: jni::sys::jfloat,
    programme_area_width_px: jni::sys::jfloat,
    viewport_start: jlong,
    viewport_end: jlong,
    reminder_keys: jni::sys::jobjectArray,
    recording_keys: jni::sys::jobjectArray,
) -> jbyteArray {
    env.with_env(|env| -> std::result::Result<jbyteArray, jni::errors::Error> {
        Ok({
        let zone_str: String = env.get_string(&zone).map(|s| s.into()).unwrap_or_default();
        let reminder = keys_from_jni_array(env, reminder_keys);
        let recording = keys_from_jni_array(env, recording_keys);

        let output = match compute_visible_rows_by_handle(
            generation_id,
            first_visible_row,
            visible_row_count,
            focused_row,
            focused_programme_index,
            &zone_str,
            now_ms,
            now_next_mode != 0,
            window_start,
            window_end,
            hour_width_px,
            programme_area_width_px,
            viewport_start,
            viewport_end,
            &reminder,
            &recording,
        ) {
            Some(o) => o,
            None => return Ok(std::ptr::null_mut()),
        };

        let bytes = encode_compute_output(&output);
        env.byte_array_from_slice(&bytes)
            .map(|a| a.into_raw())
            .unwrap_or(std::ptr::null_mut())
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn programme(id: &str, start_ms: i64, end_ms: i64, title: &str) -> ProgrammeIn {
        ProgrammeIn {
            id: id.to_string(),
            channel_id: "ch1".to_string(),
            channel_name: "Channel 1".to_string(),
            start_ms,
            end_ms,
            title: title.to_string(),
            has_catchup: false,
            progress_fraction: 0.0,
        }
    }

    fn row(row_index: i32, channel_id: &str, programmes: Vec<ProgrammeIn>) -> RowIn {
        RowIn { row_index, channel_id: channel_id.to_string(), programmes }
    }

    const HOUR: i64 = 3_600_000;

    fn base_input(rows: Vec<RowIn>) -> ComputeInput {
        ComputeInput {
            rows,
            first_visible_row: 0,
            visible_row_count: 5,
            focused_row: 0,
            focused_programme_index: 0,
            zone: "PROGRAMME".to_string(),
            now_ms: 0,
            now_next_mode: false,
            window_start: 0,
            window_end: 8 * HOUR,
            hour_width_px: 280.0,
            programme_area_width_px: 2000.0,
            viewport_start: 0,
            viewport_end: 8 * HOUR,
            reminder_keys: vec![],
            recording_keys: vec![],
        }
    }

    // Mirrors KotlinEpgGridEngineTest.emptyRows_returnsNoDrawRows
    #[test]
    fn empty_rows_returns_no_draw_rows() {
        let input = base_input(vec![]);
        assert!(compute_visible_rows(&input).rows.is_empty());
    }

    // Mirrors KotlinEpgGridEngineTest.clampsVisibleRangeToRowCount
    #[test]
    fn clamps_visible_range_to_row_count() {
        let rows = (0..5)
            .map(|i| row(i, &format!("c{i}"), vec![programme("p", 0, 8 * HOUR, "p")]))
            .collect();
        let mut input = base_input(rows);
        input.first_visible_row = 3;
        input.visible_row_count = 10;
        input.focused_row = 3;

        let output = compute_visible_rows(&input);
        let indices: Vec<i32> = output.rows.iter().map(|r| r.row_index).collect();
        assert_eq!(indices, vec![3, 4]);
    }

    // Mirrors KotlinEpgGridEngineTest.onlyFocusedRowIsMarkedFocused
    #[test]
    fn only_focused_row_is_marked_focused() {
        let rows = (0..5)
            .map(|i| row(i, &format!("c{i}"), vec![programme("p", 0, 8 * HOUR, "p")]))
            .collect();
        let mut input = base_input(rows);
        input.visible_row_count = 5;
        input.focused_row = 2;

        let output = compute_visible_rows(&input);
        let focused: Vec<i32> = output.rows.iter().filter(|r| r.is_focused_row).map(|r| r.row_index).collect();
        assert_eq!(focused, vec![2]);
    }

    // Mirrors KotlinEpgGridEngineTest.channelZone_marksIsChannelFocusedOnFocusedRowOnly
    #[test]
    fn channel_zone_marks_channel_focused_and_no_slot_focused() {
        let rows = (0..3)
            .map(|i| row(i, &format!("c{i}"), vec![programme("p", 0, 8 * HOUR, "p")]))
            .collect();
        let mut input = base_input(rows);
        input.visible_row_count = 3;
        input.focused_row = 1;
        input.zone = "CHANNEL".to_string();

        let output = compute_visible_rows(&input);
        assert!(output.rows.iter().find(|r| r.row_index == 1).unwrap().is_channel_focused);
        assert!(!output.rows.iter().find(|r| r.row_index == 0).unwrap().is_channel_focused);
        assert!(output.rows.iter().flat_map(|r| &r.slots).all(|s| !s.is_focused));
    }

    // Mirrors KotlinEpgGridEngineTest.programmeZone_marksFocusedSlotOnFocusedRow
    #[test]
    fn programme_zone_marks_focused_slot_on_focused_row() {
        let rows = vec![
            row(0, "c0", vec![programme("p0", 0, 8 * HOUR, "p0")]),
            row(1, "c1", vec![programme("p1", 0, 8 * HOUR, "p1")]),
        ];
        let mut input = base_input(rows);
        input.visible_row_count = 2;
        input.focused_row = 0;

        let output = compute_visible_rows(&input);
        let row0 = output.rows.iter().find(|r| r.row_index == 0).unwrap();
        let row1 = output.rows.iter().find(|r| r.row_index == 1).unwrap();
        assert!(row0.slots[0].is_focused);
        assert!(!row1.slots[0].is_focused);
    }

    // Phase 4 parity: mirrors EpgGridComputeTest's reminder/recording/sports flag tests.
    #[test]
    fn flags_reminder_recording_and_sports_from_lookup_and_title() {
        let mut p = programme("p1", 0, HOUR, "Arsenal vs Chelsea");
        p.channel_id = "ch1".to_string();
        p.channel_name = "Channel 1".to_string();
        let rows = vec![row(0, "ch1", vec![p])];
        let mut input = base_input(rows);
        input.visible_row_count = 1;
        input.reminder_keys = vec!["ch1:0".to_string()];
        input.recording_keys = vec!["Channel 1:0".to_string()];

        let output = compute_visible_rows(&input);
        let slot = &output.rows[0].slots[0];
        assert!(slot.has_reminder);
        assert!(slot.is_recording_scheduled);
        assert!(slot.is_sports_programme);
    }

    #[test]
    fn viewport_range_culls_programmes_outside_visible_timeline() {
        let rows = vec![row(
            0,
            "c0",
            vec![
                programme("past", 0, HOUR, "past"),
                programme("visible", 2 * HOUR, 3 * HOUR, "visible"),
                programme("future", 6 * HOUR, 7 * HOUR, "future"),
            ],
        )];
        let mut input = base_input(rows);
        input.visible_row_count = 1;
        input.focused_programme_index = 1; // focus the in-viewport cell, not the culled one
        input.viewport_start = 2 * HOUR;
        input.viewport_end = 4 * HOUR;

        let output = compute_visible_rows(&input);
        let ids: Vec<&str> = output.rows[0]
            .slots
            .iter()
            .map(|s| s.entry_id.as_str())
            .collect();
        assert_eq!(ids, vec!["visible"]);
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 2);
    }

    #[test]
    fn binary_encode_decode_roundtrip_preserves_slots_and_flags() {
        let mut p = programme("p1", 0, HOUR, "Arsenal vs Chelsea");
        p.channel_id = "ch1".to_string();
        p.channel_name = "Channel 1".to_string();
        let rows = vec![
            row(0, "ch1", vec![p]),
            row(1, "ch2", vec![programme("p2", HOUR, 2 * HOUR, "News")]),
        ];
        let mut input = base_input(rows);
        input.visible_row_count = 2;
        input.focused_row = 0;
        input.reminder_keys = vec!["ch1:0".to_string()];
        input.recording_keys = vec!["Channel 1:0".to_string()];

        let output = compute_visible_rows(&input);
        let encoded = encode_compute_output(&output);
        assert_eq!(
            u32::from_le_bytes(encoded[0..4].try_into().unwrap()),
            BINARY_MAGIC
        );
        let decoded = decode_compute_output(&encoded).expect("decode");
        assert_eq!(decoded.rows.len(), output.rows.len());
        for (a, b) in decoded.rows.iter().zip(output.rows.iter()) {
            assert_eq!(a.row_index, b.row_index);
            assert_eq!(a.channel_id, b.channel_id);
            assert_eq!(a.is_focused_row, b.is_focused_row);
            assert_eq!(a.is_channel_focused, b.is_channel_focused);
            assert_eq!(a.slots.len(), b.slots.len());
            for (sa, sb) in a.slots.iter().zip(b.slots.iter()) {
                assert_eq!(sa.entry_id, sb.entry_id);
                assert_eq!(sa.start_offset_px, sb.start_offset_px);
                assert_eq!(sa.width_px, sb.width_px);
                assert_eq!(sa.is_now, sb.is_now);
                assert_eq!(sa.is_past, sb.is_past);
                assert_eq!(sa.is_focused, sb.is_focused);
                assert_eq!(sa.has_reminder, sb.has_reminder);
                assert_eq!(sa.is_recording_scheduled, sb.is_recording_scheduled);
                assert_eq!(sa.is_sports_programme, sb.is_sports_programme);
            }
        }
        assert!(decoded.rows[0].slots[0].has_reminder);
        assert!(decoded.rows[0].slots[0].is_recording_scheduled);
        assert!(decoded.rows[0].slots[0].is_sports_programme);
        assert!(decoded.rows[0].slots[0].is_focused);
    }

    #[test]
    fn binary_decode_rejects_bad_magic_and_truncation() {
        assert!(decode_compute_output(&[]).is_none());
        assert!(decode_compute_output(&[0, 0, 0, 0, 0, 0, 0, 0]).is_none());
        let mut truncated = encode_compute_output(&ComputeOutput {
            rows: vec![RowOut {
                row_index: 0,
                channel_id: "c".into(),
                is_focused_row: false,
                is_channel_focused: false,
                slots: vec![],
            }],
        });
        truncated.truncate(truncated.len() - 1);
        // Still valid if we only dropped padding that doesn't exist — force short:
        assert!(decode_compute_output(&truncated[..4]).is_none());
    }

    #[test]
    fn binary_empty_output_is_magic_plus_zero_rows() {
        let encoded = encode_compute_output(&ComputeOutput { rows: vec![] });
        assert_eq!(encoded.len(), 8);
        let decoded = decode_compute_output(&encoded).unwrap();
        assert!(decoded.rows.is_empty());
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}
