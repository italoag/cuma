# Development

## Requirements

Rust 1.88 or later (the workspace MSRV). Verified against 1.94.1.

Nothing else: `rusqlite` is `bundled` and `reqwest` uses `rustls`, so there is no
system SQLite and no OpenSSL.

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all

cargo test -p cuma-router                    # one crate
cargo test -p cuma-orchestrator --test end_to_end
```

## Layout

```
crates/
├── cuma-core           domain, ports, errors, events   ← depends on nothing
├── cuma-config         layered configuration
├── cuma-registry       agent / model / capability registries
├── cuma-router         filter, score, explain
├── cuma-resilience     backoff, breakers, classification
├── cuma-planner        goal → DAG
├── cuma-orchestrator   execution loop, context assembly
├── cuma-usage          tokens, cost, outcomes
├── cuma-persistence    SQLite runtime state
├── cuma-memory         MemoryStore implementations
├── cuma-skills         discovery, validation, installation
├── cuma-protocol-acp   ACP adapter
├── cuma-protocol-a2a   A2A adapter
├── cuma-protocol-mcp   MCP tools
├── cuma-server-acp     CUMA as an ACP agent
├── cuma-workspace      isolation, checkpoints, sandbox, RTK
├── cuma-providers      LlmProvider implementations, secret stores
├── cuma-testkit        mock agents
├── cuma-tui            view model and rendering
└── cuma-cli            headless interface
```

Dependencies point inward.

## Rules

**Nothing protocol-shaped enters `cuma-core`.** If a type there needs to know
whether an agent speaks ACP or A2A for anything beyond bookkeeping, the
abstraction has leaked.

**No `unwrap()` or `expect()` in production paths.** Enforced at the workspace
level:

```toml
[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
```

Test modules opt out with `#![allow(clippy::unwrap_used, ...)]`.

**An estimate is never rendered as a measurement.** Use `Known<T>`.

**Defaults deny.**

## Testing

Test names are sentences, and they assert *behaviour* rather than
implementation:

```rust
#[test]
fn an_agent_lacking_a_required_capability_is_never_selected() { … }

#[test]
fn a_manifest_cannot_talk_itself_up() { … }

#[test]
fn every_failure_sequence_terminates() { … }
```

A test named `test_routing` tells a future reader nothing about what broke.

### Mock agents

`cuma-testkit` reproduces every failure mode without spending a token:

```rust
let agent = MockAgent::scripted("flaky", vec![
    Behaviour::RateLimit { retry_after_ms: Some(100) },
    Behaviour::ok("succeeded on the retry"),
]);
```

Available: `Succeed`, `Slow`, `Timeout`, `RateLimit`, `QuotaExceeded`,
`PartialStream`, `Crash`, `InvalidResponse`, `AuthFailure`, `ContextOverflow`,
`TaskFailure`.

Behaviour varies per attempt, which is what makes retry and fallback testable.

**Every new failure mode gets a mock before it gets a handler.**

### Live sandbox tests

Each sandbox provider has a test that runs an ACP agent inside the real
thing — `crates/cuma-sandbox/tests/live_<kind>.rs`. Without its variables a
live test passes having done nothing, so `cargo test` never needs a sandbox.

| Test | Needs | Variables |
|---|---|---|
| `live_docker` | a container engine | `CUMA_LIVE_DOCKER_IMAGE` |
| `live_microsandbox` | `msb`; KVM or Apple Silicon | `CUMA_LIVE_MICROSANDBOX_IMAGE` |
| `live_wasmer` | `wasmer` | `CUMA_LIVE_WASMER_PACKAGE` (`wasmer/bash`), `CUMA_LIVE_WASMER_PROGRAM` |
| `live_agentos` | node, `@rivet-dev/agentos-core` | `CUMA_LIVE_AGENTOS_MODULES` (its `node_modules`) |
| `live_kubernetes` | a cluster with agent-sandbox, `kubectl` | `CUMA_LIVE_KUBERNETES_IMAGE`, `…_NAMESPACE`, `…_CONTEXT` |
| `live_opensandbox` | a running `opensandbox-server` | `CUMA_LIVE_OPENSANDBOX_URL`, `…_IMAGE`, `…_KEY_REF`, `CUMA_LIVE_CUMA_BIN` |
| `live_e2b` | CubeSandbox or E2B | `CUMA_LIVE_E2B_API_URL`, `…_DOMAIN`, `…_TEMPLATE`, `…_KEY_REF`, `…_ENVD_SCHEME`, `CUMA_LIVE_CUMA_BIN` |
| `live_arcbox` | ArcBox (`abctl`) | `CUMA_LIVE_ARCBOX_IMAGE`, `CUMA_LIVE_ARCBOX_PROGRAM` |
| `live_firecracker` | Linux with KVM, e2fsprogs, python3 | `CUMA_LIVE_FIRECRACKER_KERNEL`, `…_ROOTFS`, `…_BIN` |

Images need only `sh`, `sed` and `tar` (`node` too for OpenSandbox's stdio
tunnel): `ci/sandboxes/images/sandbox-test` builds one.
`ci/sandboxes/firecracker-assets.sh <dir>` fetches Firecracker and a kernel
and builds a root filesystem with `cuma-init`. `*_KEY_REF` variables name the
variable that holds a key, never the key; `CUMA_LIVE_CUMA_BIN` is the `cuma`
the stdio bridge runs as (default `target/debug/cuma`).

The [Sandboxes workflow](../.github/workflows/sandboxes.yml) runs every test
on pull requests that touch the sandbox code, and by hand. Two jobs need
self-hosted runners, and stay off until enabled:

| Job | Runner labels | Repository settings |
|---|---|---|
| `arcbox` | `self-hosted, macOS, ARM64, arcbox` — Apple Silicon, ArcBox installed | variable `CUMA_ARCBOX_RUNNER=true`; optional `CUMA_ARCBOX_IMAGE` |
| `cubesandbox` | `self-hosted, linux, X64, cubesandbox` — reaches a CubeSandbox deployment | variables `CUMA_CUBESANDBOX_RUNNER=true`, `CUMA_CUBE_API_URL`, `CUMA_CUBE_DOMAIN`, `CUMA_CUBE_TEMPLATE`, optional `CUMA_CUBE_ENVD_SCHEME`; secret `CUMA_CUBE_API_KEY` |

Self-hosted jobs never run a pull request from a fork. The
[Sandbox images workflow](../.github/workflows/sandbox-images.yml) publishes
`ghcr.io/italoag/cuma-sandbox-test` and `ghcr.io/italoag/cuma-agent-node`, and
mirrors `ci/sandboxes/mirror.txt` as `ghcr.io/italoag/cuma-*`. Check each
package's visibility on GitHub after its first publication: a private one
needs `docker login ghcr.io` wherever it is pulled. Lint workflows with
`actionlint` (custom runner labels are declared in `.github/actionlint.yaml`).

## Adding things

### A protocol

1. New crate `cuma-protocol-<name>`
2. Implement `AgentAdapter`, and `AgentDiscovery` if agents can be found
3. Translate at the crate boundary — nothing protocol-shaped leaves
4. Classify failures into `ErrorClass` from structured data where possible

No change to the orchestrator, the router or the core.

### A routing dimension

1. Add the scoring function to `cuma-router/src/score.rs`
2. Add its weight to `RouterWeights` and every strategy preset
3. Include it in `ScoreBreakdown::render` — an invisible dimension cannot be tuned
4. Test that unknown data scores neutrally, not optimally

### A capability

Add the variant to `Capability`, its parse arm, and its baseline in
`TaskType::baseline_capabilities`. Unknown names already degrade to
`Capability::Custom`, so discovery keeps working meanwhile.

## Debugging

```bash
RUST_LOG=cuma_router=trace cuma explain "your goal"
cuma doctor
cuma agents show <id>
cuma usage
```

If an agent is never selected, read the `Rejected:` section of an explanation
first. It is almost always a capability mismatch or an open breaker, not a low
score.
