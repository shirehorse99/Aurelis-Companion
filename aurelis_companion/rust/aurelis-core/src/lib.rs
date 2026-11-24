//! Single cdylib that links all Aurelis Rust crates' JNI symbols into one `.so`.
//!
//! Each dependency crate produces an `rlib` (in addition to its standalone `cdylib`)
//! so its `#[no_mangle]` JNI exports are available to the linker here. The `extern crate`
//! declarations plus the `force_link_*` functions ensure the linker does not strip
//! any crate's object files — without an explicit reference, LTO can discard rlib
//! symbols that are only reached via JNI's name-based dispatch.

use jni::sys::{jint, JNI_VERSION_1_6};
use jni::JavaVM;
use std::ffi::c_void;

/// Called by the JVM when `System.loadLibrary("aurelis_core")` runs. Populates
/// the shared JNI handle cache (see `rust/jni-cache`) once, so hot paths avoid
/// per-call `find_class`. Returns the JNI version this library requires.
///
/// # Safety
/// Invoked by the JVM on the thread that loaded the library, which is attached,
/// so `vm.get_env()` inside `jni_cache::on_load` is valid.
#[no_mangle]
pub extern "system" fn JNI_OnLoad(vm: JavaVM, _reserved: *mut c_void) -> jint {
    jni_cache::on_load(&vm);
    JNI_VERSION_1_6
}

// Force the linker to include every crate's #[no_mangle] JNI symbols.
// Each function references a public item from the crate, which prevents LTO
// from discarding the crate's object files (and their #[no_mangle] exports).

extern crate aurelis_heuristics;
extern crate bitrate_calc;
extern crate channel_audit_heuristics;
extern crate channel_merge;
extern crate channel_quality_score;
extern crate companion_protocol;
extern crate epg_artwork_heuristics;
extern crate epg_channel_sort;
extern crate epg_grid;
extern crate epg_matcher;
extern crate epg_store;
extern crate m3u_parser;
extern crate media_server_parser;
extern crate metoffice_parser;
extern crate news_cluster;
extern crate news_parser;
extern crate odds_matcher;
extern crate regional_channel_filter;
extern crate smart_collection;
extern crate stream_revert;
extern crate thematic_rail_matcher;
extern crate xmltv_parser;
extern crate xtream_parser;
// Phase 4 crates
extern crate news_matcher;
extern crate sports_league_art;
extern crate sports_on_tv_linker;
extern crate subtitle_offset;
extern crate title_matcher;
extern crate tv_pick_quality;
extern crate xmltv_auto_mapper;

/// Forces the linker to include every crate's root object file (which contains
/// their `#[no_mangle]` JNI exports). Without an explicit reference, LTO strips
/// rlib symbols that are only reached via JNI's name-based dispatch.
#[used]
static FORCE_LINK: &[fn()] = &[
    aurelis_heuristics::_force_link,
    bitrate_calc::_force_link,
    channel_audit_heuristics::_force_link,
    channel_merge::_force_link,
    channel_quality_score::_force_link,
    companion_protocol::_force_link,
    epg_artwork_heuristics::_force_link,
    epg_channel_sort::_force_link,
    epg_grid::_force_link,
    epg_matcher::_force_link,
    epg_store::_force_link,
    m3u_parser::_force_link,
    media_server_parser::_force_link,
    metoffice_parser::_force_link,
    news_cluster::_force_link,
    news_parser::_force_link,
    odds_matcher::_force_link,
    regional_channel_filter::_force_link,
    smart_collection::_force_link,
    stream_revert::_force_link,
    thematic_rail_matcher::_force_link,
    xmltv_parser::_force_link,
    xtream_parser::_force_link,
    // Phase 4 crates
    news_matcher::_force_link,
    sports_league_art::_force_link,
    sports_on_tv_linker::_force_link,
    subtitle_offset::_force_link,
    title_matcher::_force_link,
    tv_pick_quality::_force_link,
    xmltv_auto_mapper::_force_link,
];
