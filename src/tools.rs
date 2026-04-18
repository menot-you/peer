//! MCP tool surface: `ask` and `list_backends`.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use rmcp::{handler::server::wrapper::Parameters, tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;

use crate::dispatch::{dispatch, AskRequest};
use crate::error::PeerError;
use crate::registry::Registry;

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
        description = "List all backends registered. Includes registry path and override flags."
    )]
    async fn list_backends(
        &self,
        _params: Parameters<ListBackendsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let backends: Vec<BackendSummary> = self
            .registry
            .list()
            .into_iter()
            .map(|b| BackendSummary {
                name: &b.name,
                description: &b.description,
                command: &b.command,
                stdin: b.stdin,
                timeout_ms_default: b.timeout_ms_default,
                auth_hint: b.auth_hint.as_deref(),
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
            "MCP server that dispatches prompts to peer LLM CLIs \
             (codex / gemini / minimax / claude + user-added backends). \
             Returns raw stdout + parsed verdict (LGTM/BLOCK/CONDITIONAL/UNKNOWN). \
             Configure via ~/.nott/peer.toml.",
        )
    }
}
