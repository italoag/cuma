# Dependency analysis

Every dependency below was verified before use: resolved against the crates.io
sparse index for its current version, then **read from its source** — the
vendored crate in `~/.cargo/registry`, or the project's repository for tools
used as external processes — to confirm the API or command line actually
exists. No API in this codebase was written from memory.

Built with Rust 1.94.1; the workspace is checked on its MSRV, 1.88, with and
without optional features.

## Protocol SDKs

### `agent-client-protocol` 2.0.0 — ACP

The official Rust SDK (`github.com/agentclientprotocol/rust-sdk`), schema 1.5.0.

- **MSRV** 1.88.0. **Adopted.** Client, agent and proxy roles, the v1 schema,
  JSON-RPC transport and process spawning.
- Read: `src/lib.rs`, `src/jsonrpc.rs`, `src/acp_agent.rs`, the schema's
  `v1/agent.rs` and `v1/client.rs`, and `examples/yolo_one_shot_client.rs`.
- Things the source settled that documentation did not:
  - A request handler runs inside the connection's dispatch loop and blocks
    every other message until it returns. CUMA's prompt handler therefore
    spawns the work (`ConnectionTo::spawn`); before that, `session/cancel`
    could not be handled during the prompt it was meant to cancel.
  - Agents are spawned as process-group leaders and the whole group is killed
    on drop, so aborting a run cannot orphan an `npx` → `node` agent.
  - `UsageUpdate` (context tokens, cumulative cost) is stable; per-turn
    `PromptResponse.usage` is behind `unstable_end_turn_token_usage`. CUMA
    enables that feature: the field is optional and default-on-error, so an
    agent that does not send it costs nothing, and one that does gets its
    tokens recorded as reported rather than estimated.
- The registry at `cdn.agentclientprotocol.com/registry/v1/latest/registry.json`
  is read against the format in the `agentclientprotocol/registry` repository
  (`FORMAT.md`, `registry.schema.json`).

### `rmcp` 3.1.4 — MCP

The official Model Context Protocol Rust SDK.

- **MSRV** 1.88. **Adopted**, with the `client`, `server`,
  `transport-child-process` and `transport-io` features.
- Read: `src/service/client.rs`, `src/handler/server.rs`, `src/model.rs`,
  `src/model/mrtr.rs`, `src/transport/io.rs`. `CallToolRequestParams` is
  `#[non_exhaustive]`; `call_tool` answers with `CallToolResponse`, of which
  `CallToolResult` is the `Complete` case.

### A2A — implemented natively

| Crate | Current | MSRV |
|---|---|---|
| `a2a-rs` | 0.10.0 | **1.96** |

Above this workspace's floor, so not adopted. CUMA implements A2A in
`cuma-protocol-a2a` against the protocol definition itself — `proto/a2a.proto`
as shipped in `a2a-rs` 0.7.0 — rather than against memory of it:

- **1.0 wire:** `SendMessage`, `SendStreamingMessage`, `GetTask`, `ListTasks`,
  `CancelTask`, `SubscribeToTask`; ProtoJSON shapes (camelCase fields,
  `TASK_STATE_*` and `ROLE_*` enum names, tag-free parts, field-presence unions);
  error codes `-32001`…`-32007`; SSE events whose data is a whole JSON-RPC
  response wrapping a `StreamResponse`; Agent Cards with `supportedInterfaces`.
- **0.3 fallback:** the client retries under 0.3 method names when a peer
  answers `-32601`; the server accepts both and answers in the dialect it was
  asked in; card parsing accepts a top-level `url`.

Swapping in the SDK later is a change to one crate, behind the `AgentAdapter`
port. See [ADR-003](docs/adr/ADR-003-a2a-interoperability.md).

## Memory: `ai-memory`

**Two different projects share this name.**

| | Source | Used by CUMA |
|---|---|---|
| `akitaonrails/ai-memory` 2.4.0 | the project the brief names | **Yes**, as an external process |
| crates.io `ai-memory` 0.10.0 | AlphaOne LLC, `alphaonedev/ai-memory-mcp` | No |

An earlier version of this document analysed the crates.io crate — its MSRV of
1.96 and its `candle` machine-learning stack — as if it were the brief's. It is
not. CUMA never linked either; it talks to Akita's `ai-memory` binary, whose
interface was read from its source (`crates/ai-memory-cli/src/cli.rs`,
`crates/ai-memory-mcp/src/server.rs`):

- CLI: `ai-memory search <query> -n <N> --json` returns `[{path, title,
  snippet, rank}]`; `ai-memory write-page --path P --body - --kind K -t tag`
  reads the body from stdin. There is no `add` command — the adapter that
  called one never worked.
- MCP: `ai-memory serve --transport stdio`, with tools `memory_query`,
  `memory_write_page` and `memory_handoff_begin` (typed, owned, claimed-once
  handoffs), among others.

The process boundary is the design, not a workaround: memory is only useful
shared, and a Codex session, a Claude session and CUMA can only share a store
that lives outside all of them. See [ADR-005](docs/adr/ADR-005-ai-memory.md).

## RTK: `rtk-ai/rtk`

**Also a name collision.**

| | Source | Used by CUMA |
|---|---|---|
| `rtk-ai/rtk` 0.49.0 ("Rust Token Killer", MSRV 1.91) | the brief's RTK | **Yes**, as an external binary |
| crates.io `rtk` 0.1.0 | "Rust Type Kit", `reachingforthejack/rtk` | No |

Because both install a binary called `rtk`, CUMA does not trust `PATH`: it runs
`rtk gain --format json` and uses the binary only if RTK's summary comes back.
The same command, with `--project`, supplies the savings RTK *measured*, which
`cuma usage` and `cuma doctor` report alongside CUMA's own estimates. Read from
`src/main.rs`, `src/analytics/gain.rs` and `src/core/tracking.rs`.

## Sandbox: `ai-jail` 2.1.0

An external binary. Read from its README and `src/cli.rs`. The flags CUMA
depends on: `--exec` (direct execution, no PTY proxy — required for ACP's stdio
JSON-RPC), `--agent-state` (the agent's own credentials, so agent-managed
authentication keeps working), `--allow-host` (filtered egress, from
`security.network_allowlist`), `--network`, `--env`, `--rw-map`.

## TUI

`ratatui` 0.30 and `crossterm` 0.29 (`event-stream`).

The `ratatui-bubbletea` family is published after all — as
`ratatui-bubbletea-components`, `ratatui-bubbletea-theme` and `ratatui-tea`,
all 0.2.0, MSRV 1.88, requiring ratatui 0.30 — although the umbrella name is
not a crate. The TUI's view model is a pure state machine tested without a
terminal, and its screens are simple tables and paragraphs, so none of the
three was adopted. `ratatui-tea` had been declared in the workspace without
any crate using it; the declaration is removed.

## Skills: integrity

| Crate | Version | MSRV | Why |
|---|---|---|---|
| `sha2` | 0.11 | 1.85 | Content digests |
| `ed25519-dalek` | 3.0 | 1.85 | Signatures; `verify_strict` rejects malleable and small-order edge cases |
| `base64` | 0.23 | 1.71 | Key and signature encoding |

`ed25519-dalek` 3.0 has no `std` feature (unlike 1.x); default features
(`fast`, `zeroize`) are used. Tests build keys from fixed bytes, so no RNG
feature is needed.

## Observability (optional)

Behind `cuma-cli`'s `otel` feature, off by default:

| Crate | Version | MSRV |
|---|---|---|
| `opentelemetry` / `opentelemetry_sdk` / `opentelemetry-otlp` | 0.33 | 1.75 |
| `tracing-opentelemetry` | 0.34 | 1.75 |

`opentelemetry-otlp` uses `http-proto` and `reqwest-blocking-client`: the SDK's
batch processor exports from its own thread, outside any tokio runtime. Its
`reqwest` is 0.13, the same as the workspace's, so TLS comes from the
workspace's `rustls` features and no second HTTP stack is built.

## Benchmarks

`criterion` 0.8.2 (MSRV 1.86), a dev-dependency with default features off
(no plotting, no rayon).

## Core dependencies

| Crate | Version | Why |
|---|---|---|
| `tokio` | 1.53 | Async runtime |
| `serde` / `serde_json` | 1 | Serialization |
| `toml` | 1.1 | Configuration |
| `thiserror` | 2 | The error taxonomy |
| `tracing` / `tracing-subscriber` | 0.1 / 0.3 | Spans and structured logs |
| `clap` | 4.6 | CLI |
| `rusqlite` | 0.40 (bundled) | Runtime state; no system SQLite |
| `reqwest` | 0.13 | A2A, providers, skill and agent registries. `rustls` + native roots; no OpenSSL |
| `axum` | 0.8 | CUMA's A2A server, including SSE |
| `chrono`, `uuid`, `rand` | — | Timestamps, identifiers, jitter and key generation |
| `async-trait` | 0.1 | Object-safe async ports |
| `which`, `shell-words` | 8, 1 | Finding and parsing commands without a shell |
| `tempfile` | 3 | Tests |

## Deliberate omissions

| Considered | Why not |
|---|---|
| `petgraph` | The task DAG needs a ready set, cycle detection and cascade skipping — a few dozen lines against a map. |
| `sqlx` | Needs a database at build time; `rusqlite` keeps the build hermetic. |
| `figment` | Field-by-field merge rules are the interesting part of `cuma-config` and belong in the codebase, tested. |
| `dashmap` | Low-contention concurrency; standard locks suffice. |
| Provider SDKs | Providers (`cuma-providers`) are a few HTTP calls over `reqwest` behind `LlmProvider`, used for the harness's own reasoning only (ADR-002). |
| `toml_edit` | `cuma agents add` appends one escaped table rather than rewriting a user's file. |

## MSRV

`rust-version = "1.88"`, the highest MSRV among linked dependencies (the ACP
and MCP SDKs). Checked with the 1.88.0 toolchain. What wants more is kept
behind a process boundary: `a2a-rs` (1.96), and the `ai-memory` and `rtk`
binaries, which CUMA runs rather than links.

## How to re-verify

```bash
cargo +1.88.0 check --workspace
cargo +1.88.0 check -p cuma-cli --features otel
cargo tree --workspace --duplicates
cargo update --dry-run
cargo test --workspace
```
