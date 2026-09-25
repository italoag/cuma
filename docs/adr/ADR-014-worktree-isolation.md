# ADR-014 — Worktree isolation applies work back uncommitted

**Status:** Accepted

*Complements [ADR-011](ADR-011-workspace-isolation.md), which chose file
ownership over a worktree per task.*

## Context

ADR-011 rejected a worktree per task because merging worktrees back means
commits on the user's branch and conflict resolution nobody asked for. File
ownership is sufficient for correctness when prediction is right. When it is
wrong — an agent writes a file its description never named — two concurrent
tasks can still overwrite each other, silently.

The worktree helper that existed also had two flaws of its own: it branched from
`HEAD`, so an agent never saw the user's uncommitted work, and merging back ran
`git merge` on the user's branch.

## Decision

`limits.isolation = "worktree"` (default `shared`) runs each *writing* task in
its own worktree, on top of ownership claims rather than instead of them:

1. **Snapshot.** The live tree — tracked, modified and untracked files,
   `.gitignore` respected — is committed through a temporary index into a
   dangling commit. The user's index, working tree and branches are untouched.
2. **Isolate.** A detached worktree is created at that commit. No branch.
3. **Apply back.** On success, the worktree's diff against the snapshot is
   checked with `git apply --check` and then applied to the workspace as
   uncommitted changes. Snapshots and applies are serialized.
4. **Refuse, do not merge.** If the diff no longer applies — someone changed
   the same lines since — nothing is applied, the task fails with the reason,
   and its worktree is kept for a person to merge.

Read-only tasks keep working in the workspace itself.

## Consequences

- An unpredicted overlapping write becomes a visible failure with the work
  preserved, instead of a silent loss.
- Nothing is ever committed on the user's behalf.
- Each writing task pays for a snapshot and a worktree — cheap next to an
  agent's latency, noticeable on very large repositories. Off by default.
- Failed tasks' worktrees are discarded; refused ones accumulate under the
  system temp directory until removed.
