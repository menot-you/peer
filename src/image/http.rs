//! Shared HTTP client for image providers.
//!
//! All HTTP-transport image backends (Gemini, MiniMax) build `reqwest::Client`
//! with consistent defaults (timeout, TLS) via [`build_client`]. Providers
//! own the payload and endpoint; this module offers the transport primitives.

use std::time::Duration;

use reqwest::Client;

use crate::error::PeerError;

/// Build a reqwest client with the standard timeout + rustls + gzip setup.
pub fn build_client(timeout_ms: u64) -> Result<Client, PeerError> {
    Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .user_agent(concat!("peer-mcp/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| PeerError::HttpFailure {
            backend: "<client-build>".to_string(),
            message: e.to_string(),
        })
}

/// Classify a reqwest error into the typed [`PeerError::HttpFailure`] for the
/// given backend.
pub fn http_error(backend: &str, err: reqwest::Error) -> PeerError {
    PeerError::HttpFailure {
        backend: backend.to_string(),
        message: err.to_string(),
    }
}

/// Return a typed provider-payload error.
pub fn payload_error<M: Into<String>>(backend: &str, message: M) -> PeerError {
    PeerError::ProviderPayload {
        backend: backend.to_string(),
        message: message.into(),
    }
}

/// Decode base64 into raw bytes and write to disk. Returns the ACTUAL path
/// written — the extension is auto-adjusted to match the sniffed content
/// type. Callers get the corrected path back and should surface that in
/// the response so the extension never lies about the bytes.
pub async fn write_base64_png(
    backend: &str,
    b64: &str,
    path: &std::path::Path,
) -> Result<std::path::PathBuf, PeerError> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let bytes = STANDARD
        .decode(b64.trim())
        .map_err(|e| payload_error(backend, format!("base64 decode failed: {e}")))?;
    write_image_bytes(backend, &bytes, path).await
}

/// Write `bytes` to disk, renaming the extension if a magic-byte sniff
/// reveals a different content type than `path`'s extension. Returns the
/// actual path used.
///
/// Examples of providers that misreport mime: Nano Banana / Gemini often
/// announces `image/png` in `inlineData.mimeType` but returns raw JPEG
/// bytes. We want the file named `hero.jpg`, not `hero.png`, so downstream
/// tooling (uploaders, browsers, ffmpeg) treats the content honestly.
pub async fn write_image_bytes(
    backend: &str,
    bytes: &[u8],
    path: &std::path::Path,
) -> Result<std::path::PathBuf, PeerError> {
    if bytes.len() < 16 {
        return Err(payload_error(
            backend,
            format!(
                "decoded image is only {} bytes — too small to be valid",
                bytes.len()
            ),
        ));
    }
    let target = adjust_extension_for_bytes(path, bytes);
    tokio::fs::write(&target, bytes)
        .await
        .map_err(PeerError::Io)?;
    Ok(target)
}

/// Return `path` with its extension swapped to match the actual image
/// format detected via magic bytes. Leaves the path untouched when the
/// extension already matches or the format is unknown.
pub fn adjust_extension_for_bytes(path: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
    let Some(detected) = detect_image_format(bytes) else {
        return path.to_path_buf();
    };
    let current_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match current_ext.as_deref() {
        Some(ext) if format_matches_ext(detected, ext) => path.to_path_buf(),
        _ => path.with_extension(detected),
    }
}

/// Detect image format from the first bytes. Returns the canonical
/// extension string (`png` / `jpg` / `webp` / `gif`) or `None` if unknown.
pub fn detect_image_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 4 {
        return None;
    }
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif");
    }
    None
}

fn format_matches_ext(format: &str, ext: &str) -> bool {
    match (format, ext) {
        ("jpg", "jpg") | ("jpg", "jpeg") => true,
        (f, e) => f == e,
    }
}

/// Read a file and return its contents as base64 (used for Gemini inline
/// reference images).
pub async fn read_as_base64(path: &std::path::Path) -> Result<String, PeerError> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let bytes = tokio::fs::read(path).await.map_err(PeerError::Io)?;
    Ok(STANDARD.encode(bytes))
}

/// Guess a PNG/JPEG mime type from a path extension; falls back to
/// `image/png` because every image surface we target accepts PNG.
pub fn mime_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/png",
    }
}
