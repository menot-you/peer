//! Image batch session — flat layout under `{project_root}/.nott/generated/images/`.
//!
//! Each `image` call produces:
//! - One or more PNG/JPEG files: `{session_id}.png` (n=1) or
//!   `{session_id}-1.png`, `{session_id}-2.png`, … (n>1).
//! - One JSON sidecar: `{session_id}.json` with request + backend +
//!   response metadata.
//!
//! `session_id` has the shape `<epoch-secs>-<backend>-<uuid8>`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::PeerError;
use crate::project::generated_images_dir;
use crate::registry::BackendSpec;

use super::{ImageAction, ImageRequest, ImageResponse};

/// Derived context for one image dispatch. Immutable wrt identity (id + dir)
/// and carries a mutable stderr-tail buffer used by the CLI backend.
pub struct ImageContext {
    pub session_id: String,
    pub base_dir: PathBuf,
    pub meta_path: PathBuf,
    /// Pre-computed output paths (length == req.n). Caller-specified
    /// `output_path` wins for n=1.
    pub output_paths: Vec<PathBuf>,
    stderr_tail: Mutex<Option<String>>,
}

impl ImageContext {
    pub fn new(backend_name: &str, req: &ImageRequest) -> Result<Self, PeerError> {
        let base_dir = resolve_base_dir(req)?;
        std::fs::create_dir_all(&base_dir).map_err(PeerError::Io)?;

        let session_id = make_session_id(backend_name);
        let (output_paths, meta_path) = plan_paths(&base_dir, &session_id, req)?;

        Ok(Self {
            session_id,
            base_dir,
            meta_path,
            output_paths,
            stderr_tail: Mutex::new(None),
        })
    }

    pub fn set_stderr_tail(&self, tail: String) {
        if let Ok(mut guard) = self.stderr_tail.lock() {
            *guard = Some(tail);
        }
    }

    pub fn take_stderr_tail(&self) -> Option<String> {
        self.stderr_tail.lock().ok().and_then(|mut g| g.take())
    }
}

fn resolve_base_dir(req: &ImageRequest) -> Result<PathBuf, PeerError> {
    if let Some(explicit) = &req.output_path {
        // If the caller specified an absolute or relative output file,
        // anchor the batch dir at its parent (creating it if needed).
        if let Some(parent) = explicit.parent() {
            if !parent.as_os_str().is_empty() {
                return Ok(parent.to_path_buf());
            }
        }
    }
    generated_images_dir().map_err(PeerError::Io)
}

fn plan_paths(
    base_dir: &Path,
    session_id: &str,
    req: &ImageRequest,
) -> Result<(Vec<PathBuf>, PathBuf), PeerError> {
    let n = req.n.max(1) as usize;

    let (stem, extension) = match &req.output_path {
        Some(explicit) => {
            let stem = explicit
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| session_id.to_string());
            let ext = explicit
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_else(|| "png".to_string());
            (stem, ext)
        }
        None => (session_id.to_string(), "png".to_string()),
    };

    let output_paths: Vec<PathBuf> = if n == 1 {
        if let Some(explicit) = &req.output_path {
            vec![explicit.clone()]
        } else {
            vec![base_dir.join(format!("{stem}.{extension}"))]
        }
    } else {
        (1..=n)
            .map(|i| base_dir.join(format!("{stem}-{i}.{extension}")))
            .collect()
    };

    let meta_path = base_dir.join(format!("{session_id}.json"));
    Ok((output_paths, meta_path))
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
    ctx: &ImageContext,
    spec: &BackendSpec,
    req: &ImageRequest,
) -> Result<(), PeerError> {
    let payload = serde_json::json!({
        "status": "running",
        "session_id": ctx.session_id,
        "backend": spec.name,
        "provider": spec.provider,
        "transport": format!("{:?}", spec.transport).to_lowercase(),
        "model": req.model.clone().or_else(|| spec.model.clone()),
        "action": action_str(req.action),
        "prompt": req.prompt,
        "edit_prompt": req.edit_prompt,
        "input_path": req.input_path.as_ref().map(|p| p.display().to_string()),
        "aspect_ratio": req.aspect_ratio.clone().or_else(|| spec.aspect_ratio_default.clone()),
        "n": req.n,
        "reference_images": req.reference_images.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "output_paths_planned": ctx.output_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "started_at": epoch_secs(),
    });
    write_json(&ctx.meta_path, &payload)
}

pub(crate) fn write_meta_done(ctx: &ImageContext, resp: &ImageResponse) -> Result<(), PeerError> {
    let payload = serde_json::json!({
        "status": "done",
        "session_id": ctx.session_id,
        "backend": resp.backend,
        "model": resp.model,
        "aspect_ratio": resp.aspect_ratio,
        "paths": resp.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "elapsed_ms": resp.elapsed_ms,
        "finished_at": epoch_secs(),
    });
    write_json(&ctx.meta_path, &payload)
}

pub(crate) fn write_meta_failure(
    ctx: &ImageContext,
    spec: &BackendSpec,
    req: &ImageRequest,
    err: &PeerError,
    elapsed_ms: u64,
) {
    let payload = serde_json::json!({
        "status": "failed",
        "session_id": ctx.session_id,
        "backend": spec.name,
        "action": action_str(req.action),
        "error": err.to_string(),
        "error_exit_code": err.exit_code(),
        "elapsed_ms": elapsed_ms,
        "finished_at": epoch_secs(),
    });
    // Failure path is best-effort; swallow the io error.
    let _ = write_json(&ctx.meta_path, &payload);
}

fn action_str(a: ImageAction) -> &'static str {
    match a {
        ImageAction::Generate => "generate",
        ImageAction::Edit => "edit",
    }
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

    fn sample_req(n: u8) -> ImageRequest {
        ImageRequest {
            action: ImageAction::Generate,
            backend: "nanobanana".into(),
            prompt: "a red cube".into(),
            edit_prompt: None,
            input_path: None,
            output_path: None,
            aspect_ratio: None,
            model: None,
            reference_images: vec![],
            n,
            timeout_ms: None,
        }
    }

    #[test]
    fn plan_paths_single_defaults_to_session_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, meta) = plan_paths(tmp.path(), "sess-123", &sample_req(1)).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].to_string_lossy().ends_with("sess-123.png"));
        assert!(meta.to_string_lossy().ends_with("sess-123.json"));
    }

    #[test]
    fn plan_paths_multi_appends_index() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, _) = plan_paths(tmp.path(), "sess-42", &sample_req(3)).unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths[0].to_string_lossy().ends_with("sess-42-1.png"));
        assert!(paths[2].to_string_lossy().ends_with("sess-42-3.png"));
    }

    #[test]
    fn plan_paths_respects_explicit_output_for_n1() {
        let tmp = tempfile::tempdir().unwrap();
        let mut req = sample_req(1);
        req.output_path = Some(tmp.path().join("custom.jpg"));
        let (paths, _) = plan_paths(tmp.path(), "ignored", &req).unwrap();
        assert_eq!(paths, vec![tmp.path().join("custom.jpg")]);
    }

    #[test]
    fn plan_paths_uses_stem_from_explicit_for_n_greater_1() {
        let tmp = tempfile::tempdir().unwrap();
        let mut req = sample_req(2);
        req.output_path = Some(tmp.path().join("brand-hero.png"));
        let (paths, _) = plan_paths(tmp.path(), "sess", &req).unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths[0].to_string_lossy().ends_with("brand-hero-1.png"));
        assert!(paths[1].to_string_lossy().ends_with("brand-hero-2.png"));
    }

    #[test]
    fn session_id_is_filesystem_safe() {
        let id = make_session_id("codex/bad name");
        assert!(id.contains("codex_bad_name"));
        assert!(!id.contains('/'));
    }
}
