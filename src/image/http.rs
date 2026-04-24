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

/// Decode base64 into raw bytes and write to disk at `path`. Returns the
/// number of bytes written so callers can include it in meta.
pub async fn write_base64_png(
    backend: &str,
    b64: &str,
    path: &std::path::Path,
) -> Result<usize, PeerError> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let bytes = STANDARD
        .decode(b64.trim())
        .map_err(|e| payload_error(backend, format!("base64 decode failed: {e}")))?;
    if bytes.len() < 16 {
        return Err(payload_error(
            backend,
            format!(
                "decoded image is only {} bytes — too small to be valid",
                bytes.len()
            ),
        ));
    }
    tokio::fs::write(path, &bytes)
        .await
        .map_err(PeerError::Io)?;
    Ok(bytes.len())
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
