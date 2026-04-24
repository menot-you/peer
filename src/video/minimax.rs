//! MiniMax video HTTP provider (async task flow).
//!
//! Three-step async contract (global endpoint: `https://api.minimax.io/v1`):
//!
//! 1. `POST /video_generation` with `{model, prompt, prompt_optimizer, first_frame_image?}`
//!    → `{task_id, base_resp}`.
//! 2. `GET /query/video_generation?task_id=<id>` polled every 5s
//!    → `{status: "Queueing"|"Preparing"|"Processing"|"Success"|"Fail", file_id, base_resp}`.
//! 3. On `status=Success`, `GET /files/retrieve?file_id=<id>`
//!    → `{file: {download_url}}`. Then GET download_url for the raw MP4.
//!
//! Configure in `peer.toml` with `base_url = "https://api.minimax.io/v1"`.

use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::time::sleep;

use crate::error::PeerError;
use crate::image::http::{build_client, http_error, payload_error};
use crate::registry::BackendSpec;

use super::{resolve_api_key, resolve_timeout, session::VideoContext, VideoBackend, VideoOutcome, VideoRequest};

pub struct MinimaxVideoBackend;

const DEFAULT_BASE_URL: &str = "https://api.minimax.io/v1";
const DEFAULT_MODEL: &str = "MiniMax-Hailuo-02";
const POLL_INTERVAL_MS: u64 = 5_000;

#[derive(Serialize)]
struct CreatePayload<'a> {
    model: &'a str,
    prompt: &'a str,
    prompt_optimizer: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_frame_image: Option<String>,
}

impl VideoBackend for MinimaxVideoBackend {
    async fn generate(
        &self,
        spec: &BackendSpec,
        req: &VideoRequest,
        ctx: &VideoContext,
    ) -> Result<VideoOutcome, PeerError> {
        let api_key = resolve_api_key(spec)?;
        let model = req
            .model
            .clone()
            .or_else(|| spec.model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let base_url_owned = spec
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let base_url = base_url_owned.trim_end_matches('/');

        let timeout_ms = resolve_timeout(spec, req.timeout_ms);
        // Per-HTTP-call timeout is 60s; the overall wait is governed by the
        // polling loop using `timeout_ms` as the wall-clock budget.
        let client = build_client(60_000)?;

        // ---- 1. Create task ----
        let first_frame = match &req.first_frame_image {
            Some(p) => Some(encode_image_data_url(p).await?),
            None => None,
        };
        let create_body = CreatePayload {
            model: &model,
            prompt: &req.prompt,
            prompt_optimizer: true,
            first_frame_image: first_frame,
        };

        tracing::info!(
            target = "peer::video::minimax",
            backend = %spec.name,
            model = %model,
            "POST video_generation (create)"
        );

        let create_resp = client
            .post(format!("{base_url}/video_generation"))
            .bearer_auth(&api_key)
            .json(&create_body)
            .send()
            .await
            .map_err(|e| http_error(&spec.name, e))?;
        let create_status = create_resp.status();
        let create_text = create_resp
            .text()
            .await
            .map_err(|e| http_error(&spec.name, e))?;
        if !create_status.is_success() {
            return Err(PeerError::HttpFailure {
                backend: spec.name.clone(),
                message: format!("HTTP {create_status} on create — {}", truncate(&create_text)),
            });
        }
        let create_json: Value = serde_json::from_str(&create_text)
            .map_err(|e| payload_error(&spec.name, format!("invalid JSON on create: {e}")))?;
        check_base_resp(&spec.name, &create_json, "create")?;
        let task_id = create_json
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| payload_error(&spec.name, "create response missing `task_id`"))?
            .to_string();

        tracing::info!(
            target = "peer::video::minimax",
            task_id = %task_id,
            "task enqueued — polling for completion"
        );

        // ---- 2. Poll until Success / Fail / timeout ----
        let started = Instant::now();
        let deadline = Duration::from_millis(timeout_ms);
        let file_id = loop {
            if started.elapsed() >= deadline {
                return Err(PeerError::Timeout {
                    backend: spec.name.clone(),
                    elapsed_ms: timeout_ms,
                });
            }
            sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;

            let query_resp = client
                .get(format!("{base_url}/query/video_generation"))
                .bearer_auth(&api_key)
                .query(&[("task_id", task_id.as_str())])
                .send()
                .await
                .map_err(|e| http_error(&spec.name, e))?;
            let q_status = query_resp.status();
            let q_text = query_resp
                .text()
                .await
                .map_err(|e| http_error(&spec.name, e))?;
            if !q_status.is_success() {
                return Err(PeerError::HttpFailure {
                    backend: spec.name.clone(),
                    message: format!("HTTP {q_status} on poll — {}", truncate(&q_text)),
                });
            }
            let q_json: Value = serde_json::from_str(&q_text)
                .map_err(|e| payload_error(&spec.name, format!("invalid JSON on poll: {e}")))?;
            check_base_resp(&spec.name, &q_json, "poll")?;
            let status_str = q_json
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown");
            tracing::debug!(
                target = "peer::video::minimax",
                task_id = %task_id,
                status = %status_str,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "poll tick"
            );
            match status_str {
                "Success" => {
                    let fid = q_json
                        .get("file_id")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            payload_error(&spec.name, "Success status but no file_id returned")
                        })?
                        .to_string();
                    break fid;
                }
                "Fail" => {
                    return Err(PeerError::HttpFailure {
                        backend: spec.name.clone(),
                        message: format!(
                            "task {task_id} failed: {}",
                            truncate(&q_text)
                        ),
                    });
                }
                _ => continue, // Queueing | Preparing | Processing | Unknown
            }
        };

        // ---- 3. Resolve file_id → download_url → save MP4 ----
        let files_resp = client
            .get(format!("{base_url}/files/retrieve"))
            .bearer_auth(&api_key)
            .query(&[("file_id", file_id.as_str())])
            .send()
            .await
            .map_err(|e| http_error(&spec.name, e))?;
        let f_status = files_resp.status();
        let f_text = files_resp
            .text()
            .await
            .map_err(|e| http_error(&spec.name, e))?;
        if !f_status.is_success() {
            return Err(PeerError::HttpFailure {
                backend: spec.name.clone(),
                message: format!("HTTP {f_status} on files/retrieve — {}", truncate(&f_text)),
            });
        }
        let f_json: Value = serde_json::from_str(&f_text)
            .map_err(|e| payload_error(&spec.name, format!("invalid JSON on files: {e}")))?;
        check_base_resp(&spec.name, &f_json, "files")?;
        let download_url = f_json
            .get("file")
            .and_then(|f| f.get("download_url"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| payload_error(&spec.name, "files response missing `file.download_url`"))?
            .to_string();

        let dl_resp = client
            .get(&download_url)
            .send()
            .await
            .map_err(|e| http_error(&spec.name, e))?;
        if !dl_resp.status().is_success() {
            return Err(PeerError::HttpFailure {
                backend: spec.name.clone(),
                message: format!("HTTP {} downloading MP4", dl_resp.status()),
            });
        }
        let bytes = dl_resp
            .bytes()
            .await
            .map_err(|e| http_error(&spec.name, e))?;
        if bytes.len() < 1024 {
            return Err(payload_error(
                &spec.name,
                format!("downloaded MP4 is only {} bytes", bytes.len()),
            ));
        }

        tokio::fs::write(&ctx.output_path, &bytes)
            .await
            .map_err(PeerError::Io)?;

        tracing::info!(
            target = "peer::video::minimax",
            path = %ctx.output_path.display(),
            bytes = bytes.len(),
            "saved video"
        );

        Ok(VideoOutcome {
            paths: vec![ctx.output_path.clone()],
            task_id: Some(task_id),
        })
    }
}

/// Read the local image and return a `data:<mime>;base64,<b64>` string (the
/// shape MiniMax expects for `first_frame_image`).
async fn encode_image_data_url(path: &std::path::Path) -> Result<String, PeerError> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let bytes = tokio::fs::read(path).await.map_err(PeerError::Io)?;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "image/png",
    };
    let b64 = STANDARD.encode(bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

fn check_base_resp(backend: &str, value: &Value, phase: &str) -> Result<(), PeerError> {
    if let Some(base) = value.get("base_resp") {
        let status_code = base
            .get("status_code")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if status_code != 0 {
            let msg = base
                .get("status_msg")
                .and_then(|v| v.as_str())
                .unwrap_or("<no message>");
            return Err(PeerError::HttpFailure {
                backend: backend.to_string(),
                message: format!("MiniMax base_resp {status_code} during {phase}: {msg}"),
            });
        }
    }
    Ok(())
}

fn truncate(s: &str) -> String {
    if s.len() > 512 {
        let mut end = 512;
        while end < s.len() && !s.is_char_boundary(end) {
            end += 1;
        }
        format!("{}...(truncated)", &s[..end])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_resp_zero_is_ok() {
        let v = serde_json::json!({"base_resp": {"status_code": 0}});
        assert!(check_base_resp("minimax-video", &v, "create").is_ok());
    }

    #[test]
    fn base_resp_non_zero_is_http_failure() {
        let v = serde_json::json!({"base_resp": {"status_code": 1008, "status_msg": "invalid"}});
        let err = check_base_resp("minimax-video", &v, "poll").unwrap_err();
        match err {
            PeerError::HttpFailure { message, .. } => {
                assert!(message.contains("1008"));
                assert!(message.contains("poll"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("abc"), "abc");
    }
}
