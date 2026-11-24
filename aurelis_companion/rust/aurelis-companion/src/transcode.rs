//! Stream transcode and probe routes: Silent transcoding/remuxing via FFmpeg for older or
//! constrained devices, and stream probe inspection.

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    response::{IntoResponse, Response},
};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use tokio_util::io::ReaderStream;

use crate::state::AppState;
use crate::stream_relay::url_hash;

static FFMPEG_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Returns true if ffmpeg binary is found and executable on the host.
pub fn is_ffmpeg_available() -> bool {
    *FFMPEG_AVAILABLE.get_or_init(|| {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Generates the argument vector for FFmpeg based on the requested profile.
pub fn build_ffmpeg_args(stream_url: &str, profile: &str) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-reconnect".to_string(),
        "1".to_string(),
        "-reconnect_streamed".to_string(),
        "1".to_string(),
        "-reconnect_delay_max".to_string(),
        "5".to_string(),
        "-i".to_string(),
        stream_url.to_string(),
    ];

    match profile {
        "firetv_stick" => {
            args.extend([
                "-c:v".to_string(),
                "libx264".to_string(),
                "-preset".to_string(),
                "veryfast".to_string(),
                "-profile:v".to_string(),
                "main".to_string(),
                "-vf".to_string(),
                "scale=min(1280\\,iw):-2".to_string(),
                "-maxrate".to_string(),
                "4500k".to_string(),
                "-bufsize".to_string(),
                "9000k".to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
                "-c:a".to_string(),
                "aac".to_string(),
                "-b:a".to_string(),
                "128k".to_string(),
                "-ac".to_string(),
                "2".to_string(),
            ]);
        }
        "audio_only_aac" => {
            args.extend([
                "-c:v".to_string(),
                "copy".to_string(),
                "-c:a".to_string(),
                "aac".to_string(),
                "-b:a".to_string(),
                "192k".to_string(),
                "-ac".to_string(),
                "2".to_string(),
            ]);
        }
        "remux" => {
            args.extend([
                "-c".to_string(),
                "copy".to_string(),
            ]);
        }
        _ => {
            // "compatibility" (default)
            args.extend([
                "-c:v".to_string(),
                "libx264".to_string(),
                "-preset".to_string(),
                "veryfast".to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
                "-c:a".to_string(),
                "aac".to_string(),
                "-b:a".to_string(),
                "192k".to_string(),
                "-ac".to_string(),
                "2".to_string(),
            ]);
        }
    }

    args.extend([
        "-f".to_string(),
        "mpegts".to_string(),
        "pipe:1".to_string(),
    ]);

    args
}

fn parse_query_params(query_str: Option<&str>) -> (Option<String>, Option<String>, Option<String>, Option<i64>) {
    let mut url = None;
    let mut profile = None;
    let mut token = None;
    let mut expires = None;

    if let Some(q) = query_str {
        for part in q.split('&') {
            if let Some((k, v)) = part.split_once('=') {
                match k {
                    "url" => url = Some(urlencoding_decode(v)),
                    "profile" => profile = Some(urlencoding_decode(v)),
                    "token" => token = Some(v.to_string()),
                    "expires" => expires = v.parse::<i64>().ok(),
                    _ => {}
                }
            }
        }
    }

    (url, profile, token, expires)
}

fn urlencoding_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let h1 = chars.next().unwrap_or('0');
            let h2 = chars.next().unwrap_or('0');
            if let Ok(byte) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                result.push(byte as char);
            }
        } else if ch == '+' {
            result.push(' ');
        } else {
            result.push(ch);
        }
    }
    result
}

/// `GET /v1/relay/transcode?url=...&profile=...&token=...&expires=...`
pub async fn transcode_stream_route(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let query_str = req.uri().query();
    let (url_opt, profile_opt, token_opt, expires_opt) = parse_query_params(query_str);

    let upstream_url = match url_opt {
        Some(u) if !u.trim().is_empty() => u,
        _ => return (StatusCode::BAD_REQUEST, "missing or empty url query parameter").into_response(),
    };

    let profile = profile_opt.unwrap_or_else(|| "compatibility".to_string());
    let key = url_hash(&upstream_url);

    // Verify HMAC token
    let authorized = match (token_opt, expires_opt) {
        (Some(token), Some(expires)) => {
            if let Some(secret) = state.auth.household_secret() {
                state.stream_relay.verify_stream_token(&secret, &key, &token, expires, state.wall_ms())
            } else {
                false
            }
        }
        _ => false,
    };

    if !authorized {
        return (StatusCode::UNAUTHORIZED, "invalid or expired stream token").into_response();
    }

    if !is_ffmpeg_available() {
        return (StatusCode::NOT_IMPLEMENTED, "ffmpeg is not installed on companion host").into_response();
    }

    let args = build_ffmpeg_args(&upstream_url, &profile);

    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to spawn ffmpeg: {e}")).into_response(),
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "failed to capture ffmpeg stdout").into_response(),
    };

    let stream = ReaderStream::new(stdout);
    let body = Body::from_stream(stream);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
        .header(header::CONNECTION, "close")
        .header("x-aurelis-transcode-profile", &profile)
        .body(body)
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

/// `GET /v1/relay/probe?url=...&token=...&expires=...`
pub async fn probe_route(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let query_str = req.uri().query();
    let (url_opt, _, token_opt, expires_opt) = parse_query_params(query_str);

    let upstream_url = match url_opt {
        Some(u) if !u.trim().is_empty() => u,
        _ => return (StatusCode::BAD_REQUEST, "missing or empty url query parameter").into_response(),
    };

    let key = url_hash(&upstream_url);

    // Verify HMAC token
    let authorized = match (token_opt, expires_opt) {
        (Some(token), Some(expires)) => {
            if let Some(secret) = state.auth.household_secret() {
                state.stream_relay.verify_stream_token(&secret, &key, &token, expires, state.wall_ms())
            } else {
                false
            }
        }
        _ => false,
    };

    if !authorized {
        return (StatusCode::UNAUTHORIZED, "invalid or expired stream token").into_response();
    }

    match crate::stream_probe::probe_stream_url(&state.stream_relay.client, &upstream_url).await {
        Ok(result) => {
            let json = serde_json::to_string(&result).unwrap_or_default();
            (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], json).into_response()
        }
        Err(why) => (StatusCode::BAD_GATEWAY, format!("probe failed: {why}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ffmpeg_args_compatibility_profile() {
        let args = build_ffmpeg_args("http://upstream.com/live.ts", "compatibility");
        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"http://upstream.com/live.ts".to_string()));
        assert!(args.contains(&"libx264".to_string()));
        assert!(args.contains(&"aac".to_string()));
        assert!(args.contains(&"-f".to_string()));
        assert!(args.contains(&"mpegts".to_string()));
        assert!(args.contains(&"pipe:1".to_string()));
    }

    #[test]
    fn build_ffmpeg_args_firetv_stick_profile() {
        let args = build_ffmpeg_args("http://upstream.com/live.ts", "firetv_stick");
        assert!(args.contains(&"libx264".to_string()));
        assert!(args.contains(&"4500k".to_string()));
        assert!(args.contains(&"scale=min(1280\\,iw):-2".to_string()));
    }

    #[test]
    fn build_ffmpeg_args_audio_only_aac_profile() {
        let args = build_ffmpeg_args("http://upstream.com/live.ts", "audio_only_aac");
        let mut cv_copy = false;
        for i in 0..args.len() - 1 {
            if args[i] == "-c:v" && args[i + 1] == "copy" {
                cv_copy = true;
                break;
            }
        }
        assert!(cv_copy);
        assert!(args.contains(&"aac".to_string()));
    }

    #[test]
    fn parse_query_params_decodes_values() {
        let q = "url=http%3A%2F%2Fexample.com%2Flive.ts&profile=firetv_stick&token=abc123def&expires=1700000000";
        let (url, profile, token, expires) = parse_query_params(Some(q));
        assert_eq!(url, Some("http://example.com/live.ts".to_string()));
        assert_eq!(profile, Some("firetv_stick".to_string()));
        assert_eq!(token, Some("abc123def".to_string()));
        assert_eq!(expires, Some(1700000000));
    }
}
