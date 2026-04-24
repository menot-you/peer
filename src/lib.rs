//! # peer-mcp
//!
//! MCP stdio server that dispatches prompts to peer LLM CLIs
//! (`codex`, `gemini`, `minimax`, `claude`, …).
//!
//! Architecture:
//! - [`registry`] loads a TOML registry via a layered precedence chain
//!   (shipped defaults → `~/.nott/peer.toml` → `./.nott/peer.toml` → env override).
//! - [`dispatch`] expands placeholders, spawns the CLI, waits with timeout,
//!   captures stdout/stderr, parses a verdict.
//! - [`tools`] exposes two rmcp tools: `ask` and `list_backends`.
//! - [`error`] defines the typed error enum with stable exit codes.

#![forbid(unsafe_code)]

pub mod dispatch;
pub mod error;
pub mod image;
pub mod project;
pub mod registry;
pub mod session;
pub mod tools;
pub mod video;
