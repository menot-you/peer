//! Entrypoint for the peer-mcp stdio MCP server.
//!
//! On boot:
//! 1. Resolve the backend registry (env override OR first-boot copy +
//!    user global + project override merge).
//! 2. Wire `PeerMcpServer` as the rmcp handler over stdio.
//!
//! If the registry fails to load, the process exits with
//! [`PeerError::RegistryLoad`]'s exit code (6).

use std::sync::Arc;

use peer_mcp::registry::Registry;
use peer_mcp::tools::PeerMcpServer;
use rmcp::service::ServiceExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Tracing goes to stderr so stdout stays clean for MCP protocol frames.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(
        target = "peer",
        version = env!("CARGO_PKG_VERSION"),
        "starting peer-mcp"
    );

    let registry = Registry::load().map_err(|e| {
        let code = e.exit_code();
        tracing::error!(
            target = "peer",
            exit_code = code,
            "registry load failed: {e}"
        );
        anyhow::Error::from(e)
    })?;

    tracing::info!(
        target = "peer",
        backends = registry.list().len(),
        registry = %registry.registry_path().display(),
        project_overrides = registry.project_overrides_loaded(),
        env_override = registry.env_override(),
        first_boot = registry.created_user_toml(),
        "registry loaded"
    );

    let server = PeerMcpServer::new(Arc::new(registry));
    let transport = rmcp::transport::io::stdio();
    let service = server.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}
