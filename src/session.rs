//! Per-invocation session directory with live-tailable stdout/stderr logs.
//!
//! Every `ask` dispatch creates
//! `~/.nott/peer/sessions/<timestamp>-<backend>/` with:
//!
//! - `stdout.log` — subprocess stdout, flushed on every chunk so
//!   `tail -f` shows output as the CLI emits it.
//! - `stderr.log` — same, for stderr.
//! - `meta.json` — backend, command, args, timing; updated at start and
//!   rewritten after completion with exit code + elapsed.
//!
//! A best-effort `latest` symlink (or pointer file on non-unix) in
//! `~/.nott/peer/sessions/` always references the most recent session so
//! `tail -f ~/.nott/peer/sessions/latest/stdout.log` works without knowing
//! the id.
//!
//! Session creation is cheap (one `mkdir`, one metadata write) and happens
//! unconditionally — independent of `save_raw`. The legacy `artifact_path`
//! in [`crate::dispatch::AskResponse`] now points at the session's
//! `stdout.log` when `save_raw=true`, unifying the two concepts.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::PeerError;

const SESSIONS_SUBDIR: &str = ".nott/peer/sessions";

/// Handle to an on-disk session directory.
///
/// Cheap to clone — holds just an id and an absolute path.
#[derive(Debug, Clone)]
pub struct PeerSession {
    /// Session id: `<epoch-secs>-<millis>-<backend>`. Sorts naturally.
    pub id: String,
    /// Absolute session directory. Guaranteed to exist after [`Self::new`].
    pub dir: PathBuf,
}

impl PeerSession {
    /// Allocate a fresh session directory for `backend`.
    ///
    /// Falls back to `$TMPDIR/peer-sessions/` when `$HOME` is unresolvable.
    pub fn new(backend: &str) -> Result<Self, PeerError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let id = format!(
            "{:010}-{:03}-{}",
            now.as_secs(),
            now.subsec_millis(),
            sanitize(backend)
        );

        let base = sessions_root();
        let dir = base.join(&id);
        std::fs::create_dir_all(&dir).map_err(PeerError::Io)?;

        // Best-effort `latest` pointer. Failure is non-fatal — the session
        // is still usable, users just lose the well-known path.
        let _ = update_latest_pointer(&base, &id);

        Ok(Self { id, dir })
    }

    /// Absolute path to the live stdout tee log.
    pub fn stdout_path(&self) -> PathBuf {
        self.dir.join("stdout.log")
    }

    /// Absolute path to the live stderr tee log.
    pub fn stderr_path(&self) -> PathBuf {
        self.dir.join("stderr.log")
    }

    /// Absolute path to the metadata manifest.
    pub fn meta_path(&self) -> PathBuf {
        self.dir.join("meta.json")
    }
}

/// Root directory containing all peer sessions.
///
/// Exposed so skills and external tooling (e.g. a `/peer-tail` helper) can
/// locate the latest run without replicating the layout constant.
pub fn sessions_root() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        return home.join(SESSIONS_SUBDIR);
    }
    std::env::temp_dir().join("peer-sessions")
}

/// Write (or overwrite) the session's `meta.json` with the supplied value.
/// Serialization failures are surfaced as [`std::io::Error`] for uniform
/// caller handling.
pub fn write_meta(session: &PeerSession, meta: &serde_json::Value) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(meta)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(session.meta_path(), text)
}

/// Replace characters that would escape the session directory or break
/// typical shells. Backend names are user-supplied via `peer.toml` so we
/// defensively normalize to `[A-Za-z0-9_.-]`.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Point `<base>/latest` at the newest session id.
///
/// Uses a symlink on unix (cheap, atomic-ish) and a pointer file elsewhere.
/// Any failure is discarded by the caller — this is UX sugar, not load-bearing.
fn update_latest_pointer(base: &Path, id: &str) -> std::io::Result<()> {
    let link = base.join("latest");
    // Remove any prior entry so we can rewrite cleanly.
    match std::fs::symlink_metadata(&link) {
        Ok(_) => {
            let _ = std::fs::remove_file(&link);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {}
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(id, &link)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&link, id)?;
    }
    Ok(())
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_safe_chars() {
        assert_eq!(sanitize("codex-1.2_beta"), "codex-1.2_beta");
    }

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize("../bad/path"), ".._bad_path");
        assert_eq!(sanitize("weird name!"), "weird_name_");
    }

    #[test]
    fn session_new_creates_dir_and_paths() {
        let session = PeerSession::new("unit-test").expect("create session");
        assert!(session.dir.is_dir(), "dir not created: {:?}", session.dir);
        // Paths are derived, not touched yet.
        assert_eq!(session.stdout_path().file_name().unwrap(), "stdout.log");
        assert_eq!(session.stderr_path().file_name().unwrap(), "stderr.log");
        assert_eq!(session.meta_path().file_name().unwrap(), "meta.json");
        // Clean up to not pollute the real sessions dir on dev machines.
        let _ = std::fs::remove_dir_all(&session.dir);
    }

    #[test]
    fn write_meta_roundtrips_json() {
        let session = PeerSession::new("meta-test").expect("create session");
        let meta = serde_json::json!({ "backend": "x", "ok": true });
        write_meta(&session, &meta).expect("write");
        let text = std::fs::read_to_string(session.meta_path()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["backend"], "x");
        assert_eq!(parsed["ok"], true);
        let _ = std::fs::remove_dir_all(&session.dir);
    }
}
