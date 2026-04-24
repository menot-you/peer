//! MiniMax image-01 HTTP provider.
//!
//! Endpoint: `POST {base_url}` (usually `https://api.minimaxi.com/v1/image_generation`).
//! Authorization: `Bearer {api_key}`.
//!
//! Request payload:
//! ```json
//! {
//!   "model": "image-01",
//!   "prompt": "...",
//!   "aspect_ratio": "16:9",
//!   "response_format": "base64",
//!   "n": 1,
//!   "prompt_optimizer": true
//! }
//! ```
//!
//! Response: `data.image_base64` array of base64 PNGs. `base_resp.status_code`
//! is 0 on success, non-zero on API-level failure.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::error::PeerError;
use crate::registry::BackendSpec;

use super::http::{build_client, http_error, payload_error, write_base64_png};
use super::{resolve_api_key, resolve_timeout, session::ImageContext, ImageBackend, ImageRequest};

pub struct MinimaxImageBackend;

const DEFAULT_BASE_URL: &str = "https://api.minimaxi.com/v1/image_generation";
const DEFAULT_MODEL: &str = "image-01";

#[derive(Serialize)]
struct Payload<'a> {
    model: &'a str,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    aspect_ratio: Option<&'a str>,
    response_format: &'a str,
    n: u8,
    prompt_optimizer: bool,
}

impl ImageBackend for MinimaxImageBackend {
    async fn generate(
        &self,
        spec: &BackendSpec,
        req: &ImageRequest,
        ctx: &ImageContext,
    ) -> Result<Vec<PathBuf>, PeerError> {
        let api_key = resolve_api_key(spec)?;
        let model = req
            .model
            .clone()
            .or_else(|| spec.model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let base_url = spec
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let aspect_owned = req
            .aspect_ratio
            .clone()
            .or_else(|| spec.aspect_ratio_default.clone());
        let aspect = aspect_owned.as_deref();

        let n = req.n.max(1);
        let payload = Payload {
            model: &model,
            prompt: &req.prompt,
            aspect_ratio: aspect,
            response_format: "base64",
            n,
            prompt_optimizer: true,
        };

        let timeout_ms = resolve_timeout(spec, req.timeout_ms);
        let client = build_client(timeout_ms)?;

        tracing::info!(
            target = "peer::image::minimax",
            backend = %spec.name,
            model = %model,
            n = n,
            "POST image_generation"
        );

        let resp = client
            .post(&base_url)
            .bearer_auth(&api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| http_error(&spec.name, e))?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| http_error(&spec.name, e))?;
        if !status.is_success() {
            return Err(PeerError::HttpFailure {
                backend: spec.name.clone(),
                message: format!("HTTP {status} — {}", truncate(&body)),
            });
        }

        let value: Value = serde_json::from_str(&body)
            .map_err(|e| payload_error(&spec.name, format!("invalid JSON: {e}")))?;
        check_base_resp(&spec.name, &value)?;

        let b64_list = extract_base64_list(&spec.name, &value)?;
        if b64_list.is_empty() {
            return Err(payload_error(&spec.name, "data.image_base64 was empty"));
        }

        let mut written = Vec::with_capacity(b64_list.len());
        for (idx, b64) in b64_list.into_iter().enumerate() {
            let target = ctx.output_paths.get(idx).cloned().unwrap_or_else(|| {
                ctx.base_dir
                    .join(format!("{}-extra-{}.png", ctx.session_id, idx + 1))
            });
            write_base64_png(&spec.name, &b64, &target).await?;
            written.push(target);
        }

        Ok(written)
    }

    async fn edit(
        &self,
        spec: &BackendSpec,
        _req: &ImageRequest,
        _ctx: &ImageContext,
    ) -> Result<Vec<PathBuf>, PeerError> {
        Err(PeerError::InvalidInput(format!(
            "backend {} (minimax image-01) does not support edit — use nanobanana or codex",
            spec.name
        )))
    }
}

fn check_base_resp(backend: &str, value: &Value) -> Result<(), PeerError> {
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
                message: format!("MiniMax base_resp {status_code}: {msg}"),
            });
        }
    }
    Ok(())
}

fn extract_base64_list(backend: &str, value: &Value) -> Result<Vec<String>, PeerError> {
    let data = value
        .get("data")
        .ok_or_else(|| payload_error(backend, "response missing `data` object"))?;
    let arr = data
        .get("image_base64")
        .and_then(|v| v.as_array())
        .ok_or_else(|| payload_error(backend, "data.image_base64 missing or not array"))?;
    let out: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    Ok(out)
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
    fn extract_works_on_valid_payload() {
        let sample = serde_json::json!({
            "data": {"image_base64": ["YWJjZGVmMTIzNDU2Nzg5MA=="]},
            "base_resp": {"status_code": 0, "status_msg": ""}
        });
        let list = extract_base64_list("minimax-image", &sample).unwrap();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn base_resp_non_zero_is_http_failure() {
        let sample = serde_json::json!({
            "base_resp": {"status_code": 1008, "status_msg": "invalid params"}
        });
        let err = check_base_resp("minimax-image", &sample).unwrap_err();
        match err {
            PeerError::HttpFailure { message, .. } => assert!(message.contains("1008")),
            _ => panic!("wrong error variant"),
        }
    }

    #[test]
    fn missing_data_is_payload_error() {
        let sample = serde_json::json!({"base_resp": {"status_code": 0}});
        assert!(extract_base64_list("minimax-image", &sample).is_err());
    }
}
