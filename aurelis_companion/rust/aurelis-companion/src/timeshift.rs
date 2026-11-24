//! Rolling Live Timeshift Buffer: manages circular segmented ring buffers on disk
//! for live stream rewind and extended pause/resume without local TV flash wear.

use crate::state::WallClock;
use crate::stream_relay::ActiveRelay;
use axum::body::Bytes;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc, Notify};

pub const SEGMENT_TARGET_DURATION_MS: i64 = 4_000; // 4 seconds per timeshift segment

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeshiftSegment {
    pub seq: u64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeshiftInfo {
    pub available: bool,
    pub key: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub duration_seconds: u64,
    pub segment_count: usize,
    pub total_bytes: u64,
    pub retention_minutes: u64,
}

struct ActiveSegment {
    seq: u64,
    start_ms: i64,
    end_ms: i64,
    path: PathBuf,
    bytes: u64,
    file: File,
}

struct TimeshiftInner {
    next_seq: u64,
    segments: VecDeque<TimeshiftSegment>,
    active: Option<ActiveSegment>,
    total_bytes: u64,
}

pub struct TimeshiftBuffer {
    pub key: String,
    pub dir: PathBuf,
    pub retention_ms: u64,
    wall: WallClock,
    inner: Arc<RwLock<TimeshiftInner>>,
    new_segment_notify: Arc<Notify>,
    closed: Arc<AtomicBool>,
}

/// Removes stale timeshift directories from previous server runs.
pub fn cleanup_stale_timeshift(data_dir: &Path) {
    let ts_dir = data_dir.join("timeshift");
    if ts_dir.exists() {
        let _ = fs::remove_dir_all(&ts_dir);
    }
}

impl TimeshiftBuffer {
    pub fn new(key: &str, data_dir: &Path, retention_minutes: u64, wall: WallClock) -> std::io::Result<Arc<Self>> {
        let dir = data_dir.join("timeshift").join(key);
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        fs::create_dir_all(&dir)?;

        let retention_ms = retention_minutes.max(1) * 60 * 1000;
        Ok(Arc::new(Self {
            key: key.to_string(),
            dir,
            retention_ms,
            wall,
            inner: Arc::new(RwLock::new(TimeshiftInner {
                next_seq: 1,
                segments: VecDeque::new(),
                active: None,
                total_bytes: 0,
            })),
            new_segment_notify: Arc::new(Notify::new()),
            closed: Arc::new(AtomicBool::new(false)),
        }))
    }

    pub fn write_chunk(&self, chunk: &[u8]) -> std::io::Result<()> {
        if self.closed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let now = (self.wall)();
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());

        let need_rotate = match &inner.active {
            None => true,
            Some(act) => (now - act.start_ms) >= SEGMENT_TARGET_DURATION_MS,
        };

        if need_rotate {
            if let Some(mut old_act) = inner.active.take() {
                let _ = old_act.file.flush();
                let seg = TimeshiftSegment {
                    seq: old_act.seq,
                    start_ms: old_act.start_ms,
                    end_ms: old_act.end_ms,
                    path: old_act.path,
                    bytes: old_act.bytes,
                };
                inner.segments.push_back(seg);
                self.new_segment_notify.notify_waiters();
            }

            let seq = inner.next_seq;
            inner.next_seq += 1;
            let file_name = format!("seg_{:08}_{}.ts", seq, now);
            let path = self.dir.join(&file_name);
            let file = OpenOptions::new().create(true).write(true).append(true).open(&path)?;
            inner.active = Some(ActiveSegment {
                seq,
                start_ms: now,
                end_ms: now,
                path,
                bytes: 0,
                file,
            });
        }

        if let Some(act) = &mut inner.active {
            act.file.write_all(chunk)?;
            act.bytes += chunk.len() as u64;
            act.end_ms = now;
            inner.total_bytes += chunk.len() as u64;
        }

        // Prune segments that have aged past the retention window
        let cutoff_ms = now.saturating_sub(self.retention_ms as i64);
        while let Some(front) = inner.segments.front() {
            if front.end_ms < cutoff_ms && inner.segments.len() > 1 {
                let old = inner.segments.pop_front().unwrap();
                inner.total_bytes = inner.total_bytes.saturating_sub(old.bytes);
                let _ = fs::remove_file(&old.path);
            } else {
                break;
            }
        }

        Ok(())
    }

    pub fn info(&self) -> TimeshiftInfo {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let count = inner.segments.len() + if inner.active.is_some() { 1 } else { 0 };
        let start_ms = inner.segments.front().map(|s| s.start_ms)
            .or_else(|| inner.active.as_ref().map(|a| a.start_ms))
            .unwrap_or(0);
        let end_ms = inner.active.as_ref().map(|a| a.end_ms)
            .or_else(|| inner.segments.back().map(|s| s.end_ms))
            .unwrap_or(0);
        let duration_seconds = (end_ms.saturating_sub(start_ms) / 1000).max(0) as u64;
        TimeshiftInfo {
            available: count > 0,
            key: self.key.clone(),
            start_ms,
            end_ms,
            duration_seconds,
            segment_count: count,
            total_bytes: inner.total_bytes,
            retention_minutes: self.retention_ms / (60 * 1000),
        }
    }

    pub fn buffer_seconds(&self) -> u64 {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let start_ms = inner.segments.front().map(|s| s.start_ms)
            .or_else(|| inner.active.as_ref().map(|a| a.start_ms))
            .unwrap_or(0);
        let end_ms = inner.active.as_ref().map(|a| a.end_ms)
            .or_else(|| inner.segments.back().map(|s| s.end_ms))
            .unwrap_or(0);
        (end_ms.saturating_sub(start_ms) / 1000).max(0) as u64
    }

    pub fn segments_from(&self, from_ms: i64) -> (Vec<TimeshiftSegment>, Option<u64>) {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if inner.segments.is_empty() {
            return (Vec::new(), inner.active.as_ref().map(|a| a.seq));
        }

        let start_idx = inner.segments.iter().position(|s| s.end_ms >= from_ms).unwrap_or(0);
        let segs: Vec<TimeshiftSegment> = inner.segments.iter().skip(start_idx).cloned().collect();
        let active_seq = inner.active.as_ref().map(|a| a.seq);
        (segs, active_seq)
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Drop for TimeshiftBuffer {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Spawns an async reader task that streams historical timeshift segments starting from `from_ms`,
/// and once caught up to the live edge, seamlessly hands off to the live broadcast channel.
pub fn spawn_timeshift_reader(
    timeshift: Arc<TimeshiftBuffer>,
    relay: Arc<ActiveRelay>,
    from_ms: i64,
    tx: mpsc::Sender<Result<Bytes, std::convert::Infallible>>,
) {
    tokio::spawn(async move {
        let (segments, _active_seq) = timeshift.segments_from(from_ms);

        let mut buf = vec![0u8; 64 * 1024];
        for seg in segments {
            if let Ok(mut file) = File::open(&seg.path) {
                loop {
                    match file.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = Bytes::copy_from_slice(&buf[..n]);
                            if tx.send(Ok(chunk)).await.is_err() {
                                return;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            }
        }

        // Seamless transition to live edge broadcast
        let mut rx_broadcast = relay.broadcast_tx.subscribe();
        loop {
            match rx_broadcast.recv().await {
                Ok(chunk) => {
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeshift_buffer_rotates_and_prunes() {
        let t = crate::household::TempDir::new();
        let wall_ms = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
        let wall: WallClock = {
            let w = wall_ms.clone();
            Arc::new(move || w.load(Ordering::Relaxed))
        };

        let ts = TimeshiftBuffer::new("test_key", &t.0, 1, wall.clone()).unwrap();

        // Write chunk at t = 1,000,000
        ts.write_chunk(&[1, 2, 3, 4]).unwrap();

        // Move time by 5,000 ms (> SEGMENT_TARGET_DURATION_MS)
        wall_ms.store(1_005_000, Ordering::Relaxed);
        ts.write_chunk(&[5, 6, 7, 8]).unwrap();

        let info = ts.info();
        assert_eq!(info.segment_count, 2);
        assert_eq!(info.total_bytes, 8);
        assert!(info.available);

        let (segs, _) = ts.segments_from(1_000_000);
        assert_eq!(segs.len(), 1); // 1 completed segment
        assert_eq!(segs[0].bytes, 4);

        // Move time past retention (1 min = 60,000 ms)
        wall_ms.store(1_070_000, Ordering::Relaxed);
        ts.write_chunk(&[9, 10]).unwrap();

        // Old segment should be pruned
        let info_after = ts.info();
        assert_eq!(info_after.segment_count, 2); // 1 completed + 1 active
        assert_eq!(info_after.total_bytes, 6); // 4 bytes was pruned
    }
}
