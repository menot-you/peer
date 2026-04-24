//! Image generation/edit dispatcher.
//!
//! The `image` MCP tool shares the backend registry with `ask` but adds a
//! second transport (`Http`) and a different output contract (file paths, not
//! verdict-parsed text). Backends declare `kinds = ["ask", "image"]` in
//! `peer.toml` and the dispatcher selects HTTP or CLI based on
//! [`crate::registry::Transport`].
//!
//! ## Design
//!
//! - Never returns raw bytes in the JSON response — every artifact is saved
//!   to disk and surfaced as an absolute [`PathBuf`]. Image bytes in JSON
//!   break LLM context windows.
//! - Default output root: `{project_root}/.nott/generated/images/`. Caller
//!   may override per-request with `output_path`.
//! - Session id pairs the image batch with a JSON sidecar containing the
//!   request + response + provider metadata.
//! - Each provider owns its own HTTP payload construction / subprocess
//!   invocation and returns raw bytes; the dispatcher handles persistence.

pub mod cli;
pub mod gemini;
pub mod http;
pub mod minimax;
pub mod session;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::PeerError;
use crate::registry::{BackendKind, BackendSpec, Registry, Transport};

/// Caller action on an image backend.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageAction {
    Generate,
    Edit,
}

/// Normalized input to the dispatcher. The MCP tool parameter struct
/// ([`crate::tools::ImageParams`]) is translated into this before dispatch.
#[derive(Debug, Clone)]
pub struct ImageRequest {
    pub action: ImageAction,
    pub backend: String,
    pub prompt: String,
    pub edit_prompt: Option<String>,
    pub input_path: Option<PathBuf>,
    pub output_path: Option<PathBuf>,
    pub aspect_ratio: Option<String>,
    pub model: Option<String>,
    pub reference_images: Vec<PathBuf>,
    pub n: u8,
    pub timeout_ms: Option<u64>,
}

/// Result surfaced to the caller. `paths` lists one or more PNG/JPEG
/// artifacts written under the project's generated-images directory.
#[derive(Debug, Clone, Serialize)]
pub struct ImageResponse {
    pub backend: String,
    pub model: String,
    pub aspect_ratio: Option<String>,
    pub paths: Vec<PathBuf>,
    pub session_id: String,
    pub meta_path: PathBuf,
    pub elapsed_ms: u64,
    /// Stderr tail from subprocess providers (codex). None for HTTP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

/// Implementation contract for each concrete image backend.
#[allow(async_fn_in_trait)]
pub trait ImageBackend {
    async fn generate(
        &self,
        spec: &BackendSpec,
        req: &ImageRequest,
        ctx: &session::ImageContext,
    ) -> Result<Vec<PathBuf>, PeerError>;

    async fn edit(
        &self,
        spec: &BackendSpec,
        req: &ImageRequest,
        ctx: &session::ImageContext,
    ) -> Result<Vec<PathBuf>, PeerError>;
}

/// Top-level dispatch. Looks up the backend, verifies it supports
/// `kind=image`, selects HTTP or CLI, and persists meta.
pub async fn dispatch_image(
    registry: &Registry,
    req: ImageRequest,
) -> Result<ImageResponse, PeerError> {
    let spec = registry
        .get(&req.backend)
        .ok_or_else(|| PeerError::BackendNotFound {
            backend: req.backend.clone(),
        })?;

    if !spec.supports(BackendKind::Image) {
        return Err(PeerError::UnsupportedKind {
            backend: spec.name.clone(),
            kind: "image".to_string(),
        });
    }

    // Allocate session context + pre-compute output paths.
    let ctx = session::ImageContext::new(&spec.name, &req)?;

    let start = std::time::Instant::now();

    // Snapshot request + backend before dispatch for forensic meta.
    session::write_meta_start(&ctx, spec, &req)?;

    let paths_result = match spec.transport {
        Transport::Http => dispatch_http(spec, &req, &ctx).await,
        Transport::Cli => {
            let cli_backend = cli::CodexImageBackend;
            match req.action {
                ImageAction::Generate => cli_backend.generate(spec, &req, &ctx).await,
                ImageAction::Edit => cli_backend.edit(spec, &req, &ctx).await,
            }
        }
    };

    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    match paths_result {
        Ok(paths) => {
            let model = req
                .model
                .clone()
                .or_else(|| spec.model.clone())
                .unwrap_or_else(|| spec.name.clone());
            let aspect_ratio = req
                .aspect_ratio
                .clone()
                .or_else(|| spec.aspect_ratio_default.clone());
            let stderr_tail = ctx.take_stderr_tail();
            let resp = ImageResponse {
                backend: spec.name.clone(),
                model,
                aspect_ratio,
                paths,
                session_id: ctx.session_id.clone(),
                meta_path: ctx.meta_path.clone(),
                elapsed_ms,
                stderr_tail,
            };
            session::write_meta_done(&ctx, &resp)?;
            Ok(resp)
        }
        Err(e) => {
            session::write_meta_failure(&ctx, spec, &req, &e, elapsed_ms);
            Err(e)
        }
    }
}

/// Route an HTTP-transport request to the matching provider based on
/// `spec.provider`.
async fn dispatch_http(
    spec: &BackendSpec,
    req: &ImageRequest,
    ctx: &session::ImageContext,
) -> Result<Vec<PathBuf>, PeerError> {
    let provider = spec.provider.as_deref().unwrap_or("").to_ascii_lowercase();
    match provider.as_str() {
        "gemini" => {
            let backend = gemini::GeminiImageBackend;
            match req.action {
                ImageAction::Generate => backend.generate(spec, req, ctx).await,
                ImageAction::Edit => backend.edit(spec, req, ctx).await,
            }
        }
        "minimax" => {
            let backend = minimax::MinimaxImageBackend;
            match req.action {
                ImageAction::Generate => backend.generate(spec, req, ctx).await,
                ImageAction::Edit => backend.edit(spec, req, ctx).await,
            }
        }
        "" => Err(PeerError::InvalidInput(format!(
            "backend {} has transport=http but no `provider` set",
            spec.name
        ))),
        other => Err(PeerError::InvalidInput(format!(
            "unknown image provider `{}` for backend {}",
            other, spec.name
        ))),
    }
}

/// Resolve the env-var-backed API key for an HTTP provider or error with a
/// helpful variant identifying which env is missing.
pub(crate) fn resolve_api_key(spec: &BackendSpec) -> Result<String, PeerError> {
    let env_var = spec.api_key_env.as_deref().ok_or_else(|| {
        PeerError::InvalidInput(format!(
            "backend {} is missing `api_key_env` in peer.toml",
            spec.name
        ))
    })?;
    std::env::var(env_var).map_err(|_| PeerError::MissingApiKey {
        backend: spec.name.clone(),
        env_var: env_var.to_string(),
    })
}

/// Clamp the per-request timeout the same way `ask` does, using the
/// backend default when unspecified.
pub(crate) fn resolve_timeout(spec: &BackendSpec, requested: Option<u64>) -> u64 {
    let ms = requested.unwrap_or(spec.timeout_ms_default);
    ms.clamp(10_000, 900_000)
}
