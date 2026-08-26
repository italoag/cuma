# Configuration

## Precedence

Lowest first. Later layers override earlier ones **field by field**:

1. Built-in defaults
2. `~/.config/cuma/config.toml`
3. `./.cuma/config.toml`
4. `CUMA_*` environment variables
5. CLI flags

Field-level merging matters: a project file that sets `router.strategy` keeps
every global weight. Whole files replacing each other is the classic
configuration footgun.

The trade-off this makes: because TOML with `#[serde(default)]` cannot
distinguish "absent" from "set to the default value", a project file cannot
override a global `max_retries = 5` back down to the default `3` by writing `3`
explicitly. That ambiguity is far cheaper than the alternative.

**Unknown keys are rejected.** A silently ignored typo is more expensive to
debug than a startup error.

## Full reference

```toml
[router]
strategy = "balanced"              # balanced | quality-first | cost-first
                                   # latency-first | local-first | privacy-first | manual
pin_agent = "claude-code"
pin_model = "sonnet"
exclude_agents = []
exclude_models = []
adaptive_weight = 0.0              # 0.0 disables learning from history
adaptive_min_samples = 0           # evidence needed before a bucket counts

[router.weights]                   # an explicit block overrides the strategy preset
quality = 0.30
cost = 0.20
latency = 0.10
reliability = 0.30
context = 0.10

[agents.codex]
enabled = true
protocol = "acp"                   # acp | a2a | native
command = "npx -y @agentclientprotocol/codex-acp@latest"
capabilities = []                  # assumed when the agent advertises none
models = []                        # assumed when the agent enumerates none

[agents.architect]
protocol = "a2a"
endpoint = "https://architect.example/a2a"
auth_secret_ref = "CUMA_ARCHITECT_TOKEN"   # a handle, never a token

[memory]
enabled = false                    # off by default; an optional external binary
backend = "ai-memory-cli"          # ai-memory-cli | none
command = "ai-memory"
recall_limit = 8

[rtk]
enabled = "auto"                   # auto | always | never

[skills]
enabled = true
auto_install = "trusted-only"      # never | trusted-only | verified
registries = ["builtin", "local"]
allow_creation = false

[security]
sandbox = true
allow_destructive_operations = false
checkpoint_before_write = true
command_allowlist = []
network_allowlist = []

[limits]
max_parallel_tasks = 4
max_retries = 3
task_timeout_secs = 600
max_cost_usd = 10.0

[telemetry]
log_level = "info"                 # error | warn | info | debug | trace
json_logs = false
database_path = ".cuma/runtime.db"
```

## Environment variables

| Variable | Sets |
|---|---|
| `CUMA_ROUTER_STRATEGY` | `router.strategy` (and its weight preset) |
| `CUMA_ROUTER_PIN_AGENT` | `router.pin_agent` |
| `CUMA_ROUTER_PIN_MODEL` | `router.pin_model` |
| `CUMA_ROUTER_EXCLUDE_AGENTS` | `router.exclude_agents` (comma-separated) |
| `CUMA_MAX_PARALLEL_TASKS` | `limits.max_parallel_tasks` |
| `CUMA_MAX_RETRIES` | `limits.max_retries` |
| `CUMA_TASK_TIMEOUT_SECS` | `limits.task_timeout_secs` |
| `CUMA_MAX_COST_USD` | `limits.max_cost_usd` |
| `CUMA_LOG_LEVEL` | `telemetry.log_level` |
| `CUMA_JSON_LOGS` | `telemetry.json_logs` |
| `CUMA_DATABASE_PATH` | `telemetry.database_path` |
| `CUMA_MEMORY_ENABLED` | `memory.enabled` |
| `CUMA_RTK` | `rtk.enabled` |
| `CUMA_SANDBOX` | `security.sandbox` |
| `CUMA_SKILLS_AUTO_INSTALL` | `skills.auto_install` |

A variable that is set but unparseable is an **error**, not a warning. An
operator who wrote `CUMA_MAX_RETRIES=three` needs to know their retry limit is
not what they think it is.

## CLI flags

```bash
--workspace <DIR>        project root
--strategy <STRATEGY>    override the routing strategy
--agent <AGENT>          pin an agent
--model <MODEL>          pin a model
--max-cost <USD>         cap this invocation's spend
--json                   structured output on stdout
-v, -vv                  raise log verbosity
```

## Validation

Rejected at startup, not at first use:

- Negative, non-finite or all-zero router weights (all-zero would make routing
  arbitrary, which is worse than an error)
- `max_parallel_tasks = 0`
- `max_retries > 10`
- A non-positive `max_cost_usd`
- An unknown protocol on an agent
- Unknown keys anywhere

## Checking what is in effect

```bash
cuma doctor          # every contributing layer, and what is unreachable
cuma agents list     # what the router can actually see
```
