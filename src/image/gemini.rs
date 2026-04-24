//! Nano Banana (Gemini image) HTTP provider.
//!
//! Endpoint: `{base_url}/models/{model}:generateContent?key={api_key}`.
//!
//! Payload shape (generate):
//! ```json
//! {
//!   "contents": [{"parts": [{"text": "..."}]}],
//!   "generationConfig": {
//!     "responseModalities": ["IMAGE", "TEXT"],
//!     "imageConfig": {"aspectRatio": "16:9"}
//!   }
//! }
//! ```
//!
//! Edit mode appends `{"inlineData": {"mimeType": "...", "data": "<b64>"}}`
//! parts for the input image + any `reference_images`.
//!
//! Response: `candidates[0].content.parts[]` — iterate and take entries with
//! `inlineData.data` (base64 PNG).

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::error::PeerError;
use crate::registry::BackendSpec;

use super::http::{
    build_client, http_error, mime_for, payload_error, read_as_base64, write_image_bytes,
};
use super::{resolve_api_key, resolve_timeout, session::ImageContext, ImageBackend, ImageRequest};

pub struct GeminiImageBackend;

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_MODEL: &str = "gemini-3.1-flash-image-preview";

#[derive(Serialize)]
struct Payload {
    contents: Vec<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct Content {
    parts: Vec<Part>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Part {
    Text {
        text: String,
    },
    Inline {
        #[serde(rename = "inlineData")]
        inline_data: InlineData,
    },
}

#[derive(Serialize)]
struct InlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

#[derive(Serialize)]
struct GenerationConfig {
    #[serde(rename = "responseModalities")]
    response_modalities: Vec<String>,
    #[serde(rename = "imageConfig", skip_serializing_if = "Option::is_none")]
    image_config: Option<ImageConfig>,
}

#[derive(Serialize)]
struct ImageConfig {
    #[serde(rename = "aspectRatio")]
    aspect_ratio: String,
}

impl GeminiImageBackend {
    async fn build_parts(
        &self,
        backend: &str,
        prompt_text: &str,
        input_path: Option<&std::path::Path>,
        reference_images: &[PathBuf],
    ) -> Result<Vec<Part>, PeerError> {
        let mut parts: Vec<Part> = vec![Part::Text {
            text: prompt_text.to_string(),
        }];
        if let Some(input) = input_path {
            let b64 = read_as_base64(input).await?;
            parts.push(Part::Inline {
                inline_data: InlineData {
                    mime_type: mime_for(input).to_string(),
                    data: b64,
                },
            });
        }
        for ref_path in reference_images {
            let b64 = read_as_base64(ref_path).await.map_err(|e| match e {
                PeerError::Io(ioe) => payload_error(
                    backend,
                    format!("reference_images[{}]: {}", ref_path.display(), ioe),
                ),
                other => other,
            })?;
            parts.push(Part::Inline {
                inline_data: InlineData {
                    mime_type: mime_for(ref_path).to_string(),
                    data: b64,
                },
            });
        }
        Ok(parts)
    }

    async fn call_once(
        &self,
        spec: &BackendSpec,
        req: &ImageRequest,
        prompt_text: &str,
        input_path: Option<&std::path::Path>,
    ) -> Result<Vec<Vec<u8>>, PeerError> {
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
        let aspect = req
            .aspect_ratio
            .clone()
            .or_else(|| spec.aspect_ratio_default.clone());

        let parts = self
            .build_parts(&spec.name, prompt_text, input_path, &req.reference_images)
            .await?;
        let payload = Payload {
            contents: vec![Content { parts }],
            generation_config: GenerationConfig {
                response_modalities: vec!["IMAGE".into(), "TEXT".into()],
                image_config: aspect.map(|ar| ImageConfig { aspect_ratio: ar }),
            },
        };

        let timeout_ms = resolve_timeout(spec, req.timeout_ms);
        let client = build_client(timeout_ms)?;
        let url = format!(
            "{}/models/{}:generateContent?key={}",
            base_url.trim_end_matches('/'),
            model,
            api_key
        );

        tracing::info!(
            target = "peer::image::gemini",
            backend = %spec.name,
            model = %model,
            "POST generateContent"
        );

        let resp = client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| http_error(&spec.name, e))?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| http_error(&spec.name, e))?;
        if !status.is_success() {
            return Err(PeerError::HttpFailure {
                backend: spec.name.clone(),
                message: format!("HTTP {} — {}", status, truncate_for_log(&body)),
            });
        }

        let value: Value = serde_json::from_str(&body)
            .map_err(|e| payload_error(&spec.name, format!("invalid JSON: {e}")))?;
        extract_image_bytes(&spec.name, &value)
    }
}

fn extract_image_bytes(backend: &str, value: &Value) -> Result<Vec<Vec<u8>>, PeerError> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let candidates = value
        .get("candidates")
        .and_then(|v| v.as_array())
        .ok_or_else(|| payload_error(backend, "response missing `candidates` array"))?;

    if candidates.is_empty() {
        return Err(payload_error(backend, "response has zero candidates"));
    }

    let mut out = Vec::new();
    for cand in candidates {
        let parts = cand
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array());
        if let Some(parts) = parts {
            for part in parts {
                if let Some(data) = part
                    .get("inlineData")
                    .and_then(|i| i.get("data"))
                    .and_then(|d| d.as_str())
                {
                    let bytes = STANDARD
                        .decode(data.trim())
                        .map_err(|e| payload_error(backend, format!("base64 decode: {e}")))?;
                    if bytes.len() < 16 {
                        return Err(payload_error(
                            backend,
                            format!("inline image is only {} bytes", bytes.len()),
                        ));
                    }
                    out.push(bytes);
                }
            }
        }
    }

    if out.is_empty() {
        return Err(payload_error(
            backend,
            "response contained no inline image parts",
        ));
    }
    Ok(out)
}

fn truncate_for_log(s: &str) -> String {
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

impl ImageBackend for GeminiImageBackend {
    async fn generate(
        &self,
        spec: &BackendSpec,
        req: &ImageRequest,
        ctx: &ImageContext,
    ) -> Result<Vec<PathBuf>, PeerError> {
        let n = req.n.max(1) as usize;
        let mut written: Vec<PathBuf> = Vec::with_capacity(n);
        for (idx, target) in ctx.output_paths.iter().enumerate() {
            let bytes_batches = self.call_once(spec, req, &req.prompt, None).await?;
            let Some(first) = bytes_batches.into_iter().next() else {
                return Err(payload_error(&spec.name, "no image bytes on iteration"));
            };
            let actual = write_image_bytes(&spec.name, &first, target).await?;
            tracing::debug!(
                target = "peer::image::gemini",
                idx,
                path = %actual.display(),
                bytes = first.len(),
                "saved generated image"
            );
            written.push(actual);
        }
        Ok(written)
    }

    async fn edit(
        &self,
        spec: &BackendSpec,
        req: &ImageRequest,
        ctx: &ImageContext,
    ) -> Result<Vec<PathBuf>, PeerError> {
        let input = req
            .input_path
            .as_ref()
            .ok_or_else(|| PeerError::InvalidInput("edit requires `input_path`".into()))?;
        let edit_prompt = req
            .edit_prompt
            .as_deref()
            .or(Some(req.prompt.as_str()))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                PeerError::InvalidInput("edit requires non-empty `edit_prompt` or `prompt`".into())
            })?;

        let target = ctx
            .output_paths
            .first()
            .ok_or_else(|| PeerError::InvalidInput("no planned output_path".into()))?;
        let bytes_batches = self.call_once(spec, req, edit_prompt, Some(input)).await?;
        let Some(first) = bytes_batches.into_iter().next() else {
            return Err(payload_error(&spec.name, "no image bytes on edit"));
        };
        let actual = write_image_bytes(&spec.name, &first, target).await?;
        tracing::debug!(
            target = "peer::image::gemini",
            path = %actual.display(),
            bytes = first.len(),
            "saved edited image"
        );
        Ok(vec![actual])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_image_bytes_succeeds_on_well_formed() {
        let sample = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "ok"},
                        {"inlineData": {"mimeType": "image/png", "data": "aGVsbG8gd29ybGQgZnJvbSBnZW1pbmk="}}
                    ]
                }
            }]
        });
        let out = extract_image_bytes("nanobanana", &sample).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].len() >= 16);
    }

    #[test]
    fn extract_errors_when_no_inline_parts() {
        let sample = serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": "only text"}]}}]
        });
        let err = extract_image_bytes("nanobanana", &sample).unwrap_err();
        match err {
            PeerError::ProviderPayload { backend, .. } => assert_eq!(backend, "nanobanana"),
            _ => panic!("wrong error variant"),
        }
    }

    #[test]
    fn extract_errors_when_candidates_empty() {
        let sample = serde_json::json!({"candidates": []});
        assert!(extract_image_bytes("nanobanana", &sample).is_err());
    }
}
