# Memory

Long-term memory is what makes multi-session, multi-agent work coherent.

```
Session 1   Codex    implements a feature
Session 2   Claude   continues it
Session 3   Gemini   investigates a bug in it
```

All three need the same decisions, conventions and findings. That only works if
the memory lives outside any one of them — which is why CUMA integrates
`ai-memory` as an **external process**, not a linked crate. See
[ADR-005](adr/ADR-005-ai-memory.md).

## Ownership

Stated explicitly, because two stores that both hold "state" will drift.

| Data | Owner |
|---|---|
| Project knowledge | **ai-memory** |
| Architectural decisions | **ai-memory** |
| Coding conventions | **ai-memory** |
| Known issues and findings | **ai-memory** |
| User preferences | **ai-memory** |
| Sessions, tasks, attempts | CUMA runtime database |
| Usage, cost, latency | CUMA runtime database |
| Routing decisions and history | CUMA runtime database |
| Agent health | CUMA runtime database |

Rule of thumb: if another agent would want it, it belongs in memory. If it is
about how *this harness* behaved, it belongs in the runtime database.

## Configuration

```toml
[memory]
enabled = true
backend = "ai-memory-cli"
command = "ai-memory"
recall_limit = 8
```

Off by default. A harness that fails to start because an optional binary is
missing is a worse default than one that starts without recall.

## Where it is used

**Planning.** Relevant memories are recalled and passed to the planner, so a
plan reflects what earlier sessions learned.

**Context.** Recalled memories lead the prompt, ahead of the task description.

**Recording.** A completed task records what worked.

## Degradation

Every operation degrades rather than fails:

| Situation | Behaviour |
|---|---|
| Backend not on `PATH` | Probed once, cached, logged at info; recall returns empty |
| Backend crashes | Warning; the session continues |
| Malformed output | Parsed permissively; unparseable lines become unstructured memories |
| Operation exceeds 10s | Abandoned — recall is on the planning critical path |
| Memory disabled | `NullMemory`; a supported configuration, not a degraded one |

`remember` on an unavailable backend returns `"not-stored:backend-unavailable"`
rather than claiming success or raising an error. The caller deserves to know
which happened.

## Parsing

The CLI's output format is not a stable contract, so parsing accepts a JSON
array, a wrapped object under `memories` / `results` / `data`, newline-delimited
JSON, or plain lines. Field spellings are aliased (`content`/`text`/`body`,
`id`/`memory_id`/`uuid`, `kind`/`type`/`category`).

Being permissive here is the difference between "recall works across backend
versions" and "recall silently returns nothing after an upgrade".

## Commands

```bash
cuma memory status               # is it reachable?
cuma memory search "oauth"       # what does it know?
```
