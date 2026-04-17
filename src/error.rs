//! Typed error enum with stable exit codes.
//!
//! Callers (skills, agents) distinguish failure modes via the numeric code
//! returned in [`PeerError::exit_code`]. The mapping is frozen so callers can
//! branch on it without parsing strings.

use std::io;

use thiserror::Error;

/// Error variants produced by the peer MCP.
///
/// Every variant has a stable numeric exit code exposed via
/// [`PeerError::exit_code`]. This code appears in the tool's output and is
/// load-bearing for caller logic (e.g., `/selfreview` branches on "auth"
/// vs "timeout" vs "binary missing").
#[derive(Debug, Error)]
pub enum PeerError {
    /// Backend binary missing on `$PATH`.
    #[error("binary not found on PATH: {command}")]
    BinaryNotFound { command: String },

    /// Subprocess appears to have failed authentication (exit 401/403 or
    /// stderr mentions "login"). Hint stored when the backend spec provides one.
    #[error("auth failure for {backend}: {hint}")]
    AuthFailure { backend: String, hint: String },

    /// Subprocess exceeded the configured timeout.
    #[error("timeout after {elapsed_ms}ms for backend={backend}")]
    Timeout { backend: String, elapsed_ms: u64 },

    /// Subprocess produced non-UTF8 stdout.
    #[error("parse failure: stdout was not valid UTF-8 for backend={backend}")]
    ParseFailure { backend: String },

    /// Requested backend is not in the loaded registry.
    #[error("backend not found in registry: {backend}")]
    BackendNotFound { backend: String },

    /// Registry TOML failed to load or parse at boot.
    #[error("registry load failed: {0}")]
    RegistryLoad(String),

    /// Generic I/O error wrapping tokio/process internals.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// Invalid input supplied by the caller (timeout out of range, etc).
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl PeerError {
    /// Returns the stable numeric exit code for this error variant.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::BinaryNotFound { .. } => 1,
            Self::AuthFailure { .. } => 2,
            Self::Timeout { .. } => 3,
            Self::ParseFailure { .. } => 4,
            Self::BackendNotFound { .. } => 5,
            Self::RegistryLoad(_) => 6,
            Self::Io(_) => 1, // treat IO as binary-unreachable
            Self::InvalidInput(_) => 7,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(
            PeerError::BinaryNotFound {
                command: "x".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(
            PeerError::AuthFailure {
                backend: "x".into(),
                hint: "y".into(),
            }
            .exit_code(),
            2
        );
        assert_eq!(
            PeerError::Timeout {
                backend: "x".into(),
                elapsed_ms: 100,
            }
            .exit_code(),
            3
        );
        assert_eq!(
            PeerError::ParseFailure {
                backend: "x".into()
            }
            .exit_code(),
            4
        );
        assert_eq!(
            PeerError::BackendNotFound {
                backend: "x".into()
            }
            .exit_code(),
            5
        );
        assert_eq!(PeerError::RegistryLoad("x".into()).exit_code(), 6);
        assert_eq!(PeerError::InvalidInput("x".into()).exit_code(), 7);
    }
}
