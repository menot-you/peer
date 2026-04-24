//! Google Veo (via Gemini API) video provider.
//!
//! Async flow:
//! 1. `POST {base_url}/models/{model}:predictLongRunning?key={API_KEY}`
//!    body: `{"instances": [{"prompt": "..."}], "parameters": {"aspectRatio": "16:9"}}`
//!    → `{"name": "models/veo-3.0-generate-preview/operations/<id>"}`
//! 2. Poll `GET {base_url}/{operation_name}?key={API_KEY}` every 10 s
//!    → when `done=true`: `{"response": {"generateVideoResponse":
//!      {"generatedSamples": [{"video": {"uri": "..."}}]}}}`
//! 3. `GET {video_uri}&key={API_KEY}` → MP4 bytes.
//!
//! Configure in `peer.toml` with
//! `base_url = "https://generativelanguage.googleapis.com/v1beta"`.

use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::time::sleep;

use crate::error::PeerError;
use crate::image::http::{build_client, http_error, payload_error};
use crate::registry::BackendSpec;

use super::{
    resolve_api_key, resolve_timeout, session::VideoContext, VideoBackend, VideoOutcome,
    VideoRequest,
};

pub struct VeoBackend;

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_MODEL: &str = "veo-3.0-fast-generate-preview";
const POLL_INTERVAL_MS: u64 = 10_000;

#[derive(Serialize)]
struct CreateBody<'a> {
    instances: Vec<Instance<'a>>,
    parameters: Parameters<'a>,
}

#[derive(Serialize)]
struct Instance<'a> {
    prompt: &'a str,
}

#[derive(Serialize)]
struct Parameters<'a> {
    #[serde(rename = "aspectRatio", skip_serializing_if = "Option::is_none")]
    aspect_ratio: Option<&'a str>,
    #[serde(rename = "personGeneration")]
    person_generation: &'static str,
}

impl VideoBackend for VeoBackend {
    async fn generate(
        &self,
        spec: &BackendSpec,
        req: &VideoRequest,
        ctx: &VideoContext,
    ) -> Result<VideoOutcome, PeerError> {
        if req.first_frame_image.is_some() {
            return Err(PeerError::InvalidInput(format!(
                "backend {} (veo) does not yet support first_frame_image — use minimax-video for image conditioning",
                spec.name
            )));
        }

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
        // Per-HTTP-call timeout is 60s; overall budget is governed by the
        // polling loop against `timeout_ms`.
        let client = build_client(60_000)?;

        let aspect_owned = req
            .aspect_ratio
            .clone()
            .or_else(|| spec.aspect_ratio_default.clone());
        let body = CreateBody {
            instances: vec![Instance {
                prompt: &req.prompt,
            }],
            parameters: Parameters {
                aspect_ratio: aspect_owned.as_deref(),
                // Veo requires an explicit person-generation policy; "allow_all"
                // matches the liberal default of Gemini Studio.
                person_generation: "allow_all",
            },
        };

        let create_url = format!("{base_url}/models/{model}:predictLongRunning?key={api_key}");
        tracing::info!(
            target = "peer::video::veo",
            backend = %spec.name,
            model = %model,
            "POST predictLongRunning (create)"
        );

        let create_resp = client
            .post(&create_url)
            .json(&body)
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
                message: format!(
                    "HTTP {create_status} on create — {}",
                    truncate(&create_text)
                ),
            });
        }
        let create_json: Value = serde_json::from_str(&create_text)
            .map_err(|e| payload_error(&spec.name, format!("invalid JSON on create: {e}")))?;
        let operation_name = create_json
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| payload_error(&spec.name, "create response missing `name` (operation)"))?
            .to_string();

        tracing::info!(
            target = "peer::video::veo",
            operation = %operation_name,
            "operation enqueued — polling"
        );

        // ---- Poll until done ----
        let started = Instant::now();
        let deadline = Duration::from_millis(timeout_ms);
        let video_uri = loop {
            if started.elapsed() >= deadline {
                return Err(PeerError::Timeout {
                    backend: spec.name.clone(),
                    elapsed_ms: timeout_ms,
                });
            }
            sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;

            let poll_url = format!("{base_url}/{operation_name}?key={api_key}");
            let poll_resp = client
                .get(&poll_url)
                .send()
                .await
                .map_err(|e| http_error(&spec.name, e))?;
            let p_status = poll_resp.status();
            let p_text = poll_resp
                .text()
                .await
                .map_err(|e| http_error(&spec.name, e))?;
            if !p_status.is_success() {
                return Err(PeerError::HttpFailure {
                    backend: spec.name.clone(),
                    message: format!("HTTP {p_status} on poll — {}", truncate(&p_text)),
                });
            }
            let p_json: Value = serde_json::from_str(&p_text)
                .map_err(|e| payload_error(&spec.name, format!("invalid JSON on poll: {e}")))?;
            if let Some(err) = p_json.get("error") {
                return Err(PeerError::HttpFailure {
                    backend: spec.name.clone(),
                    message: format!("operation error: {err}"),
                });
            }
            let done = p_json
                .get("done")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            tracing::debug!(
                target = "peer::video::veo",
                operation = %operation_name,
                done,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "poll tick"
            );
            if !done {
                continue;
            }
            let Some(uri) = extract_video_uri(&p_json) else {
                return Err(payload_error(
                    &spec.name,
                    format!(
                        "done=true but no video URI in response: {}",
                        truncate(&p_text)
                    ),
                ));
            };
            break uri;
        };

        // ---- Download MP4 ----
        // Response URIs already include their own auth, but the Google API
        // requires the key query param on the download too.
        let download_url = if video_uri.contains('?') {
            format!("{video_uri}&key={api_key}")
        } else {
            format!("{video_uri}?key={api_key}")
        };
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
            target = "peer::video::veo",
            path = %ctx.output_path.display(),
            bytes = bytes.len(),
            "saved video"
        );

        Ok(VideoOutcome {
            paths: vec![ctx.output_path.clone()],
            task_id: Some(operation_name),
        })
    }
}

/// Walk the nested Veo response tree looking for the first video URI.
/// The stable shape today is
/// `response.generateVideoResponse.generatedSamples[].video.uri`, but we
/// also look at alternates we've seen in preview responses.
fn extract_video_uri(value: &Value) -> Option<String> {
    // Primary: response.generateVideoResponse.generatedSamples[0].video.uri
    if let Some(uri) = value
        .pointer("/response/generateVideoResponse/generatedSamples/0/video/uri")
        .and_then(|v| v.as_str())
    {
        return Some(uri.to_string());
    }
    // Alternate shape: response.generatedVideos[0].video.uri
    if let Some(uri) = value
        .pointer("/response/generatedVideos/0/video/uri")
        .and_then(|v| v.as_str())
    {
        return Some(uri.to_string());
    }
    // Fallback: search for any `uri` nested under a `video` key.
    fn walk(v: &Value) -> Option<String> {
        if let Some(obj) = v.as_object() {
            if let Some(video) = obj.get("video") {
                if let Some(uri) = video.get("uri").and_then(|u| u.as_str()) {
                    return Some(uri.to_string());
                }
            }
            for (_, inner) in obj {
                if let Some(found) = walk(inner) {
                    return Some(found);
                }
            }
        } else if let Some(arr) = v.as_array() {
            for inner in arr {
                if let Some(found) = walk(inner) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(value)
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
    fn extract_uri_primary_shape() {
        let v = serde_json::json!({
            "done": true,
            "response": {
                "generateVideoResponse": {
                    "generatedSamples": [
                        {"video": {"uri": "https://example/v.mp4"}}
                    ]
                }
            }
        });
        assert_eq!(
            extract_video_uri(&v),
            Some("https://example/v.mp4".to_string())
        );
    }

    #[test]
    fn extract_uri_alternate_shape() {
        let v = serde_json::json!({
            "done": true,
            "response": {
                "generatedVideos": [
                    {"video": {"uri": "https://alt/x.mp4"}}
                ]
            }
        });
        assert_eq!(extract_video_uri(&v), Some("https://alt/x.mp4".to_string()));
    }

    #[test]
    fn extract_uri_deep_fallback() {
        let v = serde_json::json!({
            "done": true,
            "result": {"some": {"nested": {"video": {"uri": "https://deep/z.mp4"}}}}
        });
        assert_eq!(
            extract_video_uri(&v),
            Some("https://deep/z.mp4".to_string())
        );
    }

    #[test]
    fn extract_uri_none_when_missing() {
        let v = serde_json::json!({"done": true, "response": {}});
        assert_eq!(extract_video_uri(&v), None);
    }
}
