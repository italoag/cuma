# Quickstart

From zero to a verified `cuma run`, step by step. Each step ends with a
command whose output tells you it worked — if one does not, stop and fix it
before moving on; every later step depends on the earlier ones.

What you will have at the end: CUMA installed, one or more real coding agents
registered and reachable, a first goal explained and executed, and the
commands to inspect what it cost.

## Prerequisites

| Requirement | Why | Check |
|---|---|---|
| Rust 1.88+ (`rustup`) | To build the binary | `cargo --version` |
| Node.js / `npx` | The published ACP adapters for Claude Code and Codex are npm packages | `npx --version` |
| A logged-in agent | CUMA reuses the agent's own authentication — it never holds an API key | run `claude` or `codex` once and sign in |
| A git repository | Workspaces are checkpointed before writes so changes stay recoverable | `git status` in your project |

No database server, no OpenSSL, no system SQLite: `rusqlite` is bundled and
`reqwest` uses rustls.

## Step 1 — Build and install

```bash
git clone <this repo> && cd cuma
cargo build --release
cargo install --path crates/cuma-cli    # puts `cuma` on PATH
```

```bash
cuma --version
```

Without installing, everything below also works as `cargo run --release -p cuma-cli -- <command>`.

## Step 2 — Minimal configuration

The fastest path is `cuma setup`: it detects the agents installed on your
machine (native ACP agents and ones reachable through `npx`/`uvx` adapters),
lets you pick which to enable in an interactive wizard, and writes a working
config with safe defaults. `cuma setup --project` writes `.cuma/config.toml`;
without a flag it writes the global `~/.config/cuma/config.toml`, and `--yes`
applies the defaults non-interactively. Re-running it merges into the existing
file — your edits are preserved and a `.bak` backup is created.

To write the config by hand instead, in the project you want agents to work on
(must be a git repository for checkpoints to protect you):

```bash
mkdir -p .cuma && cat > .cuma/config.toml <<'EOF'
[router]
strategy = "balanced"

[agents.claude-code]
protocol = "acp"

[agents.codex]
protocol = "acp"
EOF
```

`claude-code` and `codex` are *well-known* names: they resolve automatically
to the published `npx` ACP adapters, so no `command` line is needed. Any other
agent needs its launch command:

```toml
[agents.my-agent]
protocol = "acp"
command = "my-agent --acp"        # anything that speaks ACP over stdio
env = ["SOME_API_KEY"]            # variable NAMES only — values are never written here
state = ["~/.my-agent"]           # dirs holding its login; writable inside the sandbox
```

A remote agent speaks A2A instead:

```toml
[agents.architect]
protocol = "a2a"
endpoint = "https://architect.example/a2a"
auth_secret_ref = "CUMA_ARCHITECT_TOKEN"   # name of an env var holding the token
```

## Step 3 — Verify the installation

```bash
cuma doctor
```

Healthy output names every config layer it read, `agents: N registered, N
routable`, a reachable database, `workspace: git repository, checkpointing
before writes`, and which sandbox agents run in. Anything under `FAIL` is a
real problem — read it before continuing.

```bash
cuma agents list      # what the router can actually see, with health
cuma sandbox list     # what agents run inside; `auto` picks the best available
cuma sandbox probe auto   # proves the default sandbox can launch something
```

Common failures at this step:

- **An agent is missing from `agents list`.** Its command is not on `PATH` —
  an agent the machine cannot launch is deliberately not registered.
- **The workspace is not a git repository.** Run `git init` or accept that
  agents' changes will not be recoverable.
- **No sandbox runtime.** Agents then run unconfined unless you set
  `security.require_agent_sandbox = true`; on macOS `sandbox-exec` is picked
  automatically, elsewhere bubblewrap/firejail, or install
  [ai-jail](docs/SANDBOXES.md) or configure a provider
  (`[sandboxes.<name>]`, e.g. docker).

## Step 4 — Explain before you run

```bash
cuma explain "add a health endpoint to the HTTP server"
```

You get the task DAG (type, risk, dependencies) and the routing decision for
the first task: the selected agent/model, the score broken down across
quality, cost, latency, reliability and context fit, the alternatives, and —
most useful when something is wrong — the `Rejected:` list naming every agent
that was filtered out and why (capability mismatch, open circuit breaker).

Nothing executes. `cuma run --dry-run "..."` is the same command.

## Step 5 — Run for real

```bash
cuma run "add a health endpoint to the HTTP server"
```

Progress goes to stderr (planned tasks, agent selected per task, retries,
fallbacks); the result goes to stdout — a summary plus each task marked
`[x]`/`[!]`/`[-]` with the agent that did it. **A failed session exits
non-zero**, so `cuma run "..." && ./deploy.sh` is safe.

Useful global flags (work on every command):

```bash
cuma run "..." --agent claude-code      # pin an agent
cuma run "..." --model sonnet           # pin a model
cuma run "..." --strategy cost-first    # override the strategy preset
cuma run "..." --max-cost 2.00          # hard cap on this invocation
cuma run "..." --workspace /path/to/dir # run against another project
cuma run "..." --json                   # structured output for CI/agents
cuma run "..." -vv                      # more logging on stderr
```

## Step 6 — See what it cost

```bash
cuma usage              # tokens, cost, outcomes per agent
cuma usage --by-model   # the same, per model
```

An estimate is never rendered as a measurement: cost shows `unknown`, not
`$0.00`, when an agent reports usage without pricing, and a partially-priced
total is a lower bound.

## The interactive interface

```bash
cuma chat        # or just `cuma`
```

The TUI is the same orchestrator behind a different front end — sessions,
agents, skills and memory are screens over the same event bus.

## Where CUMA also shows up as an agent

CUMA consumes agents over ACP/A2A and *is* one in both directions:

```bash
cuma serve --protocol acp   # stdio — point Zed or JetBrains at `cuma serve`
cuma serve --protocol a2a   # HTTP on 127.0.0.1:8420 (--bind, --allow-unauthenticated)
cuma serve --protocol mcp   # stdio — expose tools to any MCP host
```

From the editor's side there is a single agent; behind it is the whole
routing apparatus. A2A callers authenticate with bearer tokens named in
`security.a2a_server_token_refs`.

## Going further

```bash
cuma agents discover --registry   # browse the ACP agent catalogue
cuma agents add <id>              # writes the agent into .cuma/config.toml
cuma agents show <id>             # one agent: capabilities, models, health
cuma models list                  # every model every agent exposes

cuma mcp list                     # configured MCP servers
cuma mcp tools                    # the tools they expose, allowlists applied
cuma mcp call <tool> --args '{"k":"v"}'

cuma skills search <query>        # find skills across configured registries
cuma skills inspect <id>          # fetch + verify a skill without installing
cuma skills install <id>          # verify, install, enable
cuma skills list
```

An MCP server is shared with agents only when you ask:

```toml
[mcp.git]
command = "uvx"
args = ["mcp-server-git"]
share_with_agents = true          # reaches ACP agents through `cuma mcp proxy`
allowed_tools = ["git_log"]       # empty means every tool
```

Long-term memory (handoffs included) is off by default and needs the external
`ai-memory` binary: `[memory] enabled = true`, then `cuma memory status` and
`cuma memory search <query>`.

## Troubleshooting

| Symptom | Where to look |
|---|---|
| `no agents were registered` | `cuma agents list`, `cuma doctor` — command missing on `PATH`, or a non-well-known agent without `command` |
| An agent is never selected | The `Rejected:` section of `cuma explain` — almost always a capability mismatch or an open breaker, not a low score |
| Agent keeps failing | `cuma agents show <id>` health, `-vv` logs, `RUST_LOG=cuma_protocol_acp=trace` |
| Routing decisions look wrong | `RUST_LOG=cuma_router=trace cuma explain "..."` |
| Config not being picked up | `cuma doctor` lists every layer read, in precedence order; unknown keys are rejected at startup |
| Sandbox won't launch | `cuma sandbox list`, `cuma sandbox probe <name>`; a shortfall is a doctor FAIL, not a warning |
| The agent's login doesn't work inside the sandbox | add the agent's config dir to its `state = [...]` and any needed vars to `env = [...]` |

## Honest limitations to know early

- Only `claude-code`/`claude`/`claude-agent` and `codex`/`openai-codex` are
  well-known names; everything else needs an explicit `command` or
  `cuma agents add`.
- When CUMA serves ACP, an editor's own `mcp_servers` are accepted but not
  forwarded to the agents behind it — only `[mcp.*]` servers marked
  `share_with_agents` reach them.
- A2A authentication is bearer tokens only, in both directions.
- `network_allowlist` is enforced only by ai-jail, microsandbox, wasmer and
  opensandbox; other runtimes confine the filesystem but report the
  allowlist as not enforced. `cuma sandbox list` shows which is which.

## What to read next

[README](README.md) · [Architecture](docs/ARCHITECTURE.md) ·
[Routing](docs/ROUTING.md) · [Orchestration](docs/ORCHESTRATION.md) ·
[Sandboxes](docs/SANDBOXES.md) · [Security](docs/SECURITY.md) ·
[Configuration reference](docs/CONFIGURATION.md) — every key, every
precedence rule · [Roadmap](docs/ROADMAP.md) — what is built and what is not.
