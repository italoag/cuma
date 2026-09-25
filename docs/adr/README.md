# Architecture decision records

Each record states a decision, the forces behind it, and what it costs. A
decision without a stated cost is a decision that has not been thought through.

| # | Decision | Status |
|---|---|---|
| [001](ADR-001-rust-workspace.md) | Rust workspace with an inward-pointing dependency graph | Accepted |
| [002](ADR-002-acp-primary.md) | ACP as the primary coding-agent protocol | Accepted |
| [003](ADR-003-a2a-interoperability.md) | A2A implemented natively, behind a port | Accepted, provisional |
| [004](ADR-004-mcp-tools.md) | MCP for tools, via the official SDK | Accepted |
| [005](ADR-005-ai-memory.md) | `ai-memory` as an external process, not a linked crate | Accepted |
| [006](ADR-006-routing-strategy.md) | Filter, then weighted score, then explain | Accepted |
| [007](ADR-007-sqlite-persistence.md) | SQLite for runtime state, memory for knowledge | Accepted |
| [008](ADR-008-skill-security.md) | Trust derived from evidence, never from a claim | Accepted |
| [009](ADR-009-agent-isolation.md) | A process per execution; parallelism gated on isolation | Accepted |
| [010](ADR-010-event-driven-runtime.md) | An event bus between the runtime and every interface | Accepted |
| [011](ADR-011-workspace-isolation.md) | File ownership, not dependency order, gates parallelism | Accepted |
| [012](ADR-012-bidirectional-protocols.md) | CUMA serves the same protocols it consumes | Accepted |
| [013](ADR-013-skill-evidence.md) | A skill's evidence is its digest and a signature, checked at install | Accepted |
| [014](ADR-014-worktree-isolation.md) | Worktree isolation applies work back uncommitted | Accepted |
| [015](ADR-015-a2a-dialects.md) | Speak A2A 1.0, fall back to 0.3, and run tasks as sessions | Accepted |
| [016](ADR-016-mcp-sharing.md) | Agents reach shared MCP servers through CUMA's proxy | Accepted |
