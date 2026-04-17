# peer-mcp

MCP stdio server that dispatches a single prompt to a peer LLM CLI
(`codex` / `gemini` / `minimax` / `claude`, plus anything you add) and
returns the raw output + best-effort verdict parsing.

## Tools

- `ask(backend, prompt, ...)` — spawn the backend CLI, capture stdout,
  parse verdict (`LGTM` / `BLOCK` / `CONDITIONAL` / `UNKNOWN`), persist
  raw artifact to `/tmp/peer-<backend>-<epoch>.txt`.
- `list_backends()` — return the registry of backends available in the
  current resolution (defaults + user global + project override).

## Configuration

Registry lives in three layers (merged by `name`, last wins):

1. Shipped defaults — `peer-defaults.toml` (codex, gemini, minimax, claude).
2. User global — `~/.nott/peer.toml` (first-boot copy of the defaults).
3. Project override — `./.nott/peer.toml` (only loaded if cwd has one).

Escape hatch: `$PEER_BACKENDS_TOML=/abs/path.toml` bypasses all three.

Reset to defaults: `rm ~/.nott/peer.toml` and restart the MCP.
