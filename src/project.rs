//! Project root resolver.
//!
//! The `image` tool defaults all artifacts to
//! `{project_root}/.nott/generated/images/`. Resolution order:
//!
//! 1. `$NOTT_PROJECT_ROOT` when set and pointing at an existing directory.
//! 2. `git rev-parse --show-toplevel` starting from the current working
//!    directory.
//! 3. The current working directory itself.
//!
//! Each step is a fallback — failures in step 1 or 2 are non-fatal; we log
//! and try the next.

use std::env;
use std::path::PathBuf;
use std::process::Stdio;

/// Return the resolved project root as an absolute path.
///
/// Never returns an error: the worst case is `PathBuf::from(".")`, which
/// works on every platform for subsequent joins.
pub fn resolve_project_root() -> PathBuf {
    if let Some(raw) = env::var_os("NOTT_PROJECT_ROOT") {
        let p = PathBuf::from(&raw);
        if p.is_dir() {
            return p;
        }
        tracing::debug!(
            target = "peer::project",
            path = %p.display(),
            "NOTT_PROJECT_ROOT set but not a directory — falling through"
        );
    }

    if let Some(root) = git_toplevel_sync() {
        return root;
    }

    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Shell out to `git rev-parse --show-toplevel`. Synchronous because the
/// image dispatcher only resolves the project root once per request and
/// we're already blocking on session-dir creation anyway.
fn git_toplevel_sync() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let p = PathBuf::from(trimmed);
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// Canonical subpath under `project_root` where image artifacts live.
pub const GENERATED_IMAGES_SUBDIR: &str = ".nott/generated/images";

/// Canonical subpath under `project_root` where video artifacts live.
pub const GENERATED_VIDEOS_SUBDIR: &str = ".nott/generated/videos";

/// Absolute path to the generated-images directory, created on demand.
pub fn generated_images_dir() -> std::io::Result<PathBuf> {
    let root = resolve_project_root().join(GENERATED_IMAGES_SUBDIR);
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

/// Absolute path to the generated-videos directory, created on demand.
pub fn generated_videos_dir() -> std::io::Result<PathBuf> {
    let root = resolve_project_root().join(GENERATED_VIDEOS_SUBDIR);
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_subdir_is_stable() {
        assert_eq!(GENERATED_IMAGES_SUBDIR, ".nott/generated/images");
    }

    #[test]
    fn generated_images_dir_creates() {
        // Smoke: uses whatever the cwd resolves to — just assert we can
        // mkdir-all successfully.
        let dir = generated_images_dir().expect("create generated_images_dir");
        assert!(dir.is_dir(), "expected {dir:?} to be a dir");
    }
}
