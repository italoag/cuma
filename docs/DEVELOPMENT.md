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
