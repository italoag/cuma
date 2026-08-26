# Dependency analysis

Every dependency below was verified before use: resolved against the crates.io
sparse index for its current version, then **read from its vendored source** in
`~/.cargo/registry` to confirm the API actually exists. No API in this codebase
was written from assumption.

Verified against Rust 1.94.1, the toolchain present in this environment.

## Protocol SDKs

### `agent-client-protocol` 2.0.0 — ACP

The official Rust SDK, published by Zed (`github.com/agentclientprotocol/rust-sdk`).

- **MSRV** 1.88.0 — compatible.
- **Verdict: adopted.** It provides client, agent and proxy roles, the full v1
  schema, JSON-RPC transport and process spawning. Writing a second ACP
  implementation would be exactly the duplicated effort this project exists to
  avoid.
- API confirmed by reading `src/lib.rs`, `src/session.rs`, `src/acp_agent.rs`
  and `examples/yolo_one_shot_client.rs`, and by a live handshake against the
  published `codex-acp` adapter.
- Note: token reporting sits behind the `unstable_end_turn_token_usage` feature.
  Rather than enable an unstable feature or invent numbers, ACP attempts report
  `TokenUsage::estimated(0, 0)`, which surfaces as "unknown" in usage reports.

### `rmcp` 3.1.4 — MCP

The official Model Context Protocol Rust SDK.

- **MSRV** 1.88 — compatible.
- **Verdict: adopted.** Client role plus child-process transport is exactly what
  the tool layer needs.
- API confirmed by reading `src/service/client.rs` and `src/model.rs`.
  `CallToolRequestParams` is `#[non_exhaustive]`, so it is built through
  `::new(...).with_arguments(...)` rather than a struct literal.

### `a2a-rs` — A2A

**Verdict: not adopted for now. Implemented natively instead.**

| Version | MSRV | Compatible with 1.94.1? |
|---|---|---|
| 0.7.0 (current) | **1.96** | No |
| 0.4.1 (resolves) | — | Yes, but a superseded API |

The current release requires a newer Rust than this workspace. Pinning to 0.4.1
would mean coding against an API that has since moved, which is worse than a
narrow implementation of a stable specification.

CUMA therefore implements the slice of A2A it needs — Agent Card discovery,
`message/send`, `tasks/get`, `tasks/cancel` over JSON-RPC — in
`cuma-protocol-a2a`, behind the same `AgentAdapter` port as everything else.

**This is explicitly a temporary position.** Because the port is the boundary,
adopting `a2a-rs` later is a change to one crate. The trigger is raising the
workspace MSRV to 1.96 or later. Recorded as [ADR-003](docs/adr/ADR-003-a2a-interoperability.md).

## Memory

### `ai-memory` — long-term shared memory

**Verdict: adopted, as an external process rather than a linked crate.**

Two reasons, and the second matters more:

1. Version 0.10.0 requires Rust 1.96, above this workspace's MSRV. It also pulls
   `candle-core`, `candle-nn`, `candle-transformers` and `hf-hub` — a full
   machine-learning stack — into every CUMA build.

2. **Memory is only useful if it is shared.** The point of long-term memory here
   is that a Codex session, a Claude session and a CUMA session all see the same
   project knowledge. That cannot work if the memory lives inside one of them.
   `ai-memory` is designed for exactly this: it exposes an MCP server and a CLI
   precisely so different agent tools can share one store.

Linking it in would have been the technically inferior choice even without the
MSRV problem. Recorded as [ADR-005](docs/adr/ADR-005-ai-memory.md).

`cuma-memory` talks to it over its CLI, behind `MemoryStore`. Every operation
degrades rather than fails: a missing binary costs recall, never the session.

## TUI

### `ratatui` 0.30.2 and `ratatui-tea` 0.2.0

`ratatui-tea` is the Model/Msg/Cmd layer from the `ratatui-bubbletea` family (it
pulls `ratatui-bubbletea-theme` as a dependency, confirming the lineage).

**`ratatui-bubbletea` itself is not published to crates.io** — the index returns
404 for it. Only `ratatui-tea` and `ratatui-bubbletea-theme` are available as
registry crates. Using it directly would require a git dependency, which is a
decision for when the TUI event loop is built (Milestone 10) rather than now.

The TUI's state machine is already written and tested without depending on
either, so the choice stays open.

## RTK

`rtk` 0.1.0 exists on crates.io, but RTK (`rtk-ai/rtk`) is designed as a
**command proxy**, not a library: it wraps `git`, `cargo`, `grep` and similar and
filters their output down before it reaches an agent's context.

**Verdict: integrate as an optional external binary, detected on `PATH`.**
Linking a library would be the wrong shape for a tool whose value is intercepting
subprocess output. The `[rtk] enabled = "auto"` configuration reflects this: use
it if present, work without it if not.

*(Noted for the record: RTK is `rtk-ai/rtk`, not a project of Fabio Akita's — the
brief's own correction, confirmed here.)*

## Sandboxing

`ai-jail` is referenced in `SecurityConfig::sandbox_command` as a configurable
external sandbox rather than a hard dependency. Sandboxing is inherently
platform-specific, and an operator on a machine with `bubblewrap`, a container
runtime or macOS `sandbox-exec` should be able to use what they have.

## Core dependencies

All verified present at the listed version and MSRV-compatible.

| Crate | Version | Why |
|---|---|---|
| `tokio` | 1.53 | Async runtime |
| `serde` / `serde_json` | 1 | Serialization |
| `toml` | 1.1 | Configuration |
| `thiserror` | 2 | The error taxonomy |
| `tracing` / `tracing-subscriber` | 0.1 / 0.3 | Structured logging with correlation ids |
| `clap` | 4.6 | CLI |
| `rusqlite` | 0.40 (bundled) | Runtime state; bundled so there is no system SQLite requirement |
| `reqwest` | 0.13 | A2A transport. `rustls` + `rustls-native-certs`; **no** `default-tls`, so there is no OpenSSL build dependency |
| `chrono` | 0.4 | Timestamps |
| `uuid` | 1 | Identifiers |
| `rand` | 0.9 | Backoff jitter |
| `async-trait` | 0.1 | Object-safe async ports |
| `which` | 8 | Detecting whether an agent's command exists |
| `shell-words` | 1 | Parsing command strings without a shell |

## Deliberate omissions

| Considered | Why not |
|---|---|
| `petgraph` | Declared in the workspace but unused. The task DAG needs ready-set computation, cycle detection and cascade skipping — about 60 lines against a `BTreeMap`. A general graph library would add a dependency and an impedance mismatch for no benefit. |
| `sqlx` | Compile-time-checked queries are valuable, but they need a database at build time. `rusqlite` keeps the build hermetic. |
| `figment` | `cuma-config` needs field-level merge semantics that a generic layered-config crate does not express well. The merge rules are the interesting part and belong in the codebase, tested. |
| `dashmap` | The concurrency here is low-contention; `tokio::sync::RwLock` and `std::sync::Mutex` are sufficient and one less dependency. |
| Provider SDKs (OpenAI, Anthropic, …) | Behind the `LlmProvider` port and deliberately unimplemented. ACP/A2A are the primary path to coding agents; direct provider access is for the harness's own reasoning only (ADR-002). |

## MSRV

The workspace declares `rust-version = "1.88"`, the highest MSRV among adopted
dependencies. Two things want more:

- `a2a-rs` ≥ 0.5 requires 1.96
- `ai-memory` ≥ 0.8 requires 1.96

Both are handled by the process boundary rather than by raising the floor. When
the floor does rise, `cuma-protocol-a2a` becomes a candidate for replacement with
the official SDK; `cuma-memory` does not, because the process boundary there is
an architectural choice rather than a workaround.

## How to re-verify

```bash
cargo tree --workspace --duplicates   # duplicate versions
cargo update --dry-run                # what has moved
cargo test --workspace                # everything still holds
```
