# ADR-001 — Rust workspace with an inward-pointing dependency graph

**Status:** Accepted

## Context

The harness spawns and supervises child processes, holds many concurrent
connections, and sits on the critical path of every task a user runs. It must
not leak processes, must not deadlock, and must survive an agent dying mid-turn.

It also has to be *rearranged* repeatedly: protocols are young and will move.

## Decision

A Cargo workspace of small crates, with dependencies pointing inward toward
`cuma-core`.

`cuma-core` holds the domain — tasks, agents, models, capabilities, errors,
events — and the *ports*: traits the outer layers implement. It depends on no
protocol SDK and no provider SDK.

Enforced at the workspace level:

```toml
[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
```

## Consequences

**Good.** A new protocol is a new adapter crate. A new front end subscribes to
the event bus. Neither touches the orchestrator. Compile times stay reasonable
because a change to the ACP adapter does not rebuild the router. The lint
configuration means a panic in a production path is a build failure, not a
runtime surprise.

**Costs.** Seventeen crates is more ceremony than one: adding a type that
several crates need means editing several manifests. Object-safe async traits
need `async_trait`, which boxes futures — measurable, but not against the cost
of spawning a subprocess.

**Rejected: a single crate with modules.** Module boundaries are advisory; crate
boundaries are enforced. The one rule that matters — nothing protocol-shaped in
the core — would have decayed on the first convenient import.
