# ADR-015 — Speak A2A 1.0, fall back to 0.3, and run tasks as sessions

**Status:** Accepted

*Extends [ADR-003](ADR-003-a2a-interoperability.md).*

## Context

The first A2A implementation spoke a pre-1.0 dialect (`message/send`,
`kind`-tagged parts), treated `input-required` as success, and ran every request
synchronously, telling callers there was no task to look up afterwards. A2A 1.0,
read from `a2a.proto`, renamed every method, changed every shape, and defines a
task lifecycle with streaming. Much of the deployed ecosystem still speaks 0.3.

## Decision

**Dialects.** The client speaks 1.0 and, when a peer answers `-32601` to a 1.0
method, retries that call and later ones in 0.3. The server accepts both and
answers in the dialect it was asked in. Agent Cards are parsed in either shape
and published strictly in 1.0 — extra legacy fields risk rejection by strict
ProtoJSON parsers.

**Settled means settled.** `COMPLETED` is the only success. `INPUT_REQUIRED`
and `AUTH_REQUIRED` stop polling and fail the attempt with what the peer asked;
`REJECTED`, `FAILED` and `CANCELED` fail it. A task that comes back running is
polled with backoff; a peer that advertises streaming is followed over SSE; a
task abandoned by its deadline is cancelled rather than orphaned.

**Tasks are sessions.** Each task CUMA serves runs as its own orchestrator
session, whose id is chosen before it starts (`run_session`), so its stream
carries only its own events. Cancelling aborts the run; ownership claims are
released on drop and ACP agents' process groups are killed, so an abort leaves
nothing locked or running.

## Consequences

- A peer on either dialect works; a version bump costs one extra round trip.
- The task store is bounded. It was in memory, so a restart forgot tasks;
  since [ADR-017](ADR-017-workspaces-restarts-confinement.md) it is persisted.
- Push notifications and the extended card are refused with their own error
  codes and advertised `false`.
