//! Subprocess dispatch: placeholder expansion → spawn → timeout → verdict parse.

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;

/// Verdict regex — compiled once at static init. The pattern is a literal
/// validated by tests (`parse_verdict_*`), so panicking at init is acceptable
/// and unreachable in practice.
#[allow(clippy::expect_used)]
static VERDICT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bverdict[:\s]+(lgtm|block|conditional)\b")
        .expect("VERDICT_RE literal must compile")
});
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::Duration;

use crate::error::PeerError;
use crate::registry::Registry;
use crate::session::{write_meta, PeerSession};

/// Caller input to `ask`.
#[derive(Debug, Clone, Deserialize)]
pub struct AskRequest {
    pub backend: String,
    pub prompt: String,
    pub timeout_ms: Option<u64>,
    #[serde(default = "default_save_raw")]
    pub save_raw: bool,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub extra_env: HashMap<String, String>,
}

fn default_save_raw() -> bool {
    true
}

/// Normalized verdict extracted from the subprocess stdout.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verdict {
    Lgtm,
    Block,
    Conditional,
    Unknown,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Lgtm => "LGTM",
            Self::Block => "BLOCK",
            Self::Conditional => "CONDITIONAL",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// Result returned to the caller.
#[derive(Debug, Clone, Serialize)]
pub struct AskResponse {
    pub backend: String,
    pub verdict: Verdict,
    pub raw: String,
    pub elapsed_ms: u64,
    pub exit_code: i32,
    pub stderr: String,
    /// Canonical pointer to the captured stdout artifact.
    ///
    /// When `save_raw=true`, this is the session's `stdout.log` — the SAME
    /// file that was streamed live during execution. Callers tailing the
    /// session can keep the tail open and inspect the full artifact here
    /// after completion.
    pub artifact_path: Option<PathBuf>,
    /// Session id for the dispatch (e.g. `0001745260834-042-codex`).
    /// Always present; exposed so callers can reconstruct
    /// `<sessions_root>/<id>/` paths without string math.
    pub session_id: String,
    /// Absolute session directory containing `stdout.log`, `stderr.log`,
    /// and `meta.json`. Callers can `tail -f <dir>/stdout.log` during
    /// execution; after completion the files are closed but persist.
    pub session_dir: PathBuf,
}

/// Clamp range enforced on user-supplied timeouts.
const MIN_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 900_000;
/// Keep at most 2KB of stderr in the response (tail).
const STDERR_TAIL_BYTES: usize = 2048;
/// Scan only the last N lines of stdout for the verdict.
const VERDICT_TAIL_LINES: usize = 200;

/// Dispatch a single `ask` call.
///
/// Allocates a session directory BEFORE spawning the backend so every
/// dispatch is traceable via `~/.nott/peer/sessions/latest/`. Stdout and
/// stderr are tee'd into `stdout.log` / `stderr.log` in real time (flushed
/// per chunk) so external `tail -f` observes progress as the CLI runs.
/// Metadata is written at start and overwritten at completion with
/// elapsed + exit_code.
pub async fn dispatch(registry: &Registry, req: AskRequest) -> Result<AskResponse, PeerError> {
    let spec = registry
        .get(&req.backend)
        .ok_or_else(|| PeerError::BackendNotFound {
            backend: req.backend.clone(),
        })?;

    let timeout = clamp_timeout(req.timeout_ms.unwrap_or(spec.timeout_ms_default))?;
    let expanded_args = expand_args(&spec.args, &req.prompt, &req.extra_args);

    // Allocate session dir early — independent of save_raw, so every
    // invocation is tailable via `~/.nott/peer/sessions/latest/`.
    let session = PeerSession::new(&spec.name)?;
    let started_at = epoch_secs();
    let _ = write_meta(
        &session,
        &serde_json::json!({
            "backend": spec.name,
            "command": spec.command,
            "args": expanded_args,
            "stdin": spec.stdin,
            "timeout_ms": timeout,
            "started_at": started_at,
            "status": "running",
        }),
    );

    tracing::info!(
        target = "peer::dispatch",
        backend = %spec.name,
        session_id = %session.id,
        session_dir = %session.dir.display(),
        command = %spec.command,
        timeout_ms = timeout,
        stdin = spec.stdin,
        "spawning backend"
    );

    let start = Instant::now();
    let mut cmd = Command::new(&spec.command);
    cmd.args(&expanded_args)
        .stdin(if spec.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    for (k, v) in &req.extra_env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        ErrorKind::NotFound => PeerError::BinaryNotFound {
            command: spec.command.clone(),
        },
        _ => PeerError::Io(e),
    })?;

    if spec.stdin {
        if let Some(mut stdin_handle) = child.stdin.take() {
            stdin_handle
                .write_all(req.prompt.as_bytes())
                .await
                .map_err(PeerError::Io)?;
            drop(stdin_handle);
        }
    }

    // Take the pipes and tee them to disk + memory concurrently. We keep
    // the in-memory buffers for the final response; the disk copy exists
    // for live `tail -f` observers and post-mortem inspection.
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| PeerError::Io(io::Error::other("stdout not piped")))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| PeerError::Io(io::Error::other("stderr not piped")))?;

    let stdout_path = session.stdout_path();
    let stderr_path = session.stderr_path();
    let stdout_task = tokio::spawn(tee_stream(stdout_pipe, stdout_path));
    let stderr_task = tokio::spawn(tee_stream(stderr_pipe, stderr_path));

    let wait = child.wait();
    let status = match tokio::time::timeout(Duration::from_millis(timeout), wait).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(PeerError::Io(e)),
        Err(_) => {
            // Kill the child so the tee tasks can drain + close cleanly.
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = write_meta(
                &session,
                &serde_json::json!({
                    "backend": spec.name,
                    "command": spec.command,
                    "args": expanded_args,
                    "stdin": spec.stdin,
                    "timeout_ms": timeout,
                    "started_at": started_at,
                    "status": "timeout",
                }),
            );
            return Err(PeerError::Timeout {
                backend: spec.name.clone(),
                elapsed_ms: timeout,
            });
        }
    };

    let stdout_bytes = join_tee(stdout_task).await?;
    let stderr_bytes = join_tee(stderr_task).await?;

    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let raw = String::from_utf8(stdout_bytes).map_err(|_| PeerError::ParseFailure {
        backend: spec.name.clone(),
    })?;
    let stderr_all = String::from_utf8_lossy(&stderr_bytes).into_owned();
    let exit_code = status.code().unwrap_or(-1);

    let _ = write_meta(
        &session,
        &serde_json::json!({
            "backend": spec.name,
            "command": spec.command,
            "args": expanded_args,
            "stdin": spec.stdin,
            "timeout_ms": timeout,
            "started_at": started_at,
            "finished_at": epoch_secs(),
            "elapsed_ms": elapsed_ms,
            "exit_code": exit_code,
            "status": "done",
        }),
    );

    // Detect auth failure patterns BEFORE returning — caller wants the typed
    // error, not a generic exit=1.
    if is_auth_failure(exit_code, &stderr_all) {
        let hint = spec
            .auth_hint
            .clone()
            .unwrap_or_else(|| format!("{} appears to require re-authentication", spec.name));
        return Err(PeerError::AuthFailure {
            backend: spec.name.clone(),
            hint,
        });
    }

    let verdict = parse_verdict(&raw);
    let artifact_path = if req.save_raw {
        Some(session.stdout_path())
    } else {
        None
    };

    Ok(AskResponse {
        backend: spec.name.clone(),
        verdict,
        raw,
        elapsed_ms,
        exit_code,
        stderr: tail_stderr(&stderr_all),
        artifact_path,
        session_id: session.id.clone(),
        session_dir: session.dir.clone(),
    })
}

fn clamp_timeout(ms: u64) -> Result<u64, PeerError> {
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&ms) {
        return Err(PeerError::InvalidInput(format!(
            "timeout_ms must be in [{MIN_TIMEOUT_MS},{MAX_TIMEOUT_MS}], got {ms}"
        )));
    }
    Ok(ms)
}

fn tail_stderr(all: &str) -> String {
    if all.len() <= STDERR_TAIL_BYTES {
        return all.to_string();
    }
    // Find a safe utf-8 char boundary at or after the tail start.
    let start = all.len() - STDERR_TAIL_BYTES;
    let mut idx = start;
    while idx < all.len() && !all.is_char_boundary(idx) {
        idx += 1;
    }
    all[idx..].to_string()
}

fn is_auth_failure(exit_code: i32, stderr: &str) -> bool {
    if matches!(exit_code, 401 | 403) {
        return true;
    }
    let lower = stderr.to_ascii_lowercase();
    lower.contains("please login")
        || lower.contains("please log in")
        || lower.contains("not authenticated")
        || lower.contains("authentication failed")
        || lower.contains("401 unauthorized")
        || lower.contains("403 forbidden")
}

/// Expand placeholders in the backend's args:
/// - `{prompt}` → the caller prompt
/// - `{env:VAR:default}` → environment lookup with fallback
/// - `{extra}` → splat of `extra_args` at this position; if not present,
///   `extra_args` are appended at the end.
pub fn expand_args(args: &[String], prompt: &str, extra_args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + extra_args.len());
    let mut splat_seen = false;
    for arg in args {
        if arg == "{extra}" {
            out.extend(extra_args.iter().cloned());
            splat_seen = true;
            continue;
        }
        out.push(expand_single(arg, prompt));
    }
    if !splat_seen {
        out.extend(extra_args.iter().cloned());
    }
    out
}

fn expand_single(arg: &str, prompt: &str) -> String {
    let mut s = arg.replace("{prompt}", prompt);
    // Minimal `{env:VAR:default}` resolver. One placeholder per arg is
    // the common case; we loop to handle compound forms defensively.
    while let Some(start) = s.find("{env:") {
        let Some(end_rel) = s[start..].find('}') else {
            break;
        };
        let end = start + end_rel;
        let spec = &s[start + 5..end]; // strip `{env:` and `}`
        let (name, default) = match spec.split_once(':') {
            Some((n, d)) => (n, d.to_string()),
            None => (spec, String::new()),
        };
        let value = std::env::var(name).unwrap_or(default);
        s.replace_range(start..=end, &value);
    }
    s
}

/// Parse verdict from the last `VERDICT_TAIL_LINES` lines of stdout.
pub fn parse_verdict(raw: &str) -> Verdict {
    let tail: String = raw
        .lines()
        .rev()
        .take(VERDICT_TAIL_LINES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    // Case-insensitive: VERDICT or Verdict, followed by : or whitespace,
    // then LGTM|BLOCK|CONDITIONAL. Regex is a module-level LazyLock.
    if let Some(caps) = VERDICT_RE.captures(&tail) {
        match caps[1].to_ascii_uppercase().as_str() {
            "LGTM" => Verdict::Lgtm,
            "BLOCK" => Verdict::Block,
            "CONDITIONAL" => Verdict::Conditional,
            _ => Verdict::Unknown,
        }
    } else {
        Verdict::Unknown
    }
}

/// Read `reader` chunk-by-chunk, appending each chunk to `path` (flushed
/// immediately) AND to an in-memory buffer that is returned to the caller.
///
/// Flushing on every chunk is intentional: without it, `tail -f` on the
/// session log sees nothing until the subprocess exits. With it, observers
/// see output as the backend emits it.
async fn tee_stream<R>(mut reader: R, path: PathBuf) -> io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut file = tokio::fs::File::create(&path).await?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        file.write_all(&chunk[..n]).await?;
        file.flush().await?;
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

/// Join a tee task, flattening `JoinError` + `io::Error` into [`PeerError`].
async fn join_tee(
    handle: tokio::task::JoinHandle<io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, PeerError> {
    match handle.await {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(e)) => Err(PeerError::Io(e)),
        Err(join_err) => Err(PeerError::Io(io::Error::other(join_err))),
    }
}

/// Seconds since the Unix epoch; falls back to 0 on a broken clock.
fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_prompt_replaces_placeholder() {
        let args = vec!["-p".into(), "{prompt}".into()];
        let out = expand_args(&args, "hello world", &[]);
        assert_eq!(out, vec!["-p", "hello world"]);
    }

    #[test]
    fn expand_env_with_default() {
        // SAFETY: test-only single-threaded env manipulation.
        // Ensure the var is unset so we exercise the default path.
        std::env::remove_var("PEER_TEST_FALLBACK_VAR");
        let args = vec!["{env:PEER_TEST_FALLBACK_VAR:hello}".into()];
        let out = expand_args(&args, "", &[]);
        assert_eq!(out, vec!["hello"]);
    }

    #[test]
    fn expand_extra_splat_at_position() {
        let args = vec!["a".into(), "{extra}".into(), "b".into()];
        let out = expand_args(&args, "", &["x".into(), "y".into()]);
        assert_eq!(out, vec!["a", "x", "y", "b"]);
    }

    #[test]
    fn expand_extra_appended_when_no_splat() {
        let args = vec!["a".into(), "b".into()];
        let out = expand_args(&args, "", &["x".into()]);
        assert_eq!(out, vec!["a", "b", "x"]);
    }

    #[test]
    fn verdict_parse_lgtm_upper() {
        let raw = "some output\nVERDICT: LGTM\nmore";
        assert_eq!(parse_verdict(raw), Verdict::Lgtm);
    }

    #[test]
    fn verdict_parse_block_lower() {
        let raw = "verdict: block";
        assert_eq!(parse_verdict(raw), Verdict::Block);
    }

    #[test]
    fn verdict_parse_conditional_whitespace() {
        let raw = "Verdict     Conditional";
        assert_eq!(parse_verdict(raw), Verdict::Conditional);
    }

    #[test]
    fn verdict_parse_unknown_when_missing() {
        let raw = "no pattern here";
        assert_eq!(parse_verdict(raw), Verdict::Unknown);
    }

    #[test]
    fn clamp_rejects_below_min() {
        assert!(clamp_timeout(5_000).is_err());
    }

    #[test]
    fn clamp_rejects_above_max() {
        assert!(clamp_timeout(2_000_000).is_err());
    }

    #[test]
    fn clamp_accepts_in_range() {
        assert_eq!(clamp_timeout(60_000).unwrap(), 60_000);
    }

    #[test]
    fn auth_detect_http_401_code() {
        assert!(is_auth_failure(401, ""));
    }

    #[test]
    fn auth_detect_login_phrase() {
        assert!(is_auth_failure(1, "Error: please login first"));
    }

    #[test]
    fn auth_ignore_generic_stderr() {
        assert!(!is_auth_failure(1, "something failed"));
    }
}
