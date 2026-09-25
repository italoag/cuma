# ADR-002 — ACP as the primary coding-agent protocol

**Status:** Accepted

## Context

There are three ways to drive a coding agent:

1. Call a provider API directly and reimplement the agent loop.
2. Drive an agent CLI by parsing its stdout.
3. Speak a protocol the agent already implements.

## Decision

Use the **Agent Client Protocol** as the primary transport for local coding
agents, on the official `agent-client-protocol` Rust SDK. Provider SDKs sit
behind `LlmProvider` and are used only for the harness's *own* reasoning —
planning, classification, summarization — never as a substitute for ACP.

## Rationale

**Authentication is the decisive argument.** An ACP agent manages its own
credentials. Driving an already-logged-in Claude Code or Codex means the user's
existing subscription is reused and **the harness never holds an API key.** The
best way to avoid leaking a secret is not to have one.

Beyond that: ACP already handles session lifecycle, streaming, permission
requests, cancellation and capability negotiation. Reimplementing an agent loop
per provider would mean maintaining N of them, each subtly wrong.

Screen-scraping a CLI is not a protocol. It breaks on every output change.

## Consequences

**Good.** No credentials for ACP agents. Capability negotiation is real, not
assumed. Every ACP-compatible agent works without new code. And the same SDK
supports the agent role, which is what makes exposing CUMA *as* an ACP agent —
the primary architectural goal — a matter of implementing an existing trait.

**Costs.** ACP does not report changed files per prompt turn, so handoffs cannot
list them from the protocol alone. Token reporting is behind an unstable feature,
so ACP attempts report usage as `estimated(0,0)` — surfaced as "unknown" rather
than fabricated.

**Rejected: parsing agent CLI output.** Fragile by construction, and it would put
the harness in the business of tracking every agent's output format.
