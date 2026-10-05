# Sandboxes

Every local agent CUMA launches runs inside a sandbox the operator chooses —
a process sandbox, a container, a microVM, a WebAssembly runtime, a Kubernetes
pod or a remote sandbox service. This page is the operator's guide, the
analysis behind each integration, and the plan. The decision and its costs are
in [ADR-018](adr/ADR-018-sandbox-providers.md).

## How it works

A **provider** turns "launch this agent in this workspace" into a *prefix* the
agent's own command runs under, plus, for sandboxes with a lifecycle, a
*session* that is finished once the agent exits:

```
open ──> [create sandbox] ──> [upload workspace] ──> prefix + agent command  (ACP over stdio)
                                                          │
finish <── [destroy] <── [three-way merge of the agent's work] <── agent exits
abort  <── [destroy]                       (timeout, cancellation: nothing is merged)
```

ACP is JSON-RPC over the agent's stdin and stdout, so a prefix is all the
protocol needs. Providers that only expose HTTP are reached through CUMA
itself: their prefix is `cuma sandbox exec --session <file> --`, and CUMA
bridges stdio to the remote process.

### Workspace: mounted or copied

| Mode | Meaning | Providers |
|---|---|---|
| **mounted** | the workspace is bind-mounted at the *same absolute path*, so the `cwd` ACP sends is valid inside and the agent's edits land directly | `native`, `docker`, `microsandbox`, `wasmer`, `command`, OpenSandbox with `mount_workspace`, agentOS |
| **copied** | the workspace is uploaded as a tar archive before the agent starts and the agent's work is brought back afterwards | `arcbox`, `kubernetes`, `e2b`, OpenSandbox without `mount_workspace`, Firecracker |

Bringing work back is a **file-level three-way merge** against what was
uploaded. A file the agent created, changed or deleted is applied when the
host copy is still what was uploaded. When the host copy changed too, that
file is a conflict: nothing is overwritten, the task fails, and the sandbox's
copy is kept under `.cuma/sandbox-results/<session>/` for a manual merge.
CUMA's own state (`.cuma/`) is never uploaded.

### Choosing a sandbox

```toml
[security]
sandbox = true                 # off: agents run unconfined
agent_sandbox = "auto"         # the default for every agent:
                               #   "auto"          ai-jail, else bubblewrap, sandbox-exec, firejail
                               #   "ai-jail" | "bubblewrap" | "sandbox-exec" | "firejail"
                               #   or the name of a [sandboxes.<name>] entry

[sandboxes.vm]                 # any number of named providers
kind = "microsandbox"
image = "node:22"

[agents.claude-code]
protocol = "acp"
sandbox = "vm"                 # this agent only
```

### Any agent, not only the ones a preset knows

A sandbox hides the home directory, so an agent needs to say where it keeps
its login and which variables it reads:

```toml
[agents.devin]
protocol = "acp"
command = "<the agent's ACP command>"
state = ["~/.config/devin", "~/.local/share/devin"]   # kept between runs: login, sessions
env = ["DEVIN_API_KEY"]                               # forwarded by name, never by value

[agents.openclaw]
protocol = "acp"
command = "<the agent's ACP command>"
state = ["~/.openclaw"]

[agents.hermes]
protocol = "acp"
command = "<the agent's ACP command>"
state = ["~/.hermes"]
env = ["OPENROUTER_API_KEY"]
```

The directories above are examples — use the ones each agent documents.
`state` is writable inside every mounted sandbox: under the native runtimes it
is added to the profile (ai-jail gets `--rw-map`), under containers and
microVMs the directories under your home are mounted below the guest's home
(`home`, default `/root`) and the rest at the same path. Copied sandboxes do
not receive `state`: give remote agents an API key through `env`.

An agent that keeps its login in the macOS keychain has no login inside a
Linux guest; it needs an API key forwarded through `env`.

### Credentials

No secret is ever placed on a command line. Depending on the provider a
variable listed in `env` (or `security.agent_env`) is:

| Provider | How it reaches the agent |
|---|---|
| `native` | kept in the environment; everything else is removed by name |
| `docker` | `-e NAME` — the engine copies the value from CUMA's environment |
| `microsandbox` | `--secret NAME@host` substitutes it outside the VM, for the listed hosts only; other names are not forwarded |
| `wasmer` | `--forward-host-env`, after every unlisted variable is removed with `env -u` |
| `arcbox`, `kubernetes` | written to a mode-0600 file inside the sandbox, sourced by the launch wrapper, destroyed with the sandbox |
| `e2b`, `opensandbox` | sent in the process-start request over the provider's API |
| `command`, `plugin` | the program decides; CUMA passes names, not values |

### Is it working?

```bash
cuma sandbox list               # every provider, its isolation and what it can enforce
cuma sandbox probe vm           # run something inside it, now
cuma doctor                     # which sandbox each agent uses, and what is not enforced
```

A provider is used only after something actually ran inside it: `probe` and
`doctor` run a trivial command through it, and at run time the agent's own
`initialize` is that test — an agent whose negotiation fails is registered
unavailable with the reason, never routed to.

## Provider reference

Fields not shown take the defaults listed. Unknown keys are rejected.

### `kind = "native"`

```toml
[sandboxes.jail]
kind = "native"
runtime = "auto"               # auto | ai-jail | bubblewrap | sandbox-exec | firejail
```

The profile in [SECURITY.md](SECURITY.md#agents-themselves). Naming a runtime
uses that one only — `runtime = "sandbox-exec"` is the way around ai-jail on a
Mac where it cannot execute mise-installed binaries.

### `kind = "docker"`

```toml
[sandboxes.container]
kind = "docker"
image = "node:22-bookworm"     # required: needs the agent's runtime (node, python, …)
program = "docker"             # docker | podman | nerdctl | a path
context = "arcbox"             # optional: a docker context — ArcBox, OrbStack, Rancher Desktop, Colima
runtime = "runsc"              # optional: gVisor (runsc), Kata (kata, kata-fc for Firecracker), …
network = "bridge"             # docker --network
home = "/root"                 # the guest home `state` is mounted under
user = "1000:1000"             # optional; default: the image's user (files it writes are owned by it)
memory = "4g"                  # optional
cpus = "2"                     # optional
entrypoint = ""                # optional: "" resets the image's entrypoint
extra_args = []                # appended to `docker run` before the image
```

Each launch is `docker run --rm -i --init` with a per-launch container name,
the workspace (and a worktree's git directory) mounted at the same path,
`--security-opt no-new-privileges` and `--cap-drop ALL`. The container is
removed by name when the session ends, so a killed client cannot leave it
running. Exercised live with Rancher Desktop's engine on macOS: an ACP agent
completed a turn through `cuma run`, wrote into the mounted workspace, and its
container was gone afterwards (`cargo test -p cuma-sandbox --test
live_docker`, with `CUMA_LIVE_DOCKER_IMAGE` set).

### Images, and the agent's own command

In a container or a virtual machine the agent's command is the guest's: `sh`
or `node` means the guest's, so the image must contain the agent's runtime.
Paths the command names are mounted read-only, except system directories
(`/bin`, `/usr`, `/lib`, `/etc`, …), which the guest has its own of — a host
binary mounted there could not even run.

Each engine pulls what `image` names; CUMA pulls nothing itself. Behind a
registry restriction — Docker Hub and Quay blocked, `ghcr.io` allowed, say —
point `image` at a mirror on a registry you can reach, such as
`ghcr.io/<org>/mirror/node:22`.

### `kind = "microsandbox"`

```toml
[sandboxes.vm]
kind = "microsandbox"
image = "node:22"              # required
program = "msb"
cpus = 2                       # optional
memory = "2G"                  # optional
home = "/root"
[sandboxes.vm.secrets]         # NAME = hosts the real value may be sent to
ANTHROPIC_API_KEY = ["api.anthropic.com"]
[sandboxes.vm.env]             # non-secret values, written here
NODE_OPTIONS = "--max-old-space-size=2048"
```

### `kind = "arcbox"`

```toml
[sandboxes.mac]
kind = "arcbox"
image = "node:22"              # exactly one of image, template, dockerfile
program = "abctl"
cpus = 2                       # optional
memory_mib = 2048              # optional
ttl_secs = 3600                # hard lifetime: a sandbox CUMA could not destroy still dies
user = "node"                  # optional: the guest user the agent runs as
```

### `kind = "kubernetes"`

```toml
[sandboxes.cluster]
kind = "kubernetes"
image = "node:22"              # exactly one of image (a Sandbox) or warm_pool (a SandboxClaim)
program = "kubectl"
context = "kind-agents"        # optional
namespace = "default"
container = "agent"
runtime_class = "gvisor"       # optional: gvisor, kata, kata-fc (Firecracker), …
service_account = "agent"      # optional
ready_timeout_secs = 300
lifetime_secs = 3600           # spec.shutdownTime: the controller reaps what CUMA could not
```

### `kind = "e2b"`

```toml
[sandboxes.cube]
kind = "e2b"
api_url = "http://cube.internal:3000"   # CubeSandbox's CubeAPI, or https://api.e2b.app
domain = "cube.internal"                # where <port>-<sandbox>.<domain> reaches envd
template = "coding-agent"               # required: a template with the agent's runtime
api_key_ref = "E2B_API_KEY"             # a handle: the variable holding the key
envd_scheme = "https"                   # http for a deployment without TLS
envd_port = 49983
timeout_secs = 3600
user = "user"
internet = true                         # allow_internet_access
```

### `kind = "opensandbox"`

```toml
[sandboxes.osb]
kind = "opensandbox"
server_url = "http://localhost:8080"    # required: the lifecycle API
api_key_ref = "OPEN_SANDBOX_API_KEY"    # optional handle
image = "node:22"                       # required; must contain node, for the stdio tunnel
cpu = "1"
memory = "2Gi"
mount_workspace = false                 # true: host-path volume (Docker runtime); false: copied
timeout_secs = 3600
tunnel_port = 7681
```

### `kind = "wasmer"`

```toml
[sandboxes.wasm]
kind = "wasmer"
program = "wasmer"
extra_args = []

[agents.wasm-agent]
protocol = "acp"
sandbox = "wasm"
command = "my-org/my-agent -- --acp"    # a Wasm package or .wasm file, then `--`, then its arguments
```

### `kind = "command"`

```toml
[sandboxes.custom]
kind = "command"
prefix = ["my-sandbox", "run", "--mount", "{workspace}:{workspace}", "--"]
probe = ["my-sandbox", "version"]       # optional; default: the prefix running `true`
setup = []                              # optional, before each launch; {id} is a fresh session id
teardown = []                           # optional, after each launch
isolation = "process"                   # process | container | microvm | wasm | remote (reported)
```

Placeholders: `{workspace}`, `{id}`, `{home}`. A command sandbox is reported
as not enforcing a network allowlist: CUMA cannot verify that it does.

### `kind = "plugin"`

```toml
[sandboxes.aos]
kind = "plugin"
program = "/opt/cuma/plugins/cuma-sandbox-agentos.mjs"
timeout_secs = 120
[sandboxes.aos.options]                 # handed to the plugin as JSON
memory_mb = 512
```

## Writing a plugin

A plugin is any executable. CUMA runs it with one argument — the operation —
writes a JSON request to its stdin, and reads a JSON reply from its stdout.
Exit status `0` means success; stderr is shown to the operator on failure.

| Operation | Request | Reply |
|---|---|---|
| `probe` | `{"options": {…}}` | `{"isolation": "microvm", "workspace": "mounted", "network_allowlist": false}` |
| `open` | `{"workspace", "purpose": "negotiate"\|"execute", "keep_env": [names], "readable": [paths], "state": [paths], "allowed_hosts": [hosts], "options": {…}}` | `{"prefix": [words], "session": "<opaque>"}` |
| `close` | `{"session", "collect": true\|false}` | `{}`, or `{"result_dir": "<path>"}` for a copied workspace |
| `release` | `{"session"}` | `{}` — after the merge: remove the copy `close` returned |
| `abort` | `{"session"}` | `{}` |

The agent is launched as `prefix + agent command`. A plugin that bridges stdio
itself returns a prefix pointing back at itself (`["/path/plugin", "exec",
"<session>", "--"]`). A plugin with a copied workspace says so from `probe`
and returns, from `close`, a directory holding the sandbox's final copy; CUMA
records the workspace before `open`, performs the three-way merge, and then
calls `release` — it never deletes a path a plugin named. Every plugin gets
the same conflict handling. Each operation is bounded by `timeout_secs`.

Two reference plugins ship in [`plugins/sandbox/`](../plugins/sandbox/):
`agentos` (Node.js) and `firecracker` (Python 3, standard library only, with
its POSIX-shell guest init).

## The eight sandboxes

| Sandbox | Isolation | Runs on | CUMA drives it through | Workspace | Host allowlist | Secrets stay outside |
|---|---|---|---|---|---|---|
| OpenSandbox | container or Firecracker microVM | Docker or Kubernetes, self-hosted | lifecycle and execd REST APIs, stdio tunnel | mounted or copied | yes (egress policy) | no |
| CubeSandbox | KVM microVM | x86-64 Linux with KVM, self-hosted | E2B REST and envd Connect APIs | copied | no (CubeEgress, server-side) | no |
| microsandbox | libkrun microVM | macOS (Apple Silicon), Linux (KVM), Windows (WHP) | `msb` CLI | mounted | yes | yes |
| agentOS | V8 isolates and WebAssembly, in-process | anywhere Node.js runs | reference plugin over its Node.js API | mounted | no | no |
| ArcBox | Firecracker microVM nested in a VZ VM | macOS 15+, Apple M3 or newer | `abctl` CLI | copied | no | no |
| Kubernetes agent-sandbox | pod under gVisor, Kata or runc | any cluster with the controller | `kubectl` | copied | no (NetworkPolicy, cluster-side) | no |
| Firecracker | KVM microVM | Linux with KVM | reference plugin: VM config, serial console, ext4 workspace image | copied | no | no |
| Wasmer | WebAssembly (WASI/WASIX) | anywhere | `wasmer run` | mounted | yes (DNS rules) | no |

### OpenSandbox

[opensandbox-group/OpenSandbox](https://github.com/opensandbox-group/OpenSandbox):
a self-hosted sandbox platform. A server exposes a lifecycle API
(`/v1/sandboxes`, `OPEN-SANDBOX-API-KEY`) over a Docker runtime locally or a
Kubernetes runtime (including Firecracker "fast sandboxes"); each sandbox runs
`execd`, which executes commands (output streamed as server-sent events),
moves files, and exposes ports through `/sandboxes/{id}/endpoints/{port}`.

- **Mechanism** — `open` creates a sandbox from `image` with a keep-alive
  entrypoint and waits for it; the agent is started by the bridge as a
  background `execd` command wrapped in a small Node.js tunnel
  (`cuma-tunnel.mjs`, uploaded through `execd`), and the bridge relays stdio
  over the tunnel's endpoint: output as server-sent events, input as ordered
  POSTs. `finish` deletes the sandbox.
- **Workspace** — mounted through a host-path volume when `mount_workspace =
  true` (runtimes that allow host mounts, such as the Docker runtime); copied
  through `execd`'s file API otherwise.
- **Network** — `security.network_allowlist` becomes the sandbox's egress
  policy (deny by default, allow the listed hosts).
- **Credentials** — names in `env` are sent as the command's environment over
  the API. OpenSandbox's credential vault is configured server-side.
- **Requirements** — a running server; Node.js inside the image (OpenSandbox's
  `execd` has no stdin, so the tunnel is how an ACP agent gets one).
- **Status** — built in. Requests, tunnel protocol and copy-back are tested
  against a local stand-in for the server; `live_opensandbox` runs against a
  real `opensandbox-server` (1.1.0, Docker runtime) in the Sandboxes
  pipeline, not yet run.

### CubeSandbox

[TencentCloud/CubeSandbox](https://github.com/TencentCloud/CubeSandbox): a
KVM microVM sandbox service (RustVMM) with an E2B-compatible API — CubeAPI for
the lifecycle, CubeProxy routing `<port>-<sandbox>.<domain>` to each sandbox's
`envd`. The same provider drives E2B itself.

- **Mechanism** — `kind = "e2b"`. `open` creates a sandbox from `template`
  (`POST /sandboxes`), uploads the workspace archive through `envd`'s
  `/files` endpoint and unpacks it; the bridge starts the agent with `envd`'s
  Connect API (`process.Process/Start`, a server stream of stdout and exit
  events) and feeds stdin with `SendInput` and `CloseStdin`. `finish` archives
  the workspace, downloads it, merges, and kills the sandbox
  (`DELETE /sandboxes/{id}`).
- **Workspace** — copied.
- **Network** — `internet` maps to `allow_internet_access`. Host allowlists
  are CubeEgress policies, configured on the Cube side; CUMA reports the
  allowlist as not enforced.
- **Credentials** — the API key is a handle (`api_key_ref`); agent variables
  are sent in the process-start request. `envd`'s access token is kept in the
  session file (mode 0600), never on a command line.
- **Requirements** — a Cube deployment (x86-64 Linux with KVM) and a template
  containing the agent's runtime, or an E2B account.
- **Status** — built in. Requests and the Connect framing are tested against a
  local stand-in for CubeAPI and `envd`; `live_e2b` runs against a real
  deployment from the pipeline's self-hosted `cubesandbox` job (a deployment
  needs x86-64 KVM), not yet run.

### microsandbox

[superradcompany/microsandbox](https://github.com/superradcompany/microsandbox):
local libkrun microVMs from OCI images, driven by the `msb` CLI, with host
allowlists and secrets that never enter the VM. Its documentation names ACP
agents as the use of `msb exec --stream`.

- **Mechanism** — `msb create <image> --name <session>` with the mounts,
  network rules and secrets; prefix `msb exec --stream <session> --`; `msb rm
  --force <session>` when the session ends.
- **Workspace** — mounted (`-v workspace:workspace`, `-w workspace`), plus the
  agent's `state` under `home`.
- **Network** — enforced: a non-empty allowlist becomes `--net-default deny
  --net-rule allow@host,…`.
- **Credentials** — `secrets` become `--secret NAME@host,…`: `msb` reads the
  variable from CUMA's environment and the guest only sees a placeholder that
  is substituted for the listed hosts.
- **Requirements** — `msb`; Apple Silicon macOS, Linux with KVM, or Windows
  with WHP.
- **Status** — built in, and exercised live on macOS (Apple M3 Pro, `msb`
  0.7.6): an ACP agent completed a turn in a microVM through `cuma run`, wrote
  into the workspace mounted at its own path, and the VM was removed
  afterwards (`cargo test -p cuma-sandbox --test live_microsandbox`, with
  `CUMA_LIVE_MICROSANDBOX_IMAGE` set). Every flag used was checked against
  `msb` 0.7.6's own help.

### agentOS

[rivet-dev/agentos](https://github.com/rivet-dev/agentos): an operating system
as a Node.js library — a Linux-like kernel whose processes are V8 isolates
(JavaScript) and WebAssembly (shell and coreutils), with mountable
filesystems.

- **Mechanism** — reference plugin
  [`plugins/sandbox/agentos`](../plugins/sandbox/agentos/): `exec` boots a VM
  with `AgentOs.create`, mounts the workspace and the agent's `state` writable
  at their own paths (`hostDirMount(…, { readOnly: false })` — mounts are
  read-only by default), runs the guest as your uid/gid so it may write what
  you may, spawns the agent with `vm.process.spawn`, relays stdin through
  `writeStdin`/`closeStdin` in order, and exits with the agent's exit code.
  Directories of files the agent's command names are mounted read-only, never
  the home directory or above it.
- **Workspace** — mounted.
- **Network** — agentOS denies external network by default; the plugin opens
  it (agents need their model APIs), and `create.permissions` can narrow it.
  CUMA reports the allowlist as not enforced.
- **Credentials** — names in `env` are passed to the guest process.
- **Requirements** — Node.js 22 and `@rivet-dev/agentos-core`, installed next
  to the plugin or wherever `options.module` (a `node_modules` directory)
  points — a global bun install, say. Guest code is JavaScript or WebAssembly
  only: agents shipping native binaries do not run, pure-JavaScript ones do.
  The guest has `sh`, coreutils and `sed`.
- **Status** — exercised live on macOS (Apple M3 Pro, agentos-core 0.2.22): an
  ACP agent completed a turn in an agentOS VM through `cuma run`, writing into
  the mounted workspace as the user (`cargo test -p cuma-sandbox --test
  live_agentos`, with `CUMA_LIVE_AGENTOS_MODULES` set). A launch takes tens of
  seconds — two VMs boot per task, one to negotiate and one to work.

### ArcBox

[arcboxlabs/arcbox](https://github.com/arcboxlabs/arcbox): a macOS container
and VM runtime. Two of its tiers matter here: a drop-in Docker engine, and
disposable Firecracker microVMs nested in its VM for agents.

- **Mechanism** — its Docker engine is `kind = "docker"` with `context =
  "arcbox"`. Its sandboxes are `kind = "arcbox"`: `abctl sandbox create --id
  <session>`, the archive copied in with `abctl sandbox cp` and unpacked with
  `abctl sandbox run`, prefix `abctl sandbox exec <session> --` with a small
  `sh` wrapper that enters the workspace and loads the environment file, and
  `abctl sandbox rm` at the end.
- **Workspace** — copied: sandbox V1 rejects mounts, and transfers are limited
  to 256 MiB per file, so the archive must fit.
- **Network** — enabled or none; no host allowlist.
- **Credentials** — an environment file copied in (mode 0600).
- **Requirements** — `abctl`; sandboxes need Apple Silicon M3 or newer and
  macOS 15 or later.
- **Status** — built in, unit-tested against the documented CLI;
  `live_arcbox` runs from the pipeline's self-hosted `arcbox` job (hosted macOS
  runners have no nested virtualization), not yet run.

### Kubernetes agent-sandbox

[kubernetes-sigs/agent-sandbox](https://github.com/kubernetes-sigs/agent-sandbox):
a `Sandbox` custom resource (`agents.x-k8s.io/v1beta1`) — a single stateful
pod with a stable identity, isolated by the runtime class (gVisor, Kata) —
and extensions for templates, warm pools and claims.

- **Mechanism** — `kubectl apply` of a `Sandbox` built from `image` (or a
  `SandboxClaim` on `warm_pool`), `kubectl wait --for=condition=Ready`, the
  archive streamed in through `kubectl exec -i … tar -x`, prefix `kubectl exec
  -i <pod> -c <container> --` with the `sh` wrapper, the archive streamed back,
  `kubectl delete`. `lifetime_secs` sets `shutdownTime`, so the controller
  reaps a sandbox CUMA could not.
- **Workspace** — copied.
- **Network** — NetworkPolicy is the cluster's; not enforced by CUMA.
- **Credentials** — an environment file inside the pod, not a Kubernetes
  Secret, so tokens are never stored in etcd.
- **Requirements** — the controller installed, `kubectl`, and RBAC to create
  sandboxes and exec into pods.
- **Status** — built in, unit-tested against agent-sandbox v1.0.5's CRDs
  (`v1beta1`); `live_kubernetes` runs on a kind cluster with the controller in
  the Sandboxes pipeline, not yet run.

### Firecracker

[firecracker-microvm/firecracker](https://github.com/firecracker-microvm/firecracker):
the VMM itself. It boots a kernel and root filesystem in milliseconds and
offers block devices, a TAP network interface, vsock and a serial console —
and no shared filesystem.

- **Mechanism** — reference plugin
  [`plugins/sandbox/firecracker`](../plugins/sandbox/firecracker/) (Python 3,
  standard library only): `open` builds an ext4 image of the workspace
  (`mke2fs -d`, no journal) and writes a VM configuration; `exec` boots
  `firecracker --no-api --config-file` with the agent's command in the kernel
  command line, and the guest init `cuma-init` mounts the workspace `sync` at
  its own path, puts the serial console in raw mode and runs the agent on it —
  so the agent's stdio *is* Firecracker's; `close` copies the work out of the
  image with `debugfs` for CUMA to merge.
- **Workspace** — copied (an ext4 image).
- **Network** — a TAP device the operator prepares; no allowlist.
- **Credentials** — names in `env` are written to a private file in the image,
  beside (not inside) the workspace; `cuma-init` loads it and deletes it before
  the agent starts.
- **Requirements** — Linux with KVM, `firecracker`, a kernel image, a root
  filesystem with the agent's runtime and `/sbin/cuma-init`, e2fsprogs, and a
  TAP device.
- **Firecracker elsewhere** — ArcBox sandboxes and OpenSandbox fast sandboxes
  are Firecracker microVMs, and Kata Containers can use Firecracker as its
  hypervisor: `runtime = "kata-fc"` (docker) or `runtime_class = "kata-fc"`
  (Kubernetes).
- **Status** — reference plugin. Its relay is tested against a stand-in VM
  (`firecracker_relay`): the boot is discarded, nothing is sent before
  `cuma-init` is ready, a whole turn crosses the console, and the end of stdin
  stops the VM. `live_firecracker` boots real microVMs in the Sandboxes
  pipeline (Linux, KVM), not yet run.

### Wasmer

[wasmerio/wasmer](https://github.com/wasmerio/wasmer): a WebAssembly runtime.
Guests see no file or network the host did not grant.

- **Mechanism** — prefix `wasmer run --volume workspace:workspace --cwd
  workspace --forward-host-env [--net…]`, behind `env -u` for every variable
  the agent must not see. The agent's command is a package or `.wasm` file,
  then `--`, then its arguments: `wasmer/bash -- /path/to/agent.sh`.
  `--volume` takes directories only and has no read-only form, so a file the
  command names brings its directory, writable — never the home directory or
  above it.
- **Workspace** — mounted.
- **Network** — enforced: an allowlist becomes `--net=dns:allow=<host>:*,…`;
  with none, `--net` grants the host network.
- **Credentials** — forwarded by name through the filtered environment.
- **Requirements** — `wasmer` 7 or later (the installer puts it in
  `~/.wasmer/bin`: set `program` if that is not on CUMA's `PATH`), and an
  agent compiled to WASI/WASIX or run by one, such as `wasmer/bash`.
- **Status** — exercised live on macOS (Apple M3 Pro, `wasmer` 7.5.0): an ACP
  agent run by `wasmer/bash` completed a turn through `cuma run`, writing into
  the mapped workspace (`cargo test -p cuma-sandbox --test live_wasmer`, with
  `CUMA_LIVE_WASMER_PACKAGE` set). The allowlist's `--net=dns:allow` rules are
  tested against the documented syntax, not yet against live traffic.

## Plan

### Phase 1 — the provider model *(done)*

- `SandboxLaunch` and `SandboxSession` in `cuma_core::ports`; the ACP adapter
  opens a launch per negotiation and per execution, finishes it after the
  turn, and aborts it when the turn is abandoned.
- `cuma-sandbox`: the provider trait, the registry that resolves
  `security.agent_sandbox` and `[agents.<id>] sandbox`, the three-way merge,
  the stdio bridge, and the `native`, `docker`, `command` and `plugin` kinds.
- Configuration: `[sandboxes.<name>]`, `security.agent_sandbox`, and agents'
  `sandbox`, `state` and `env`, validated at startup.
- `cuma sandbox list | probe`, and `cuma doctor` reporting each agent's
  sandbox and what it cannot enforce.
- **Accepted when** every kind parses and validates, the adapter finishes or
  aborts every launch, and an ACP agent completes a turn in a real container
  with the container removed afterwards.

### Phase 2 — the eight sandboxes *(done, pending live runs)*

- Built in: `microsandbox`, `arcbox`, `kubernetes`, `e2b` (CubeSandbox),
  `opensandbox`, `wasmer`.
- Reference plugins: `agentos`, `firecracker` (with `cuma-init`).
- **Accepted when** each provider's commands or requests are tested against
  its documented interface, copied workspaces round-trip through the merge,
  and HTTP providers are exercised against local stand-ins of their APIs.

### Phase 3 — live verification *(in progress)*

One opt-in test per provider, `crates/cuma-sandbox/tests/live_<kind>.rs`,
running the shell ACP fixture (`tests/fixtures/acp_agent.sh`, POSIX `sh` and
`sed` only) for a whole turn, checking the work lands in the workspace —
only when the launch finishes, for copied sandboxes — and that the sandbox is
gone afterwards. Each passes without doing anything when its variables are
unset, so `cargo test` stays hermetic.

The [Sandboxes pipeline](../.github/workflows/sandboxes.yml) runs all of them
on pull requests that touch the sandbox code, and by hand: each job installs
or builds its sandbox — Docker, microsandbox and Firecracker with KVM, a kind
cluster with agent-sandbox, an `opensandbox-server` on the runner's Docker,
Wasmer, agentOS — and the image the test runs in
(`ci/sandboxes/images/sandbox-test`). ArcBox and CubeSandbox need
self-hosted runners; see [DEVELOPMENT.md](DEVELOPMENT.md#live-sandbox-tests).

| Environment | Providers | State |
|---|---|---|
| macOS, Apple M3 Pro | `docker` (Rancher Desktop), `microsandbox`, `wasmer`, agentOS (plugin) | **done**: the live tests, and `cuma run` end to end for each |
| hosted runners (`ubuntu-latest`, KVM) | docker, microsandbox, wasmer, agentOS, kubernetes (kind + agent-sandbox), opensandbox, Firecracker (plugin) | pipeline ready; not yet run on GitHub |
| self-hosted macOS, Apple Silicon | ArcBox | pipeline job ready, enabled by a repository variable |
| self-hosted x86-64 Linux with KVM | CubeSandbox | pipeline job ready, enabled by a repository variable |

**Accepted when** each test passes and the provider's status here changes from
"not yet exercised" to the environment it was verified on.

The [Sandbox images pipeline](../.github/workflows/sandbox-images.yml)
publishes `ghcr.io/italoag/cuma-sandbox-test` and `ghcr.io/italoag/cuma-agent-node`
(a starting image for real agents: Node.js, git and common tools) for amd64
and arm64, and mirrors the third-party images the sandboxes need
(`ci/sandboxes/mirror.txt`) as `ghcr.io/italoag/cuma-*`.

### Phase 4 — depth *(later)*

- **Warm starts**: microsandbox snapshots, ArcBox checkpoints, agent-sandbox
  warm pools and Cube snapshots, so a launch restores instead of booting.
- **Session reuse**: one sandbox per CUMA session rather than per attempt,
  with the merge run per task.
- **Delta transfers** for copied workspaces, instead of whole archives.
- **Secret substitution everywhere**: the microsandbox model (placeholder in
  the guest, real value at the egress) through CubeEgress, OpenSandbox's
  credential vault, and an egress proxy CUMA runs for the others.
- **Risk-aware routing**: let a task's `Risk` require a minimum isolation
  (for example, `Medium` and above only under a microVM), as a router filter
  with its own rejection reason.
