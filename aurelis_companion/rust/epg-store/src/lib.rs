//! Packed live-guide sidecar. Byte-for-byte twin of
//! `core/common/.../epg/EpgPackedStore.kt`.
//!
//! Disk is untrusted: magic, version, and every offset are checked before use.

use jni::{EnvUnowned};
use jni::objects::{JClass, JIntArray, JString};
use jni::sys::{jbyteArray, jint, jintArray, jlong};
use jni::Env;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// v4: on-disk format v2 — the header now carries `coversUntilMs`, so a sidecar states the
/// window it actually packed instead of leaving callers to infer one from its mtime. Bumped
/// because a v3 `.so` cannot read a v2 file: better a printable "engine unavailable" than a
/// decoder that silently returns nothing for every query.
///
/// v3: process-local decode cache **plus** per-channel-key index so Guide category
/// queries are O(keys) instead of a full-table scan (v2 still scanned every programme).
pub const ENGINE_VERSION: i32 = 4;
pub const MAGIC: u32 = 0x4750_4541;
pub const VERSION: u32 = 2;
/// magic u32, version u32, count u32, blobLen u32, coversUntilMs i64.
pub const HEADER_BYTES: usize = 24;
pub const PROGRAMME_BYTES: usize = 28;
pub const FLAG_CATCHUP: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Programme {
    pub channel_key: u32,
    pub start_ms: i64,
    pub end_ms: i64,
    pub title: String,
    pub has_catchup: bool,
}

/// Only the per-key index is kept: queries never read the flat list, and holding both kept every
/// programme twice in native memory on a 1.7 GB stick (.246, 27 Oct 2026).
struct MemCache {
    path: PathBuf,
    mtime: SystemTime,
    by_key: Arc<HashMap<u32, Vec<Programme>>>,
}

static MEM_CACHE: Mutex<Option<MemCache>> = Mutex::new(None);

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

/// Moves the programmes into the index: no clone of each title.
fn index_by_key(programmes: Vec<Programme>) -> HashMap<u32, Vec<Programme>> {
    let mut map: HashMap<u32, Vec<Programme>> = HashMap::with_capacity(programmes.len() / 4);
    for p in programmes {
        map.entry(p.channel_key).or_default().push(p);
    }
    map
}

/// Process-local decode + index cache keyed by path + mtime.
pub fn cached_read_file(path: &Path) -> Option<Arc<HashMap<u32, Vec<Programme>>>> {
    let mtime = file_mtime(path)?;
    if let Ok(guard) = MEM_CACHE.lock() {
        if let Some(hit) = guard.as_ref() {
            if hit.path == path && hit.mtime == mtime {
                return Some(Arc::clone(&hit.by_key));
            }
        }
    }
    // The previous file's index is released before the new one is built, not after.
    invalidate_cache();
    let by_key = Arc::new(index_by_key(read_file(path)?));
    if let Ok(mut guard) = MEM_CACHE.lock() {
        *guard = Some(MemCache {
            path: path.to_path_buf(),
            mtime,
            by_key: Arc::clone(&by_key),
        });
    }
    Some(by_key)
}

pub fn invalidate_cache() {
    if let Ok(mut guard) = MEM_CACHE.lock() {
        *guard = None;
    }
}

pub fn fnv1a32(s: &str) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for b in s.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(16_777_619);
    }
    h
}

pub fn encode(programmes: &[Programme]) -> Vec<u8> {
    let mut sorted = programmes.to_vec();
    sorted.sort_by(|a, b| a.channel_key.cmp(&b.channel_key).then(a.start_ms.cmp(&b.start_ms)));
    let title_bytes: Vec<Vec<u8>> = sorted
        .iter()
        .map(|p| {
            let mut t = p.title.as_bytes().to_vec();
            if t.len() > 65535 {
                t.truncate(65535);
            }
            t
        })
        .collect();
    let blob_len: usize = title_bytes.iter().map(|t| t.len()).sum();
    let mut out = Vec::with_capacity(HEADER_BYTES + sorted.len() * PROGRAMME_BYTES + blob_len);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    out.extend_from_slice(&(blob_len as u32).to_le_bytes());
    // Greatest packed endMs, 0 when empty. Twin of EpgPackedStore.encode.
    let covers_until: i64 = sorted.iter().map(|p| p.end_ms).max().unwrap_or(0);
    out.extend_from_slice(&covers_until.to_le_bytes());
    let mut off: u32 = 0;
    for (i, p) in sorted.iter().enumerate() {
        let tlen = title_bytes[i].len() as u16;
        out.extend_from_slice(&p.channel_key.to_le_bytes());
        out.extend_from_slice(&p.start_ms.to_le_bytes());
        out.extend_from_slice(&p.end_ms.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&tlen.to_le_bytes());
        let flags: u16 = if p.has_catchup { FLAG_CATCHUP } else { 0 };
        out.extend_from_slice(&flags.to_le_bytes());
        off += u32::from(tlen);
    }
    for t in title_bytes {
        out.extend_from_slice(&t);
    }
    out
}

/// Twin of `EpgPackedStore.readHeader`: `(count, coversUntilMs)` without decoding the table.
/// None when the bytes are short, not AEPG, or not format v[VERSION] — each of which means
/// "cannot answer", never "covers nothing".
pub fn decode_header(bytes: &[u8]) -> Option<(u32, i64)> {
    if bytes.len() < HEADER_BYTES {
        return None;
    }
    if u32::from_le_bytes(bytes[0..4].try_into().ok()?) != MAGIC {
        return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
        return None;
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let covers_until = i64::from_le_bytes(bytes[16..24].try_into().ok()?);
    Some((count, covers_until))
}

pub fn decode(bytes: &[u8]) -> Option<Vec<Programme>> {
    if bytes.len() < HEADER_BYTES {
        return None;
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    if magic != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
    let blob_len = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
    // bytes[16..24] is coversUntilMs — see decode_header; the decoded list carries the same
    // facts, so the query path does not need it.
    if count > 2_000_000 {
        return None;
    }
    let table = count.checked_mul(PROGRAMME_BYTES)?;
    if HEADER_BYTES.checked_add(table)?.checked_add(blob_len)? != bytes.len() {
        return None;
    }
    let blob_start = HEADER_BYTES + table;
    let blob = &bytes[blob_start..blob_start + blob_len];
    let mut out = Vec::with_capacity(count);
    let mut cur = HEADER_BYTES;
    for _ in 0..count {
        let key = u32::from_le_bytes(bytes[cur..cur + 4].try_into().ok()?);
        let start = i64::from_le_bytes(bytes[cur + 4..cur + 12].try_into().ok()?);
        let end = i64::from_le_bytes(bytes[cur + 12..cur + 20].try_into().ok()?);
        let off = u32::from_le_bytes(bytes[cur + 20..cur + 24].try_into().ok()?) as usize;
        let len = u16::from_le_bytes(bytes[cur + 24..cur + 26].try_into().ok()?) as usize;
        let flags = u16::from_le_bytes(bytes[cur + 26..cur + 28].try_into().ok()?);
        cur += PROGRAMME_BYTES;
        if off.checked_add(len)? > blob.len() {
            return None;
        }
        let title = std::str::from_utf8(&blob[off..off + len]).ok()?.to_string();
        out.push(Programme {
            channel_key: key,
            start_ms: start,
            end_ms: end,
            title,
            has_catchup: flags & FLAG_CATCHUP != 0,
        });
    }
    Some(out)
}

/// Full-table scan — kept for parity tests; Guide JNI uses [query_indexed].
pub fn query(programmes: &[Programme], keys: &[u32], start_ms: i64, end_ms: i64) -> Vec<Programme> {
    let set: std::collections::HashSet<u32> = keys.iter().copied().collect();
    programmes
        .iter()
        .filter(|p| set.contains(&p.channel_key) && p.end_ms >= start_ms && p.start_ms <= end_ms)
        .cloned()
        .collect()
}

/// Twin of Kotlin [EpgPackedStore.queryIndexed] — O(requested keys), not O(all programmes).
pub fn query_indexed(
    by_key: &HashMap<u32, Vec<Programme>>,
    keys: &[u32],
    start_ms: i64,
    end_ms: i64,
) -> Vec<Programme> {
    if by_key.is_empty() || keys.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(keys.len() * 4);
    for &key in keys {
        if let Some(list) = by_key.get(&key) {
            for p in list {
                if p.end_ms >= start_ms && p.start_ms <= end_ms {
                    out.push(p.clone());
                }
            }
        }
    }
    out
}

pub fn write_file(path: &Path, programmes: &[Programme]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("aepg.tmp");
    fs::write(&tmp, encode(programmes))?;
    fs::rename(&tmp, path)?;
    invalidate_cache();
    Ok(())
}

pub fn read_file(path: &Path) -> Option<Vec<Programme>> {
    let bytes = fs::read(path).ok()?;
    decode(&bytes)
}

pub fn _force_link() {}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_epg_EpgStoreNativeBridge_nativeEngineVersion(
    _env: EnvUnowned,
    _class: JClass,
) -> jint {
    ENGINE_VERSION
}

/// Query a packed file. Returns encoded AEPG of the matching slice, or null on failure.
#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_common_epg_EpgStoreNativeBridge_nativeQueryFile(
    mut env: EnvUnowned,
    _class: JClass,
    path: JString,
    keys: jintArray,
    start_ms: jlong,
    end_ms: jlong,
) -> jbyteArray {
    env.with_env(|env| -> std::result::Result<jbyteArray, jni::errors::Error> {
        Ok({
        let path = match env.get_string(&path) {
            Ok(s) => String::from(s),
            Err(_) => return Ok(std::ptr::null_mut()),
        };
        let by_key = match cached_read_file(Path::new(&path)) {
            Some(p) => p,
            None => return Ok(std::ptr::null_mut()),
        };
        let keys_arr = unsafe { JIntArray::from_raw(env, keys) };
        let len = match env.get_array_length(&keys_arr) {
            Ok(n) if n >= 0 => n as usize,
            _ => return Ok(std::ptr::null_mut()),
        };
        let mut key_buf = vec![0i32; len];
        if len > 0 && env.get_int_array_region(&keys_arr, 0, &mut key_buf).is_err() {
            return Ok(std::ptr::null_mut());
        }
        let key_u32: Vec<u32> = key_buf.into_iter().map(|k| k as u32).collect();
        let hits = query_indexed(&by_key, &key_u32, start_ms, end_ms);
        let encoded = encode(&hits);
        env.byte_array_from_slice(&encoded)
            .map(|a| a.into_raw())
            .unwrap_or(std::ptr::null_mut())
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn decode_reads_titles_shared_between_programmes() {
        // The Kotlin writer stores each distinct title once; several programmes then carry the
        // same (off, len). Layout: header, three 28-byte records, blob "NewsSoap".
        let blob = b"NewsSoap";
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        out.extend_from_slice(&9_000i64.to_le_bytes());
        for (i, (off, len)) in [(0u32, 4u16), (0, 4), (4, 4)].iter().enumerate() {
            out.extend_from_slice(&7u32.to_le_bytes());
            out.extend_from_slice(&(i as i64 * 1_000).to_le_bytes());
            out.extend_from_slice(&(i as i64 * 1_000 + 500).to_le_bytes());
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
        }
        out.extend_from_slice(blob);
        let decoded = decode(&out).expect("shared-title file decodes");
        let titles: Vec<&str> = decoded.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, vec!["News", "News", "Soap"]);
    }

    #[test]
    fn round_trip_and_query() {
        let progs = vec![
            Programme {
                channel_key: fnv1a32("bbc1"),
                start_ms: 100,
                end_ms: 200,
                title: "News".into(),
                has_catchup: true,
            },
            Programme {
                channel_key: fnv1a32("bbc2"),
                start_ms: 150,
                end_ms: 250,
                title: "Drama".into(),
                has_catchup: false,
            },
        ];
        let bytes = encode(&progs);
        let back = decode(&bytes).expect("decode");
        assert_eq!(back.len(), 2);
        let hits = query(&back, &[fnv1a32("bbc1")], 90, 210);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "News");
        assert!(hits[0].has_catchup);
    }

    #[test]
    fn query_indexed_only_scans_requested_keys() {
        let mut progs = Vec::new();
        for i in 1..500u32 {
            progs.push(Programme {
                channel_key: i,
                start_ms: i as i64 * 10,
                end_ms: i as i64 * 10 + 5,
                title: format!("T{i}"),
                has_catchup: false,
            });
        }
        let target = fnv1a32("target");
        progs.push(Programme {
            channel_key: target,
            start_ms: 1_000,
            end_ms: 1_100,
            title: "Hit".into(),
            has_catchup: false,
        });
        let by_key = index_by_key(progs);
        let hits = query_indexed(&by_key, &[target], 900, 1_200);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Hit");
    }

    #[test]
    fn rejects_truncated() {
        assert!(decode(&[1, 2, 3]).is_none());
        let mut bytes = encode(&[Programme {
            channel_key: 1,
            start_ms: 0,
            end_ms: 1,
            title: "X".into(),
            has_catchup: false,
        }]);
        bytes.truncate(20);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut bytes = encode(&[Programme {
            channel_key: 1,
            start_ms: 0,
            end_ms: 1,
            title: "X".into(),
            has_catchup: false,
        }]);
        bytes[0] = b'Z';
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn rejects_wrong_endian_magic() {
        let mut bytes = encode(&[Programme {
            channel_key: 1,
            start_ms: 0,
            end_ms: 1,
            title: "X".into(),
            has_catchup: false,
        }]);
        bytes[0..4].copy_from_slice(&MAGIC.to_be_bytes());
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn cached_read_reuses_decode_for_same_mtime() {
        invalidate_cache();
        let dir = std::env::temp_dir().join(format!(
            "aurelis_epg_cache_{}",
            UNIX_EPOCH.elapsed().unwrap_or(Duration::from_secs(1)).as_nanos()
        ));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("guide.aepg");
        let progs = vec![Programme {
            channel_key: fnv1a32("bbc1"),
            start_ms: 100,
            end_ms: 200,
            title: "News".into(),
            has_catchup: false,
        }];
        write_file(&path, &progs).expect("write");
        let first_idx = cached_read_file(&path).expect("first");
        let second_idx = cached_read_file(&path).expect("second");
        assert!(Arc::ptr_eq(&first_idx, &second_idx));
        let hits = query_indexed(&second_idx, &[fnv1a32("bbc1")], 90, 210);
        assert_eq!(hits.len(), 1);
        let _ = fs::remove_dir_all(&dir);
        invalidate_cache();
    }

    fn prog(id: &str, start_ms: i64, end_ms: i64, title: &str) -> Programme {
        Programme {
            channel_key: fnv1a32(id),
            start_ms,
            end_ms,
            title: title.into(),
            has_catchup: false,
        }
    }

    #[test]
    fn engine_version_is_v4() {
        assert_eq!(ENGINE_VERSION, 4);
    }

    #[test]
    fn header_states_the_greatest_packed_end() {
        let bytes = encode(&[
            prog("bbc1", 100, 200, "News"),
            prog("itv", 150, 900, "Film"),
            prog("c4", 120, 300, "Doc"),
        ]);
        assert_eq!(decode_header(&bytes), Some((3, 900)));
    }

    /// An empty pack must declare zero coverage, not "as new as its mtime". This is the
    /// whole point of the field: a 24-byte file is structurally valid and would otherwise
    /// read as a perfectly fresh sidecar covering the full window.
    #[test]
    fn empty_pack_covers_nothing() {
        let bytes = encode(&[]);
        assert_eq!(bytes.len(), HEADER_BYTES);
        assert_eq!(decode_header(&bytes), Some((0, 0)));
        assert_eq!(decode(&bytes), Some(vec![]));
    }

    #[test]
    fn decode_header_rejects_short_bad_magic_and_old_version() {
        assert_eq!(decode_header(&[1, 2, 3]), None);
        let ok = encode(&[prog("bbc1", 0, 10, "X")]);
        assert_eq!(decode_header(&ok[..HEADER_BYTES - 1]), None);
        let mut bad_magic = ok.clone();
        bad_magic[0] = b'Z';
        assert_eq!(decode_header(&bad_magic), None);
        let mut v1 = ok.clone();
        v1[4] = 1;
        assert_eq!(decode_header(&v1), None, "a v1 file must not read as v2");
        assert_eq!(decode(&v1), None);
    }
}
