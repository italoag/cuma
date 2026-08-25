# ADR-003 — A2A implemented natively, behind a port

**Status:** Accepted, provisional

## Context

A2A covers agents that are not local coding CLIs: a remote architecture
reviewer, a specialist service, another orchestrator. The specification is at
1.0 and there is an official Rust SDK, `a2a-rs`.

## The problem

| Version | MSRV | Compatible with this workspace (1.94.1)? |
|---|---|---|
| 0.7.0 (current) | **1.96** | No |
| 0.4.1 (what resolves) | — | Yes, but a superseded API |

Adopting 0.4.1 would mean writing against an API that has since moved — the
worst of both options.

## Decision

Implement the slice of A2A that CUMA needs — Agent Card discovery,
`message/send`, `tasks/get`, `tasks/cancel` over JSON-RPC — in
`cuma-protocol-a2a`, behind the same `AgentAdapter` port as every other
transport.

**This is provisional.** The trigger for revisiting is raising the workspace
MSRV to 1.96 or later. Because the port is the boundary, adopting `a2a-rs` then
is a change to one crate with no effect on the orchestrator.

## Why this is not the same mistake as reimplementing ACP

ACP's SDK is compatible and its surface is large — sessions, streaming,
permissions, proxies, a conductor. Reimplementing it would be substantial and
duplicated work.

The A2A surface CUMA needs is four JSON-RPC methods and one well-known document.
That is a small, stable target against a published 1.0 specification.

## Security decisions taken along the way

Remote agents are the least trusted thing the harness talks to, so:

- **Cleartext is refused.** Non-HTTPS endpoints are rejected unless the host is
  unambiguously local. A host merely *starting* with `localhost` — say
  `localhost.evil.example` — does not qualify.
- **Response bodies are size-capped** at 8MB. A peer is not trusted to bound its
  own output.
- **Agent Card tags are sanitized** before becoming capability names: path
  separators, shell metacharacters, control characters and over-long tags are
  dropped. A card may claim `shell_execution` — that is what the field is for
  and the router will believe it — but it cannot turn its prose into anything
  but an opaque capability string.
- **Only a secret handle is stored**, never a token.

## Consequences

**Good.** No MSRV constraint imposed on the workspace by an optional protocol.
Full control over the security posture at a hostile boundary. The swap path is
one crate.

**Costs.** Streaming, push notifications and the full task lifecycle are not
implemented — the subset covers delegation, not every A2A feature. Spec changes
must be tracked by hand until the SDK is adopted.
