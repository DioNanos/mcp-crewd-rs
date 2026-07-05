# mcp-crewd-rs

**crewd** is a cell fabric daemon for AI agents: it lets one AI session
(Claude Code, Codex CLI, or any MCP client) spawn, coordinate and message
other AI worker sessions — *cells* — through a single MCP server. Rust,
one static binary per component, no network listener: everything runs over
a local Unix socket.

Part of the `mcp-*-rs` family of Rust MCP servers.

```
┌────────────┐  MCP stdio   ┌──────┐   UDS (NDJSON)   ┌───────┐  spawns   ┌─────────────────┐
│ Claude Code│─────────────▶│ crew │─────────────────▶│ crewd │──────────▶│ engine processes│
│ / Codex CLI│  cell_* tools│ shim │  0600 socket     │daemon │           │ claude / codex  │
└────────────┘              └──────┘                  └───────┘           │ / pi workers    │
                                                                          └─────────────────┘
```

## Components

| Binary | Role |
|--------|------|
| `crewd` | Daemon: cell registry, job scheduler, engine supervisor, message bus, SQLite (WAL) store, append-only hash-chained audit log |
| `crew`  | Operator CLI (`status`, `inspect`, `audit verify`) + per-cell MCP stdio shim (`crew mcp`) |

## MCP tools

Mounted per-cell via `crew mcp`:

- **Fabric** — `cell_spawn` (launch a worker cell: engine + profile + task),
  `cell_send_task`, `cell_status`, `cell_result`, `cell_cancel`, `cell_list`
- **Bus** — `cell_send` (fire-and-forget), `cell_ask` / `cell_await`
  (ask ticket + long-poll reply), `cell_reply`, `cell_broadcast`, `cell_inbox`

Worker cells get the same shim with `--worker-mode`, which hides the spawn
surface (no uncontrolled nested fan-out).

## Engines

| Engine | How | Session continuity |
|--------|-----|--------------------|
| `claude` | Node shim on the Claude Agent SDK (`shim/claude-shim.mjs`) | resume by session id |
| `codex`  | `codex app-server` JSON-RPC (v2 protocol) | reattach by thread id |
| `pi`     | pi rpc | none (v0) |

Claude profiles: `max` (default credentials) or `zai-a` / `zai-p`
(Anthropic-compatible Z.AI endpoint; requires `keys_env_path`, see below).

A cell's identity is its **working directory**: the spawned engine loads
whatever `CLAUDE.md`, `.mcp.json`, memory and skills live in the `cwd` you
pass to `cell_spawn`. One daemon can therefore serve several "personas" by
spawning cells in different project roots.

## Install

Requirements: Linux (Unix sockets, `SO_PEERCRED`), Rust ≥ 1.85. The claude
engine additionally needs Node ≥ 20 with `@anthropic-ai/claude-agent-sdk`
installed for the shim; the codex engine needs the `codex` CLI on the
daemon's `PATH`.

```sh
cargo build --release
install -m 0755 target/release/crewd target/release/crew ~/.local/bin/
```

### Configuration

`crewd.toml` (passed explicitly via `--config`, never cwd-derived):

```toml
runtime_dir = "/home/you/.config/crewd/runtime"   # socket, db, audit, tokens
acl_path    = "/home/you/.config/crewd/acl.toml"
# Required for the zai-a / zai-p claude profiles: a KEY=value env file
# containing ZAI_API_KEY_A / ZAI_API_KEY_P.
keys_env_path = "/home/you/.config/crewd/keys.env"
```

`acl.toml` — one section per registered cell with its engine and
capabilities (`send`, `ask`, `reply`, `broadcast`, `read_inbox`,
`list_cells`, `read_audit`, `spawn`):

```toml
[cell.coordinator]
engine = "claude"
capabilities = ["send","ask","reply","broadcast","read_inbox","list_cells","read_audit","spawn"]
```

Per-cell auth: a 0600 token file per cell (`L0` scheme). Mount the shim in
your MCP client config:

```jsonc
// .mcp.json (Claude Code) — one entry per cell identity
{
  "mcpServers": {
    "crew": {
      "command": "crew",
      "args": ["mcp",
        "--runtime-dir", "/home/you/.config/crewd/runtime",
        "--cell", "coordinator",
        "--token-file", "/home/you/.config/crewd/coordinator.secret"]
    }
  }
}
```

### systemd

```ini
[Unit]
Description=crewd — cell fabric daemon
After=network.target

[Service]
Type=simple
User=you
Group=you
# The claude shim path is resolved relative to this directory.
WorkingDirectory=/path/to/mcp-crewd-rs
ExecStart=%h/.local/bin/crewd --config %h/.config/crewd/crewd.toml
# IMPORTANT: the default systemd PATH does not include user-level bins.
# Engine adapters spawn `codex` / `node` from the daemon's PATH:
Environment="PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin"
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
```

## Security model

- Unix socket `0600` inside a `0700` runtime dir; bind refuses symlinks.
- `SO_PEERCRED` checked at handshake, then a per-cell token (file `0600`).
- Engine children run with an **exact env allowlist** — secrets are read
  from `keys_env_path` by the daemon and injected only into the child that
  needs them; they never appear in logs or error messages.
- Append-only, hash-chained audit log (`crew audit verify`).
- Honest failure states: crashed/orphaned turns are recovered at boot as
  `interrupted` / `failed_unknown`, never silently retried after engine
  acceptance.

**Scope honesty**: processes running under the *same UID* as the daemon can
read the socket and token files — crewd separates *cells*, it is not a
same-user privilege boundary. See [THREAT_MODEL.md](THREAT_MODEL.md).

## Platform

Linux only (tested). Other Unixes may work (rustix + Unix sockets) but are
not CI-covered. No Windows support.

## Docs

- [SPEC.md](SPEC.md) — normative protocol & behaviour spec
- [THREAT_MODEL.md](THREAT_MODEL.md) — threat model and non-goals

## Status

Pre-1.0 (`v0.1.x`): single-host fabric (UDS). Cross-host fabric, operator
token CLI and warm engine reuse are on the roadmap.

## License

Apache-2.0. See [LICENSE](LICENSE).
