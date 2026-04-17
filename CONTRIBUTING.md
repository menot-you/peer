# Contributing to menot-you-mcp-peer

Thanks for your interest. Here's how to get moving.

## Development Setup

```bash
git clone https://github.com/menot-you/peer
cd peer
cargo build
cargo test
```

No CLI credentials required — the test suite uses fake backends from
`tests/fixtures/` and the `$PEER_BACKENDS_TOML` escape hatch, so you can run
the full suite without `codex`, `gemini`, or `claude` installed.

## Project Layout

```
src/
├── main.rs        # Entrypoint — stdio MCP server startup, tracing init
├── lib.rs         # Public crate surface, module index
├── tools.rs       # MCP tool router via rmcp (`ask`, `list_backends`)
├── dispatch.rs    # Placeholder expansion → spawn → timeout → verdict parse
├── registry.rs    # Layered TOML merge (defaults → user → project → env)
└── error.rs       # Typed error enum with stable exit codes

tests/
└── integration_test.rs   # End-to-end: MCP transport + fake backends

peer-defaults.toml        # Shipped default registry (4 backends)
```

Everything that handles subprocesses lives in `dispatch.rs`. Everything that
reads TOML lives in `registry.rs`. Keep those responsibilities separate.

## Quality Gates

Every push must pass:

```bash
cargo fmt --check              # formatting
cargo clippy -- -D warnings    # zero clippy warnings
cargo test                     # unit + integration
```

CI runs these on every PR. Hooks in the parent workspace enforce them on
pre-push as well.

## Workflow

1. Fork the repo and create a branch: `git checkout -b feat/your-feature`
2. Write tests first — red-green-refactor. The test suite is fast (<10s)
   because backends are faked.
3. Implement the change. Keep each file under 500 LOC.
4. Update `README.md` if the tool surface or registry schema changed.
5. Open a PR with:
   - What changed and why (one paragraph max).
   - Before/after behavior if the change is observable.
   - Any new placeholders or env vars.

## Adding a Backend to the Shipped Defaults

Edit `peer-defaults.toml`. Rules:

- Must run unattended after auth has been set up once (`codex login`, etc.).
- Must write the answer to stdout. Verdict parsing ignores stderr.
- Must exit `0` on success. Non-zero maps to `parse_failure` unless stderr
  matches an auth pattern (`401`, `403`, `auth`, `login`, `unauthor…`).
- Must respect the timeout — no "trust me I'll be quick" defaults above
  `480_000` ms.
- `stdin = true` is preferred when the CLI supports it. Args get logged;
  stdin doesn't.

Example:

```toml
[[backend]]
name = "your-backend"
description = "One sentence. What kind of perspective does it bring?"
command = "your-cli"
args = ["--some-flag"]
stdin = true
timeout_ms_default = 180000
auth_hint = "run `your-cli login` if calls return 401"
```

Then add a fixture to `tests/integration_test.rs` that exercises the backend
through the fake-CLI harness. The real CLI is NEVER called from tests.

## Adding a Tool

Peer's surface is intentionally tiny — two tools. Before adding a third:

1. Can this be a new `extra_args` / `extra_env` pattern on `ask`?
2. Can this be computed client-side from `list_backends` output?
3. Does adding this tool make the MCP reach into a new domain (filesystem,
   network, something outside "dispatch a prompt")?

If you still need a new tool, the pattern is in `src/tools.rs`:

1. Add a parameter struct with `#[derive(Debug, Deserialize, JsonSchema)]`.
2. Add a `#[tool(description = "…")] async fn your_tool(...)` to the
   `#[tool_router] impl PeerMcpServer` block.
3. Return `Result<CallToolResult, rmcp::ErrorData>`.
4. Add integration tests in `tests/integration_test.rs`.
5. Update `README.md` under `## Tools`.

## Adding a Placeholder

Placeholders are expanded in `dispatch.rs::expand_args`. Adding one:

1. Extend the regex + match arm in `expand_args`.
2. Add a unit test covering:
   - Successful expansion
   - Missing-value behavior (error vs. default)
   - Interaction with `{extra}` splat ordering
3. Document in `README.md` under `### Placeholders`.
4. Document in `peer-defaults.toml` comments at the top of the file.

## Error Taxonomy

`src/error.rs` defines every error. Rules:

- Each variant maps to a stable `kind` string surfaced via MCP error `data`.
- Each variant maps to a stable `exit_code` (useful for `codex exec` style
  wrappers that pipe peer output into shell scripts).
- Never add a `kind` without updating the README table and tests.
- Never change an existing `kind` string — downstream tools grep for it.

## Code Style

- Doc comments (`///`) on every `pub` item.
- `#![forbid(unsafe_code)]` at the crate root — don't weaken this.
- Error handling: `Result<T, PeerError>`. No `unwrap()` outside tests.
- Files under 500 LOC; functions under 60 lines. Extract if either grows.
- One TOML layer per function in `registry.rs` — the precedence chain is
  easier to read as four steps than one branchy function.
- `tracing::info!` for lifecycle events (registry loaded, backend X spawned).
  `tracing::error!` only for genuine errors — typed errors bubble to the
  MCP client, not the log.

## Tests

Every new behavior gets at least one test. Categories:

- **Unit** — in-file `#[cfg(test)] mod tests`. Cover pure functions
  (placeholder expansion, verdict parsing, registry merge).
- **Integration** — `tests/integration_test.rs`. End-to-end via fake
  backend scripts under `tests/fixtures/`.
- **Chaos** — adversarial inputs: empty prompt, giant prompt, bad placeholders,
  zombie subprocesses, stderr flooding. Keep these alongside integration.

Run a subset during iteration: `cargo test --test integration_test`.

## Pull Requests

- One feature per PR.
- Tests required for new behavior.
- CI must pass (all jobs).
- Update `README.md` and `peer-defaults.toml` if tool surface or registry
  schema changed.

## License

By contributing, you agree that your contributions are licensed under the
AGPL-3.0 of this project. See [LICENSE](LICENSE).
