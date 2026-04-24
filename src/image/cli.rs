//! CLI subprocess image backend (codex via `$imagegen` convention).
//!
//! Strategy:
//! 1. Expand the spec's `image_template` (or `image_edit_template`) with
//!    `{prompt}` / `{edit_prompt}` / `{output_path}`.
//! 2. For edits, splice `image_edit_prefix_args` (e.g. `["-i", "{input_path}"]`)
//!    into argv before the prompt.
//! 3. Spawn the subprocess, feed the expanded prompt via stdin (matching
//!    `spec.stdin`), wait with timeout.
//! 4. Verify that `output_path` exists on disk + looks like an image by
//!    magic-byte sniff.
//! 5. If missing, retry ONCE with a reinforced prompt; if still missing,
//!    return [`PeerError::ImageNotProduced`] with stderr tail.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

use crate::dispatch::expand_args;
use crate::error::PeerError;
use crate::registry::BackendSpec;

use super::{resolve_timeout, session::ImageContext, ImageBackend, ImageRequest};

pub struct CodexImageBackend;

/// Maximum stderr captured (bytes) for the tail surfaced to the caller.
const STDERR_TAIL_BYTES: usize = 2048;

impl ImageBackend for CodexImageBackend {
    async fn generate(
        &self,
        spec: &BackendSpec,
        req: &ImageRequest,
        ctx: &ImageContext,
    ) -> Result<Vec<PathBuf>, PeerError> {
        if req.n > 1 {
            return Err(PeerError::InvalidInput(format!(
                "codex CLI backend does not support n>1 (got {}); call multiple times",
                req.n
            )));
        }
        let target = ctx
            .output_paths
            .first()
            .cloned()
            .ok_or_else(|| PeerError::InvalidInput("no planned output_path".into()))?;

        let template = spec.image_template.as_deref().ok_or_else(|| {
            PeerError::InvalidInput(format!(
                "backend {} missing `image_template` for generate",
                spec.name
            ))
        })?;
        let prompt = expand_template(
            template,
            &[
                ("prompt", req.prompt.as_str()),
                ("output_path", &target.to_string_lossy()),
            ],
        );

        run_codex(spec, req, ctx, &target, &prompt, &[]).await?;
        Ok(vec![target])
    }

    async fn edit(
        &self,
        spec: &BackendSpec,
        req: &ImageRequest,
        ctx: &ImageContext,
    ) -> Result<Vec<PathBuf>, PeerError> {
        if req.n > 1 {
            return Err(PeerError::InvalidInput(format!(
                "codex CLI edit does not support n>1 (got {})",
                req.n
            )));
        }
        let input = req
            .input_path
            .as_ref()
            .ok_or_else(|| PeerError::InvalidInput("edit requires `input_path`".into()))?;
        if !input.is_file() {
            return Err(PeerError::InvalidInput(format!(
                "edit input_path does not exist: {}",
                input.display()
            )));
        }
        let edit_prompt = req
            .edit_prompt
            .as_deref()
            .or(Some(req.prompt.as_str()))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| PeerError::InvalidInput("edit requires `edit_prompt`".into()))?;
        let target = ctx
            .output_paths
            .first()
            .cloned()
            .ok_or_else(|| PeerError::InvalidInput("no planned output_path".into()))?;

        let template = spec
            .image_edit_template
            .as_deref()
            .or(spec.image_template.as_deref())
            .ok_or_else(|| {
                PeerError::InvalidInput(format!(
                    "backend {} missing `image_edit_template`/`image_template` for edit",
                    spec.name
                ))
            })?;
        let prompt = expand_template(
            template,
            &[
                ("edit_prompt", edit_prompt),
                ("prompt", edit_prompt),
                ("input_path", &input.to_string_lossy()),
                ("output_path", &target.to_string_lossy()),
            ],
        );

        let prefix_args_raw = spec.image_edit_prefix_args.clone().unwrap_or_default();
        let prefix_args: Vec<String> = prefix_args_raw
            .into_iter()
            .map(|a| expand_template(&a, &[("input_path", &input.to_string_lossy())]))
            .collect();

        run_codex(spec, req, ctx, &target, &prompt, &prefix_args).await?;
        Ok(vec![target])
    }
}

/// Run the codex subprocess with optional one-shot retry when the artifact
/// does not appear on disk.
async fn run_codex(
    spec: &BackendSpec,
    req: &ImageRequest,
    ctx: &ImageContext,
    target: &Path,
    prompt: &str,
    extra_args: &[String],
) -> Result<(), PeerError> {
    // First attempt — use the template-expanded prompt verbatim.
    let first = spawn_once(spec, req, prompt, extra_args).await?;
    if image_is_valid(target).await {
        ctx.set_stderr_tail(tail_stderr(&first.stderr));
        return Ok(());
    }

    tracing::warn!(
        target = "peer::image::cli",
        backend = %spec.name,
        path = %target.display(),
        "codex finished but target missing/invalid — retry with reinforced prompt"
    );

    // Retry with a hardened prompt that reiterates the save requirement.
    let hardened = format!(
        "{}\n\nIMPORTANT: you MUST save the resulting image at exactly `{}`. Do not write to any other path. Verify the file exists before exiting.",
        prompt,
        target.display()
    );
    let second = spawn_once(spec, req, &hardened, extra_args).await?;
    if image_is_valid(target).await {
        ctx.set_stderr_tail(tail_stderr(&second.stderr));
        return Ok(());
    }

    ctx.set_stderr_tail(tail_stderr(&second.stderr));
    Err(PeerError::ImageNotProduced {
        backend: spec.name.clone(),
        path: target.display().to_string(),
        reason: format!(
            "codex exited with code {} but no valid image at target after 2 attempts",
            second.exit_code
        ),
    })
}

struct SubprocResult {
    exit_code: i32,
    stderr: String,
}

async fn spawn_once(
    spec: &BackendSpec,
    req: &ImageRequest,
    prompt: &str,
    extra_args: &[String],
) -> Result<SubprocResult, PeerError> {
    // Reuse ask-dispatch's placeholder expansion so `{prompt}`, `{env:...}`,
    // and `{extra}` in spec.args behave consistently across tools. On image
    // dispatch the "prompt" is the template-expanded image prompt.
    let mut argv: Vec<String> = expand_args(&spec.args, prompt, &[]);
    if let Some(image_extras) = &spec.image_extra_args {
        argv.extend(image_extras.iter().cloned());
    }
    argv.extend(extra_args.iter().cloned());

    let timeout_ms = resolve_timeout(spec, req.timeout_ms);

    let mut cmd = Command::new(&spec.command);
    cmd.args(&argv)
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

    tracing::info!(
        target = "peer::image::cli",
        backend = %spec.name,
        command = %spec.command,
        args = ?argv,
        stdin = spec.stdin,
        prompt_len = prompt.len(),
        "spawning codex for image"
    );

    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => PeerError::BinaryNotFound {
            command: spec.command.clone(),
        },
        _ => PeerError::Io(e),
    })?;

    // If using stdin, pipe the expanded prompt in.
    if spec.stdin {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .map_err(PeerError::Io)?;
            drop(stdin);
        }
    }

    // Drain stdout/stderr concurrently so the child can make progress.
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_fut = async {
        if let Some(mut s) = stdout_pipe {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).into_owned()
        } else {
            String::new()
        }
    };
    let stderr_fut = async {
        if let Some(mut s) = stderr_pipe {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).into_owned()
        } else {
            String::new()
        }
    };

    let wait = child.wait();
    let result = timeout(Duration::from_millis(timeout_ms), wait).await;

    match result {
        Ok(Ok(status)) => {
            let (_stdout, stderr) = tokio::join!(stdout_fut, stderr_fut);
            Ok(SubprocResult {
                exit_code: status.code().unwrap_or(-1),
                stderr,
            })
        }
        Ok(Err(e)) => Err(PeerError::Io(e)),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(PeerError::Timeout {
                backend: spec.name.clone(),
                elapsed_ms: timeout_ms,
            })
        }
    }
}

/// True when `path` exists, is non-empty, and starts with a known image
/// magic byte sequence (PNG, JPEG, WebP, GIF).
pub(crate) async fn image_is_valid(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let mut buf = [0u8; 12];
    let n = match tokio::fs::File::open(path).await {
        Ok(mut f) => match f.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    if n < 8 {
        return false;
    }
    // PNG
    if buf.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return true;
    }
    // JPEG (FF D8 FF)
    if buf.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return true;
    }
    // WebP: RIFF....WEBP
    if &buf[0..4] == b"RIFF" && n >= 12 && &buf[8..12] == b"WEBP" {
        return true;
    }
    // GIF
    if buf.starts_with(b"GIF87a") || buf.starts_with(b"GIF89a") {
        return true;
    }
    false
}

/// Expand `{key}` placeholders with values from pairs; leaves unrecognized
/// placeholders untouched.
pub(crate) fn expand_template(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in pairs {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

fn tail_stderr(all: &str) -> String {
    if all.len() <= STDERR_TAIL_BYTES {
        return all.to_string();
    }
    let start = all.len() - STDERR_TAIL_BYTES;
    let mut idx = start;
    while idx < all.len() && !all.is_char_boundary(idx) {
        idx += 1;
    }
    all[idx..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_replaces_known_keys() {
        let t = "$imagegen {prompt}. Salve em {output_path}";
        let out = expand_template(t, &[("prompt", "red cube"), ("output_path", "/tmp/x.png")]);
        assert_eq!(out, "$imagegen red cube. Salve em /tmp/x.png");
    }

    #[test]
    fn expand_leaves_unknown_keys() {
        let out = expand_template("{foo} {bar}", &[("foo", "A")]);
        assert_eq!(out, "A {bar}");
    }

    #[tokio::test]
    async fn image_is_valid_detects_png_magic() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.png");
        let bytes = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4, 5,
        ];
        tokio::fs::write(&p, bytes).await.unwrap();
        assert!(image_is_valid(&p).await);
    }

    #[tokio::test]
    async fn image_is_valid_rejects_text_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.png");
        tokio::fs::write(&p, b"not an image").await.unwrap();
        assert!(!image_is_valid(&p).await);
    }

    #[tokio::test]
    async fn image_is_valid_rejects_missing() {
        assert!(!image_is_valid(Path::new("/tmp/nonexistent-peer-test.png")).await);
    }
}
