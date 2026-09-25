# ADR-017 — Serve each directory, remember A2A tasks, confine agents everywhere

**Status:** Accepted

*Closes three gaps [ADR-012](ADR-012-bidirectional-protocols.md),
[ADR-015](ADR-015-a2a-dialects.md) and [ADR-009](ADR-009-agent-isolation.md) left
open.*

## Context

Three things CUMA did were narrower than what it claimed to be:

1. As an ACP agent it recorded the working directory an editor sent and then
   worked in the one it was started in.
2. As an A2A agent it kept tasks in memory, so a restart turned every task id
   a caller held into "not found".
3. It confined agents only when ai-jail was installed. Everywhere else they
   ran with the user's whole account: `~/.ssh`, cloud credentials, every token
   in CUMA's environment.

## Decision

**An orchestrator per ACP working directory.** Everything an orchestrator does
is rooted in one workspace, so a session in another directory gets an
orchestrator built for that directory, on first use, and shared by every
session there — sharing is what keeps two sessions in one repository behind a
single ownership ledger. Building happens off the connection's dispatch loop.
Idle orchestrators beyond eight directories are dropped; one in use never is.

**A project's configuration is code.** It names the commands agents are
launched with, so it is applied only for directories the operator trusts: the
one CUMA was started in, and those under `security.trusted_workspaces`. Any
other directory is served with CUMA's own configuration and a warning. The
alternative — loading whatever `.cuma/config.toml` a repository carries —
would make opening a folder in an editor execute what it says.

**A2A tasks are stored as their own A2A JSON** in the runtime database
(schema version 2), written on every state change. After a restart, finished
tasks read as before. A task that was running is marked failed with the
reason, **not re-run**: it may have been partly carried out, and repeating a
goal nobody re-sent could repeat its side effects.

**One agent profile, several runtimes.** ai-jail's profile — read-only system,
private `$HOME` with agent and toolchain state put back, credentials absent,
private `/tmp`, writable workspace, reduced environment — is rendered for
ai-jail, bubblewrap, `sandbox-exec` and firejail, in that order of preference.
A runtime is used only after running something inside it succeeds, so one
that is installed but blocked is never reported as protecting anything.
Environment variables are removed by name, never passed by value, because
command lines are readable by every user on the machine. The prefix is
computed per execution, from the directory the agent works in, so a worktree
and its repository's git directory are writable where they are needed.

## Consequences

- Editors that keep one agent process across projects now get each project's
  own agents, history and database — if trusted — or CUMA's otherwise.
- `.cuma/runtime.db` and `.cuma/` appear in each directory sessions work in,
  as they do for `cuma run`.
- A2A callers can poll across restarts; a caller whose task was interrupted
  must resend it. That is deliberate.
- Only ai-jail filters the network by host. Under the other runtimes a
  configured allowlist is reported as not enforced; `require_agent_sandbox`
  turns that, and "no runtime at all", into a refusal.
- Agents that authenticate through an environment variable not in the
  baseline need it listed in `security.agent_env`, as under ai-jail.
- The macOS profile is unit-tested but has not been exercised on macOS by
  this project's own checks; the Linux profiles were checked with a probe
  agent.
