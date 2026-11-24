//! Stream probe: Lightweight stream analysis to detect container, video/audio codecs,
//! and determine whether transcoding or downsampling is required for smooth playback.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StreamProbeResult {
    pub url: String,
    pub container: String,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub is_hevc: bool,
    pub is_interlaced_mpeg2: bool,
    pub audio_is_aac: bool,
    pub needs_transcode: bool,
    pub recommended_profile: String,
}

impl StreamProbeResult {
    pub fn new(url: String, container: String, video_codec: Option<String>, audio_codec: Option<String>) -> Self {
        let is_hevc = video_codec.as_deref().map(|c| c.eq_ignore_ascii_case("hevc") || c.eq_ignore_ascii_case("h265")).unwrap_or(false);
        let is_interlaced_mpeg2 = video_codec.as_deref().map(|c| c.eq_ignore_ascii_case("mpeg2")).unwrap_or(false);
        let audio_is_aac = audio_codec.as_deref().map(|c| c.eq_ignore_ascii_case("aac")).unwrap_or(false);

        let (needs_transcode, recommended_profile) = if is_hevc {
            (true, "firetv_stick".to_string())
        } else if is_interlaced_mpeg2 {
            (true, "compatibility".to_string())
        } else if !audio_is_aac && audio_codec.is_some() {
            // Video is fine (e.g. H.264), but audio (AC3/EAC3/DTS) might struggle or be silent on some TVs
            (true, "audio_only_aac".to_string())
        } else {
            (false, "remux".to_string())
        };

        Self {
            url,
            container,
            video_codec,
            audio_codec,
            is_hevc,
            is_interlaced_mpeg2,
            audio_is_aac,
            needs_transcode,
            recommended_profile,
        }
    }
}

/// Parses elementary video and audio codecs from an MPEG-TS buffer.
/// Returns (video_codec, audio_codec) if successfully detected.
pub fn probe_ts_buffer(bytes: &[u8]) -> Option<(Option<String>, Option<String>)> {
    if bytes.len() < 376 {
        return None;
    }

    // Find sync byte 0x47 aligned at 188-byte stride
    let mut offset = 0;
    let mut found_sync = false;
    while offset + 376 <= bytes.len() {
        if bytes[offset] == 0x47 && bytes[offset + 188] == 0x47 && bytes[offset + 376] == 0x47 {
            found_sync = true;
            break;
        }
        offset += 1;
    }

    if !found_sync {
        return None;
    }

    let mut pmt_pid: Option<u16> = None;
    let mut video_codec: Option<String> = None;
    let mut audio_codec: Option<String> = None;

    // Scan TS packets
    let mut pos = offset;
    while pos + 188 <= bytes.len() {
        let packet = &bytes[pos..pos + 188];
        pos += 188;

        if packet[0] != 0x47 {
            continue;
        }

        let pusi = (packet[1] & 0x40) != 0;
        let pid = (((packet[1] & 0x1F) as u16) << 8) | (packet[2] as u16);
        let afc = (packet[3] >> 4) & 0x03;

        // Skip adaptation field if present
        let mut payload_start = 4;
        if afc == 0x02 || afc == 0x00 {
            // No payload
            continue;
        } else if afc == 0x03 {
            let af_len = packet[4] as usize;
            payload_start = 5 + af_len;
        }

        if payload_start >= 188 {
            continue;
        }

        let mut payload = &packet[payload_start..];
        if pusi && !payload.is_empty() {
            let pointer_field = payload[0] as usize;
            if 1 + pointer_field < payload.len() {
                payload = &payload[1 + pointer_field..];
            }
        }

        // Check for PAT (PID 0x0000)
        if pid == 0x0000 && pmt_pid.is_none() {
            if payload.len() >= 8 && payload[0] == 0x00 {
                // Table ID 0x00 = PAT
                let section_length = ((((payload[1] & 0x0F) as usize) << 8) | (payload[2] as usize)).min(payload.len() - 3);
                if section_length >= 9 {
                    let mut pat_pos = 8; // skip transport_stream_id, version, section numbers
                    while pat_pos + 4 <= 3 + section_length - 4 { // exclude 4-byte CRC
                        let prog_num = ((payload[pat_pos] as u16) << 8) | (payload[pat_pos + 1] as u16);
                        let prog_pid = (((payload[pat_pos + 2] & 0x1F) as u16) << 8) | (payload[pat_pos + 3] as u16);
                        if prog_num != 0 {
                            pmt_pid = Some(prog_pid);
                            break;
                        }
                        pat_pos += 4;
                    }
                }
            }
        } else if Some(pid) == pmt_pid {
            // Table ID 0x02 = PMT
            if payload.len() >= 12 && payload[0] == 0x02 {
                let section_length = ((((payload[1] & 0x0F) as usize) << 8) | (payload[2] as usize)).min(payload.len() - 3);
                if section_length >= 13 {
                    let program_info_len = (((payload[10] & 0x0F) as usize) << 8) | (payload[11] as usize);
                    let mut es_pos = 12 + program_info_len;
                    let end_es = (3 + section_length).saturating_sub(4); // exclude CRC

                    while es_pos + 5 <= end_es && es_pos + 5 <= payload.len() {
                        let stream_type = payload[es_pos];
                        let es_info_len = (((payload[es_pos + 3] & 0x0F) as usize) << 8) | (payload[es_pos + 4] as usize);

                        match stream_type {
                            0x01 | 0x02 => {
                                if video_codec.is_none() {
                                    video_codec = Some("mpeg2".to_string());
                                }
                            }
                            0x1B => {
                                if video_codec.is_none() {
                                    video_codec = Some("h264".to_string());
                                }
                            }
                            0x24 => {
                                if video_codec.is_none() {
                                    video_codec = Some("hevc".to_string());
                                }
                            }
                            0x03 | 0x04 => {
                                if audio_codec.is_none() {
                                    audio_codec = Some("mp3".to_string());
                                }
                            }
                            0x0F | 0x11 => {
                                if audio_codec.is_none() {
                                    audio_codec = Some("aac".to_string());
                                }
                            }
                            0x81 => {
                                if audio_codec.is_none() {
                                    audio_codec = Some("ac3".to_string());
                                }
                            }
                            0x87 => {
                                if audio_codec.is_none() {
                                    audio_codec = Some("eac3".to_string());
                                }
                            }
                            0x82 | 0x85 | 0x86 => {
                                if audio_codec.is_none() {
                                    audio_codec = Some("dts".to_string());
                                }
                            }
                            0x06 => {
                                // PES private data - scan descriptors for AC-3 or DTS registration tags
                                let desc_start = es_pos + 5;
                                let desc_end = (desc_start + es_info_len).min(payload.len());
                                let mut d_pos = desc_start;
                                while d_pos + 2 <= desc_end {
                                    let tag = payload[d_pos];
                                    let len = payload[d_pos + 1] as usize;
                                    if tag == 0x6A && audio_codec.is_none() {
                                        audio_codec = Some("ac3".to_string());
                                    } else if tag == 0x7A && audio_codec.is_none() {
                                        audio_codec = Some("eac3".to_string());
                                    } else if tag == 0x7B && audio_codec.is_none() {
                                        audio_codec = Some("dts".to_string());
                                    }
                                    d_pos += 2 + len;
                                }
                            }
                            _ => {}
                        }

                        es_pos += 5 + es_info_len;
                    }
                }
            }
        }

        if video_codec.is_some() && audio_codec.is_some() {
            break;
        }
    }

    if video_codec.is_some() || audio_codec.is_some() {
        Some((video_codec, audio_codec))
    } else {
        None
    }
}

/// Parses codecs from an HLS master playlist text.
pub fn probe_hls_playlist(text: &str) -> Option<(Option<String>, Option<String>)> {
    if !text.contains("#EXTM3U") {
        return None;
    }

    let mut video_codec = None;
    let mut audio_codec = None;

    for line in text.lines() {
        if line.starts_with("#EXT-X-STREAM-INF:") {
            if let Some(pos) = line.find("CODECS=\"") {
                let rest = &line[pos + 8..];
                if let Some(end_pos) = rest.find('"') {
                    let codecs_str = &rest[..end_pos];
                    for c in codecs_str.split(',') {
                        let c = c.trim();
                        if c.starts_with("hvc1") || c.starts_with("hev1") {
                            video_codec = Some("hevc".to_string());
                        } else if c.starts_with("avc1") {
                            video_codec = Some("h264".to_string());
                        } else if c.starts_with("mp4a") {
                            audio_codec = Some("aac".to_string());
                        } else if c.starts_with("ac-3") {
                            audio_codec = Some("ac3".to_string());
                        } else if c.starts_with("ec-3") {
                            audio_codec = Some("eac3".to_string());
                        }
                    }
                }
            }
        }
    }

    if video_codec.is_some() || audio_codec.is_some() {
        Some((video_codec, audio_codec))
    } else {
        None
    }
}

/// Probes a remote stream URL by requesting the initial chunk and inspecting headers/packets.
pub async fn probe_stream_url(client: &reqwest::Client, url: &str) -> Result<StreamProbeResult, String> {
    let req = client.get(url)
        .header("User-Agent", "VLC/3.0.18 LibVLC/3.0.18")
        .header("Range", "bytes=0-131071")
        .header("Accept", "*/*");

    let resp = req.send().await.map_err(|e| format!("HTTP probe request failed: {e}"))?;
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format!("upstream HTTP status {}", resp.status()));
    }

    let bytes = resp.bytes().await.map_err(|e| format!("reading stream probe body failed: {e}"))?;

    // Check if body is HLS text
    if let Ok(text) = std::str::from_utf8(&bytes) {
        if let Some((v, a)) = probe_hls_playlist(text) {
            return Ok(StreamProbeResult::new(url.to_string(), "hls".to_string(), v, a));
        }
    }

    // Check if body is MPEG-TS
    if let Some((v, a)) = probe_ts_buffer(&bytes) {
        return Ok(StreamProbeResult::new(url.to_string(), "mpegts".to_string(), v, a));
    }

    // Fallback: undetected container
    Ok(StreamProbeResult::new(url.to_string(), "unknown".to_string(), None, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hls_playlist_probe_detects_hevc_and_aac() {
        let hls = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080,CODECS=\"hvc1.1.6.L93.B0,mp4a.40.2\"\nchunklist.m3u8";
        let res = probe_hls_playlist(hls);
        assert_eq!(res, Some((Some("hevc".to_string()), Some("aac".to_string()))));

        let probe = StreamProbeResult::new("http://example.com/live.m3u8".to_string(), "hls".to_string(), Some("hevc".to_string()), Some("aac".to_string()));
        assert!(probe.is_hevc);
        assert!(probe.audio_is_aac);
        assert!(probe.needs_transcode);
        assert_eq!(probe.recommended_profile, "firetv_stick");
    }

    #[test]
    fn hls_playlist_probe_detects_h264_and_ac3() {
        let hls = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=4000000,CODECS=\"avc1.4d401f,ac-3\"\nchunklist.m3u8";
        let res = probe_hls_playlist(hls);
        assert_eq!(res, Some((Some("h264".to_string()), Some("ac3".to_string()))));

        let probe = StreamProbeResult::new("http://example.com/live.m3u8".to_string(), "hls".to_string(), Some("h264".to_string()), Some("ac3".to_string()));
        assert!(!probe.is_hevc);
        assert!(!probe.audio_is_aac);
        assert!(probe.needs_transcode);
        assert_eq!(probe.recommended_profile, "audio_only_aac");
    }

    #[test]
    fn ts_probe_detects_hevc_and_ac3_from_synthetic_packets() {
        let mut buf = vec![0u8; 188 * 3];

        // Packet 0: PAT (PID 0) pointing program 1 to PMT PID 0x1000
        buf[0] = 0x47;
        buf[1] = 0x40; // PUSI = 1, PID = 0
        buf[2] = 0x00;
        buf[3] = 0x10; // no adaptation field, CC = 0
        buf[4] = 0x00; // pointer field = 0
        buf[5] = 0x00; // Table ID = PAT
        buf[6] = 0xB0; // section syntax + length high
        buf[7] = 0x0D; // section length = 13
        buf[8] = 0x00; // TS ID high
        buf[9] = 0x01; // TS ID low
        buf[10] = 0xC1;
        buf[11] = 0x00;
        buf[12] = 0x00;
        // Program 1 -> PMT PID 0x1000
        buf[13] = 0x00;
        buf[14] = 0x01; // program number = 1
        buf[15] = 0xF0; // PMT PID high 5 bits (0x10)
        buf[16] = 0x00; // PMT PID low (0x00)

        // Packet 1: PMT (PID 0x1000) with HEVC (0x24) and AC3 (0x81)
        let p1 = 188;
        buf[p1] = 0x47;
        buf[p1 + 1] = 0x50; // PUSI = 1, PID = 0x1000 (high: 0x10)
        buf[p1 + 2] = 0x00; // PID low: 0x00
        buf[p1 + 3] = 0x10;
        buf[p1 + 4] = 0x00; // pointer field
        buf[p1 + 5] = 0x02; // Table ID = PMT
        buf[p1 + 6] = 0xB0;
        buf[p1 + 7] = 0x17; // section length = 23
        buf[p1 + 8] = 0x00;
        buf[p1 + 9] = 0x01;
        buf[p1 + 10] = 0xC1;
        buf[p1 + 11] = 0x00;
        buf[p1 + 12] = 0x00;
        buf[p1 + 13] = 0xE1; // PCR PID
        buf[p1 + 14] = 0x00;
        buf[p1 + 15] = 0xF0; // Program info len = 0
        buf[p1 + 16] = 0x00;
        // Stream 1: HEVC (0x24)
        buf[p1 + 17] = 0x24; // stream type = HEVC
        buf[p1 + 18] = 0xE1; // elementary PID = 0x0101
        buf[p1 + 19] = 0x01;
        buf[p1 + 20] = 0xF0; // ES info len = 0
        buf[p1 + 21] = 0x00;
        // Stream 2: AC-3 (0x81)
        buf[p1 + 22] = 0x81; // stream type = AC-3
        buf[p1 + 23] = 0xE1; // elementary PID = 0x0102
        buf[p1 + 24] = 0x02;
        buf[p1 + 25] = 0xF0; // ES info len = 0
        buf[p1 + 26] = 0x00;

        // Packet 2: sync packet
        let p2 = 376;
        buf[p2] = 0x47;

        let res = probe_ts_buffer(&buf);
        assert_eq!(res, Some((Some("hevc".to_string()), Some("ac3".to_string()))));

        let probe = StreamProbeResult::new("http://example.com/live.ts".to_string(), "mpegts".to_string(), Some("hevc".to_string()), Some("ac3".to_string()));
        assert!(probe.is_hevc);
        assert!(!probe.audio_is_aac);
        assert!(probe.needs_transcode);
        assert_eq!(probe.recommended_profile, "firetv_stick");
    }
}
