//! Video batch session — flat layout under `{project_root}/.nott/generated/videos/`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::PeerError;
use crate::project::generated_videos_dir;
use crate::registry::BackendSpec;

use super::{VideoRequest, VideoResponse};

pub struct VideoContext {
    pub session_id: String,
    pub base_dir: PathBuf,
    pub meta_path: PathBuf,
    pub output_path: PathBuf,
}

impl VideoContext {
    pub fn new(backend_name: &str, req: &VideoRequest) -> Result<Self, PeerError> {
        let base_dir = resolve_base_dir(req)?;
        std::fs::create_dir_all(&base_dir).map_err(PeerError::Io)?;
        let session_id = make_session_id(backend_name);
        let (output_path, meta_path) = plan_paths(&base_dir, &session_id, req);
        Ok(Self {
            session_id,
            base_dir,
            meta_path,
            output_path,
        })
    }
}

fn resolve_base_dir(req: &VideoRequest) -> Result<PathBuf, PeerError> {
    if let Some(explicit) = &req.output_path {
        if let Some(parent) = explicit.parent() {
            if !parent.as_os_str().is_empty() {
                return Ok(parent.to_path_buf());
            }
        }
    }
    generated_videos_dir().map_err(PeerError::Io)
}

fn plan_paths(base_dir: &Path, session_id: &str, req: &VideoRequest) -> (PathBuf, PathBuf) {
    let output_path = match &req.output_path {
        Some(explicit) => explicit.clone(),
        None => base_dir.join(format!("{session_id}.mp4")),
    };
    let meta_path = base_dir.join(format!("{session_id}.json"));
    (output_path, meta_path)
}

fn make_session_id(backend_name: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let id = uuid::Uuid::new_v4().simple().to_string();
    let short = &id[..8.min(id.len())];
    let clean: String = backend_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{secs:010}-{clean}-{short}")
}

pub(crate) fn write_meta_start(
    ctx: &VideoContext,
    spec: &BackendSpec,
    req: &VideoRequest,
) -> Result<(), PeerError> {
    let payload = serde_json::json!({
        "status": "running",
        "session_id": ctx.session_id,
        "backend": spec.name,
        "provider": spec.provider,
        "transport": format!("{:?}", spec.transport).to_lowercase(),
        "model": req.model.clone().or_else(|| spec.model.clone()),
        "prompt": req.prompt,
        "first_frame_image": req.first_frame_image.as_ref().map(|p| p.display().to_string()),
        "output_path_planned": ctx.output_path.display().to_string(),
        "started_at": epoch_secs(),
    });
    write_json(&ctx.meta_path, &payload)
}

pub(crate) fn write_meta_done(ctx: &VideoContext, resp: &VideoResponse) -> Result<(), PeerError> {
    let payload = serde_json::json!({
        "status": "done",
        "session_id": ctx.session_id,
        "backend": resp.backend,
        "model": resp.model,
        "paths": resp.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "task_id": resp.task_id,
        "elapsed_ms": resp.elapsed_ms,
        "finished_at": epoch_secs(),
    });
    write_json(&ctx.meta_path, &payload)
}

pub(crate) fn write_meta_failure(
    ctx: &VideoContext,
    spec: &BackendSpec,
    req: &VideoRequest,
    err: &PeerError,
    elapsed_ms: u64,
) {
    let payload = serde_json::json!({
        "status": "failed",
        "session_id": ctx.session_id,
        "backend": spec.name,
        "prompt": req.prompt,
        "error": err.to_string(),
        "error_exit_code": err.exit_code(),
        "elapsed_ms": elapsed_ms,
        "finished_at": epoch_secs(),
    });
    let _ = write_json(&ctx.meta_path, &payload);
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<(), PeerError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| PeerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    std::fs::write(path, text).map_err(PeerError::Io)
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_req() -> VideoRequest {
        VideoRequest {
            backend: "minimax-video".into(),
            prompt: "a running horse".into(),
            first_frame_image: None,
            output_path: None,
            model: None,
            timeout_ms: None,
        }
    }

    #[test]
    fn plan_paths_defaults_to_session_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let (video, meta) = plan_paths(tmp.path(), "sess-1", &sample_req());
        assert!(video.to_string_lossy().ends_with("sess-1.mp4"));
        assert!(meta.to_string_lossy().ends_with("sess-1.json"));
    }

    #[test]
    fn plan_paths_respects_explicit_output() {
        let tmp = tempfile::tempdir().unwrap();
        let mut req = sample_req();
        req.output_path = Some(tmp.path().join("teaser.mp4"));
        let (video, _) = plan_paths(tmp.path(), "ignored", &req);
        assert_eq!(video, tmp.path().join("teaser.mp4"));
    }
}
