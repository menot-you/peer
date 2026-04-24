//! MCP tool surface: `ask`, `image`, and `list_backends`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use rmcp::{handler::server::wrapper::Parameters, tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;

use crate::dispatch::{dispatch, AskRequest};
use crate::error::PeerError;
use crate::image::{dispatch_image, ImageAction, ImageRequest};
use crate::registry::Registry;
use crate::video::{dispatch_video, VideoRequest};

/// MCP server wrapping a resolved [`Registry`].
#[derive(Clone)]
pub struct PeerMcpServer {
    registry: Arc<Registry>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl PeerMcpServer {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            tool_router: Self::tool_router(),
        }
    }
}

// -------------------------------------------------------------------------
// Parameter structs
// -------------------------------------------------------------------------

/// Input for the `ask` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskParams {
    /// Lookup key in the backend registry (`codex`, `gemini`, `minimax`, `claude`, or any name
    /// added to `~/.nott/peer.toml`).
    pub backend: String,
    /// Prompt content — routed to stdin or substituted via `{prompt}` placeholder.
    pub prompt: String,
    /// Optional timeout override in milliseconds. Clamped to [10_000, 900_000].
    /// Falls back to the backend's `timeout_ms_default` when omitted.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Persist raw stdout to `/tmp/peer-<backend>-<epoch>.txt`. Default: true.
    #[serde(default)]
    pub save_raw: Option<bool>,
    /// Extra positional args appended (or splatted at `{extra}`) to the base args.
    #[serde(default)]
    pub extra_args: Option<Vec<String>>,
    /// Extra environment variables layered over backend + process env.
    #[serde(default)]
    pub extra_env: Option<HashMap<String, String>>,
}

/// Input for the `list_backends` tool (no parameters).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListBackendsParams {}

/// Input for the `image` tool. Dispatches image generation or editing to
/// backends declaring `kinds=["image"]` (Nano Banana, MiniMax, codex).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImageParams {
    /// Lookup key in the backend registry. Must declare `kinds = ["image"]`
    /// (or include "image" alongside "ask") in `peer.toml`.
    pub backend: String,
    /// Action: "generate" (default) or "edit". Edit requires `input_path`.
    #[serde(default)]
    pub action: Option<String>,
    /// Prompt used on generate (and on edit when `edit_prompt` is omitted).
    pub prompt: String,
    /// Edit prompt. When present, `action` is treated as "edit".
    #[serde(default)]
    pub edit_prompt: Option<String>,
    /// Path to an existing image to edit (required when action="edit").
    #[serde(default)]
    pub input_path: Option<String>,
    /// Override the default output path. Absent → auto at
    /// `{project_root}/.nott/generated/images/<session_id>.png`.
    #[serde(default)]
    pub output_path: Option<String>,
    /// Aspect ratio (backend-specific). Examples: "1:1", "16:9", "9:16", "4:3", "3:4".
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    /// Override the backend's default model.
    #[serde(default)]
    pub model: Option<String>,
    /// Paths to reference images (only used by Nano Banana for style
    /// consistency; ignored by other backends).
    #[serde(default)]
    pub reference_images: Option<Vec<String>>,
    /// Number of images to generate. Default 1. CLI backends (codex) only
    /// support n=1.
    #[serde(default)]
    pub n: Option<u8>,
    /// Per-request timeout override (ms). Clamped to [10000, 900000].
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Input for the `video` tool. Dispatches async video generation to backends
/// declaring `kinds=["video"]` (currently MiniMax video-01 / Hailuo-02).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct VideoParams {
    /// Lookup key in the backend registry. Must declare `kinds = ["video"]`
    /// in `peer.toml`.
    pub backend: String,
    /// Textual description of the video.
    pub prompt: String,
    /// Optional path to an image used as the first frame (MiniMax).
    /// Veo currently rejects this param — pass a text prompt only.
    #[serde(default)]
    pub first_frame_image: Option<String>,
    /// Override the default output path. Absent → auto at
    /// `{project_root}/.nott/generated/videos/<session_id>.mp4`.
    #[serde(default)]
    pub output_path: Option<String>,
    /// Aspect ratio (backend-specific). Veo accepts "16:9" or "9:16".
    /// Ignored by MiniMax video-01.
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    /// Override the backend's default model.
    #[serde(default)]
    pub model: Option<String>,
    /// Per-request wall-clock budget in ms. Clamped to [10000, 900000].
    /// Default is the backend's `timeout_ms_default` (typically 10 min).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

// -------------------------------------------------------------------------
// Response shapes
// -------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct BackendSummary<'a> {
    name: &'a str,
    description: &'a str,
    command: &'a str,
    stdin: bool,
    timeout_ms_default: u64,
    auth_hint: Option<&'a str>,
    kinds: Vec<&'static str>,
    transport: &'static str,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    api_key_env: Option<&'a str>,
    aspect_ratio_default: Option<&'a str>,
    /// True iff the env var named by `api_key_env` is currently set
    /// (empty value counts as missing). `None` when the backend does not
    /// declare an `api_key_env` (e.g. CLI backends like codex).
    status: &'static str,
}

#[derive(Debug, serde::Serialize)]
struct ListBackendsResponse<'a> {
    backends: Vec<BackendSummary<'a>>,
    registry_path: String,
    project_overrides_loaded: bool,
    env_override: bool,
}

// -------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------

fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, rmcp::ErrorData> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| rmcp::ErrorData::internal_error(format!("serialization error: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

fn peer_err(e: PeerError) -> rmcp::ErrorData {
    let kind = peer_error_kind(&e);
    let data = serde_json::json!({
        "peer_exit_code": e.exit_code(),
        "kind": kind,
    });
    rmcp::ErrorData::internal_error(e.to_string(), Some(data))
}

fn peer_error_kind(e: &PeerError) -> &'static str {
    match e {
        PeerError::BinaryNotFound { .. } => "binary_not_found",
        PeerError::AuthFailure { .. } => "auth_failure",
        PeerError::Timeout { .. } => "timeout",
        PeerError::ParseFailure { .. } => "parse_failure",
        PeerError::BackendNotFound { .. } => "backend_not_found",
        PeerError::RegistryLoad(_) => "registry_load",
        PeerError::Io(_) => "io",
        PeerError::InvalidInput(_) => "invalid_input",
        PeerError::MissingApiKey { .. } => "missing_api_key",
        PeerError::HttpFailure { .. } => "http_failure",
        PeerError::ProviderPayload { .. } => "provider_payload",
        PeerError::ImageNotProduced { .. } => "image_not_produced",
        PeerError::UnsupportedKind { .. } => "unsupported_kind",
    }
}

// -------------------------------------------------------------------------
// Tool router
// -------------------------------------------------------------------------

#[tool_router]
impl PeerMcpServer {
    /// Dispatch a prompt to a peer CLI and return the raw + parsed verdict.
    #[tool(
        description = "Dispatch a prompt to a peer LLM CLI (codex, gemini, minimax, claude, …) and return raw output + verdict."
    )]
    async fn ask(&self, params: Parameters<AskParams>) -> Result<CallToolResult, rmcp::ErrorData> {
        let p = params.0;
        let req = AskRequest {
            backend: p.backend,
            prompt: p.prompt,
            timeout_ms: p.timeout_ms,
            save_raw: p.save_raw.unwrap_or(true),
            extra_args: p.extra_args.unwrap_or_default(),
            extra_env: p.extra_env.unwrap_or_default(),
        };
        let resp = dispatch(&self.registry, req).await.map_err(peer_err)?;
        json_result(&resp)
    }

    /// List all backends registered in the resolved precedence chain.
    #[tool(
        description = "List all backends registered. Includes registry path, override flags, and per-backend capabilities (kinds, transport, auth status)."
    )]
    async fn list_backends(
        &self,
        _params: Parameters<ListBackendsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let backends: Vec<BackendSummary> = self
            .registry
            .list()
            .into_iter()
            .map(|b| {
                let status = match b.api_key_env.as_deref() {
                    None => "ok",
                    Some(var) => match std::env::var(var) {
                        Ok(v) if !v.is_empty() => "ok",
                        _ => "missing_key",
                    },
                };
                BackendSummary {
                    name: &b.name,
                    description: &b.description,
                    command: &b.command,
                    stdin: b.stdin,
                    timeout_ms_default: b.timeout_ms_default,
                    auth_hint: b.auth_hint.as_deref(),
                    kinds: b.kinds.iter().map(|k| k.as_str()).collect(),
                    transport: match b.transport {
                        crate::registry::Transport::Cli => "cli",
                        crate::registry::Transport::Http => "http",
                    },
                    provider: b.provider.as_deref(),
                    model: b.model.as_deref(),
                    api_key_env: b.api_key_env.as_deref(),
                    aspect_ratio_default: b.aspect_ratio_default.as_deref(),
                    status,
                }
            })
            .collect();
        let resp = ListBackendsResponse {
            backends,
            registry_path: self.registry.registry_path().display().to_string(),
            project_overrides_loaded: self.registry.project_overrides_loaded(),
            env_override: self.registry.env_override(),
        };
        json_result(&resp)
    }

    /// Dispatch an image generate/edit request to an image-capable backend.
    #[tool(
        description = "Generate or edit an image via the chosen backend (Nano Banana / Gemini, MiniMax image-01, or codex CLI with $imagegen). Saves to {project_root}/.nott/generated/images/ by default and returns the resulting file paths."
    )]
    async fn image(
        &self,
        params: Parameters<ImageParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let p = params.0;
        let action = match p.action.as_deref() {
            Some("edit") => ImageAction::Edit,
            Some("generate") | None => {
                // Auto-promote to edit when edit_prompt + input_path provided.
                if p.edit_prompt.is_some() && p.input_path.is_some() {
                    ImageAction::Edit
                } else {
                    ImageAction::Generate
                }
            }
            Some(other) => {
                return Err(peer_err(PeerError::InvalidInput(format!(
                    "action must be 'generate' or 'edit', got '{other}'"
                ))));
            }
        };

        let req = ImageRequest {
            action,
            backend: p.backend,
            prompt: p.prompt,
            edit_prompt: p.edit_prompt,
            input_path: p.input_path.map(PathBuf::from),
            output_path: p.output_path.map(PathBuf::from),
            aspect_ratio: p.aspect_ratio,
            model: p.model,
            reference_images: p
                .reference_images
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            n: p.n.unwrap_or(1),
            timeout_ms: p.timeout_ms,
        };
        let resp = dispatch_image(&self.registry, req)
            .await
            .map_err(peer_err)?;
        json_result(&resp)
    }

    /// Dispatch an async video generation request (MiniMax video-01 / Hailuo-02).
    #[tool(
        description = "Generate a video via the chosen backend (MiniMax video-01 / Hailuo-02 async flow). Saves to {project_root}/.nott/generated/videos/ by default. Wall-clock budget clamps at 15 min."
    )]
    async fn video(
        &self,
        params: Parameters<VideoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let p = params.0;
        let req = VideoRequest {
            backend: p.backend,
            prompt: p.prompt,
            first_frame_image: p.first_frame_image.map(PathBuf::from),
            output_path: p.output_path.map(PathBuf::from),
            aspect_ratio: p.aspect_ratio,
            model: p.model,
            timeout_ms: p.timeout_ms,
        };
        let resp = dispatch_video(&self.registry, req)
            .await
            .map_err(peer_err)?;
        json_result(&resp)
    }
}

// -------------------------------------------------------------------------
// Server handler
// -------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for PeerMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools_with(ToolsCapability { list_changed: None })
                .build(),
        )
        .with_server_info(Implementation::new("peer-mcp", env!("CARGO_PKG_VERSION")))
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "MCP server with four tools: \
             `ask` dispatches prompts to peer LLM CLIs \
             (codex / gemini / minimax / claude + user-added backends) and returns \
             raw stdout + parsed verdict (LGTM/BLOCK/CONDITIONAL/UNKNOWN); \
             `image` generates or edits images via Nano Banana (Gemini HTTP), \
             MiniMax image-01 (HTTP), or codex (`$imagegen` via CLI) and saves \
             artifacts to {project_root}/.nott/generated/images/; \
             `video` generates videos via MiniMax video-01 / Hailuo-02 (async \
             task + poll + download) and saves to {project_root}/.nott/generated/videos/; \
             `list_backends` reports all registered backends with their kinds / \
             transport / auth status. Configure via ~/.nott/peer.toml.",
        )
    }
}
