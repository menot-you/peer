//! Video generation dispatcher.
//!
//! Mirrors the `image` module but handles ASYNC providers: create a task
//! on the provider → poll until it reports success → download the file.
//!
//! Today the only supported transport is HTTP, and the only supported
//! provider is MiniMax (`video-01` / `video-01-live2d` / `video-01-director`
//! / `MiniMax-Hailuo-02`). Google Veo and other video providers plug in
//! via a new module + `provider` discriminator.
//!
//! Artifacts land at `{project_root}/.nott/generated/videos/{session_id}.mp4`.

pub mod minimax;
pub mod session;
pub mod veo;

use std::path::PathBuf;

use serde::Serialize;

use crate::error::PeerError;
use crate::registry::{BackendKind, BackendSpec, Registry, Transport};

/// Normalized input to the dispatcher.
#[derive(Debug, Clone)]
pub struct VideoRequest {
    pub backend: String,
    pub prompt: String,
    /// Optional path to an image to use as the first frame (MiniMax) or
    /// conditioning image (Veo). Provider decides how to use it.
    pub first_frame_image: Option<PathBuf>,
    pub output_path: Option<PathBuf>,
    pub aspect_ratio: Option<String>,
    pub model: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoResponse {
    pub backend: String,
    pub model: String,
    pub paths: Vec<PathBuf>,
    pub session_id: String,
    pub meta_path: PathBuf,
    pub elapsed_ms: u64,
    /// Provider-assigned task id (useful for debugging).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

#[allow(async_fn_in_trait)]
pub trait VideoBackend {
    async fn generate(
        &self,
        spec: &BackendSpec,
        req: &VideoRequest,
        ctx: &session::VideoContext,
    ) -> Result<VideoOutcome, PeerError>;
}

/// Provider dispatch result combining saved paths with provider metadata.
pub struct VideoOutcome {
    pub paths: Vec<PathBuf>,
    pub task_id: Option<String>,
}

pub async fn dispatch_video(
    registry: &Registry,
    req: VideoRequest,
) -> Result<VideoResponse, PeerError> {
    let spec = registry
        .get(&req.backend)
        .ok_or_else(|| PeerError::BackendNotFound {
            backend: req.backend.clone(),
        })?;

    if !spec.supports(BackendKind::Video) {
        return Err(PeerError::UnsupportedKind {
            backend: spec.name.clone(),
            kind: "video".to_string(),
        });
    }

    let ctx = session::VideoContext::new(&spec.name, &req)?;

    let start = std::time::Instant::now();
    session::write_meta_start(&ctx, spec, &req)?;

    let outcome_result = match spec.transport {
        Transport::Http => dispatch_http(spec, &req, &ctx).await,
        Transport::Cli => Err(PeerError::InvalidInput(format!(
            "backend {} uses transport=cli but no CLI video provider is wired",
            spec.name
        ))),
    };

    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    match outcome_result {
        Ok(outcome) => {
            let model = req
                .model
                .clone()
                .or_else(|| spec.model.clone())
                .unwrap_or_else(|| spec.name.clone());
            let resp = VideoResponse {
                backend: spec.name.clone(),
                model,
                paths: outcome.paths,
                session_id: ctx.session_id.clone(),
                meta_path: ctx.meta_path.clone(),
                elapsed_ms,
                task_id: outcome.task_id,
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

async fn dispatch_http(
    spec: &BackendSpec,
    req: &VideoRequest,
    ctx: &session::VideoContext,
) -> Result<VideoOutcome, PeerError> {
    let provider = spec.provider.as_deref().unwrap_or("").to_ascii_lowercase();
    match provider.as_str() {
        "minimax" | "minimax-video" => {
            let backend = minimax::MinimaxVideoBackend;
            backend.generate(spec, req, ctx).await
        }
        "gemini" | "veo" => {
            let backend = veo::VeoBackend;
            backend.generate(spec, req, ctx).await
        }
        "" => Err(PeerError::InvalidInput(format!(
            "backend {} has transport=http but no `provider` set",
            spec.name
        ))),
        other => Err(PeerError::InvalidInput(format!(
            "unknown video provider `{other}` for backend {}",
            spec.name
        ))),
    }
}

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

pub(crate) fn resolve_timeout(spec: &BackendSpec, requested: Option<u64>) -> u64 {
    let ms = requested.unwrap_or(spec.timeout_ms_default);
    ms.clamp(10_000, 900_000)
}
