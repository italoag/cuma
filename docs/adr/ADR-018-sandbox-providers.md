# ADR-018 — Sandboxes are providers the operator chooses

**Status:** Accepted

*Extends [ADR-009](ADR-009-agent-isolation.md) and
[ADR-017](ADR-017-workspaces-restarts-confinement.md), which confine agents with
a fixed list of local runtimes.*

## Context

CUMA confined agents with ai-jail, else bubblewrap, `sandbox-exec` or firejail,
all rendering one profile. That served the agents the profile was written for
and left three needs unmet:

1. **Agents the profile does not know.** The writable home entries are a fixed
   list (`~/.claude`, `~/.codex`, …). ai-jail runs any command, but its
   `--agent-state` presets cover a dozen agents; Devin, OpenClaw, Hermes and
   the next one keep their login elsewhere and fail inside the jail.
2. **Stronger or different isolation.** A shared-kernel process sandbox is the
   floor, not the only choice. Operators want containers, microVMs
   (microsandbox, ArcBox, Firecracker, CubeSandbox), WebAssembly (Wasmer,
   agentOS) or a cluster (Kubernetes agent-sandbox, OpenSandbox).
3. **Local quirks.** On macOS, ai-jail refuses to execute binaries installed
   by mise (`Operation not permitted`), so every `npx`-launched agent fails
   negotiation on such a machine, with no alternative to pick.

The eight requested sandboxes do not share a shape. Some are command-line
wrappers that pass stdio through; some have a lifecycle (create, exec,
destroy); some cannot mount the workspace at all; two expose only HTTP APIs,
one of which has no stdin; one is a JavaScript library; one is a bare VMM.

## Decision

**A sandbox is a provider behind one port.** `cuma_core::ports` gains
`SandboxLaunch` and `SandboxSession`: a launch is the *prefix* an agent's
command runs under, plus an optional session that is *finished* after the
agent exits (copy its work back, tear down) or *aborted* if the launch is
abandoned (tear down only). ACP's stdio JSON-RPC needs nothing more, so the
ACP adapter is unchanged apart from opening and finishing launches; nothing
sandbox-shaped reaches the orchestrator or the router.

**Providers live in a new crate, `cuma-sandbox`,** one module per kind:

| Kind | Sandbox | Mechanism |
|---|---|---|
| `native` | ai-jail, bubblewrap, `sandbox-exec`, firejail | the existing profile, unchanged |
| `docker` | Docker, Podman, nerdctl, and ArcBox/OrbStack/Rancher engines; gVisor or Kata-Firecracker through `runtime` | `docker run -i`, workspace bind-mounted |
| `microsandbox` | microsandbox microVMs | `msb create`, `msb exec --stream`, `msb rm` |
| `arcbox` | ArcBox sandbox microVMs (Firecracker) | `abctl sandbox create/cp/run/exec/rm`, workspace copied |
| `kubernetes` | kubernetes-sigs/agent-sandbox `Sandbox` or `SandboxClaim` | `kubectl apply/wait/exec -i/delete`, workspace copied |
| `e2b` | CubeSandbox, and E2B itself | E2B REST API and envd's Connect process API |
| `opensandbox` | OpenSandbox | lifecycle and execd REST APIs, plus a stdio tunnel |
| `wasmer` | Wasmer | `wasmer run --volume --net` |
| `command` | anything with a wrapper CLI | a configured prefix template |
| `plugin` | anything else | an external program speaking a small JSON protocol |

agentOS (a Node.js library) and Firecracker (a bare VMM needing a kernel, a
root filesystem and a guest init) are integrated as **reference plugins**
under `plugins/sandbox/`, which is also the proof that the plugin protocol is
sufficient.

**Two workspace modes, stated by each provider.** *Mounted*: the workspace is
bind-mounted at the same absolute path, so the `cwd` ACP sends is valid inside.
*Copied*: the workspace is uploaded as a tar archive before the agent starts,
and the agent's work is brought back by a file-level three-way merge against
what was uploaded: a file changed only in the sandbox is applied, a file also
changed on the host is a conflict, and conflicts fail the task with the
sandbox's copy kept under `.cuma/sandbox-results/` for a manual merge. Nothing
is overwritten silently.

**Providers that only speak HTTP are reached through CUMA itself.** Their
prefix is `cuma sandbox exec --session <file> --`: CUMA bridges its own stdio
to the remote process. The session file is created mode 0600 and holds the
access token, so no secret appears in an argument list.

**The operator chooses, per agent if needed.** `[sandboxes.<name>]` declares a
provider; `security.agent_sandbox` picks the default (`auto` keeps today's
native order); `[agents.<id>] sandbox` overrides it for one agent, and the
agent's `state` and `env` say which directories it keeps its login in and
which variables it needs — so any agent can be confined, not only those a
preset knows.

**Credentials never travel on a command line.** Variables are forwarded by
name (`docker -e NAME`), declared as secrets the sandbox substitutes outside
the guest (`msb --secret NAME@host`), or written to a mode-0600 file inside the
sandbox that the launch wrapper sources and the session's teardown destroys.

**A provider is used only once something ran inside it.** `cuma sandbox probe`
and `cuma doctor` run a trivial command through the provider; at run time the
agent's own `initialize` is that test, and an agent whose negotiation fails is
registered unavailable with the reason.

## Consequences

**Good.** Any coding agent can run under any isolation the operator trusts,
from a process sandbox to a remote microVM, without code changes for a new
agent and without code changes for a new sandbox (a plugin). The router and
orchestrator are untouched. Network allowlists are enforced where the provider
can (ai-jail, microsandbox, Wasmer, OpenSandbox egress policies) and reported
as not enforced where it cannot.

**Bad.** Copied workspaces cost an upload and a download per task, and a
three-way merge at file granularity cannot combine two edits to the same file —
it refuses instead. Inside a Linux guest, an agent that keeps its login in the
macOS keychain has no login: such agents need an API key forwarded by name,
and the image must contain the agent's runtime — a guest never sees the host's
system binaries. Docker, microsandbox, Wasmer and agentOS were exercised live
(macOS, Apple M3 Pro); the others could not be where this was written (ArcBox
is not installed, there was no agent-sandbox cluster or OpenSandbox server,
CubeSandbox and Firecracker need x86-64 Linux with KVM). Their commands and
requests are tested against the documented interfaces and local stand-ins,
and the probe keeps an unworking one from being used. OpenSandbox has no
stdin API, so its agents need Node.js in the image for the tunnel.

## Alternatives

**Replace ai-jail.** Rejected: it is still the only local runtime that filters
by host, and the native profile is well tested. It became one provider among
others, and is now invoked with `--clean --no-save-config` and probed by
actually running something inside it.

**Link every sandbox's SDK.** Rejected: eight SDKs, several without Rust
versions, each pinning its own HTTP and gRPC stacks, for what are mostly a
handful of CLI calls or REST requests. The CLIs and documented APIs are the
stable surfaces; `reqwest` was already a dependency.

**In-process transports instead of a stdio bridge.** The ACP SDK can speak
over any byte stream, but that would give the adapter a second launch path.
A bridge process keeps one.

**Only a command template.** Rejected as the *only* mechanism: a template
cannot express a lifecycle (create, copy in, copy out, destroy), which five of
the eight sandboxes need. It is kept as the `command` kind for wrappers.
