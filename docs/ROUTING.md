# Routing

The component that justifies the product. Full rationale in
[ADR-006](adr/ADR-006-routing-strategy.md); this is the operator's guide.

## Never "the most powerful model"

It is wrong on cost, wrong on latency, and wrong on quality — the most capable
model in general is frequently not the best at a specific kind of work.

## Three stages

### 1. Filter

Hard constraints remove candidates entirely:

| Filter | Rejection reason |
|---|---|
| Disabled agent | `disabled by configuration` |
| `exclude_agents` / `exclude_models` | `excluded by router.exclude_*` |
| `pin_agent` / `pin_model` | `router.pin_agent is X` |
| Unhealthy | `health is Unavailable` |
| Open breaker | `circuit breaker open` |
| Missing capability | `missing required capabilities: X, Y` |
| Already failed this task | `already failed this task` |
| Over budget | `estimated $X exceeds $Y remaining budget` |
| Remote, under a privacy policy | `PrivacyFirst excludes remote A2A agents` |

**A filtered candidate can never be rescued by a high score.**

### 2. Score

```
score = quality·w_q + cost·w_c + latency·w_l + reliability·w_r + context·w_x
```

Every dimension is `[0,1]`, higher better.

| Dimension | Computed from |
|---|---|
| **quality** | capability coverage (70%) + model skill (30%); the skill blend shifts from coding toward reasoning as complexity rises |
| **cost** | `1 − blended_price/$30` per Mtok, output weighted 0.6 |
| **latency** | `1 − ms/120000`, live measurement preferred over advertised |
| **reliability** | historical success × health multiplier (healthy 1.0, degraded 0.6, rate-limited 0.3) |
| **context** | window headroom, saturating at two-thirds free |

**Unknown scores 0.5, never 1.0.** Rewarding an agent for disclosing nothing
would make silence the winning strategy.

### 3. Explain

```
Selected:
  Agent: claude-code
  Model: sonnet
Strategy: Balanced
Reasons:
  capability & quality     0.94  x0.30  =  0.282
  cost                     0.72  x0.20  =  0.144
  latency                  0.88  x0.10  =  0.088
  reliability              0.91  x0.30  =  0.273
  context fit              1.00  x0.10  =  0.100
  total                                    0.887
Alternatives:
  codex/gpt-x       0.821
  gemini/flash      0.714
Rejected:
  broken: circuit breaker open
  weak: missing required capabilities: architecture
```

Contributions are shown alongside raw scores, because a 0.9 on a dimension
weighted at 0.05 is nearly irrelevant and showing only the raw value hides that.

Scores are comparable *within one decision only*. They are not an absolute
rating of an agent.

## Strategies

| Strategy | quality | cost | latency | reliability | context |
|---|---|---|---|---|---|
| `balanced` *(default)* | 0.30 | 0.20 | 0.10 | 0.30 | 0.10 |
| `quality-first` | 0.55 | 0.05 | 0.05 | 0.25 | 0.10 |
| `cost-first` | 0.15 | 0.55 | 0.05 | 0.15 | 0.10 |
| `latency-first` | 0.15 | 0.10 | 0.50 | 0.20 | 0.05 |
| `local-first` | 0.25 | 0.15 | 0.15 | 0.35 | 0.10 |
| `privacy-first` | 0.25 | 0.15 | 0.15 | 0.35 | 0.10 |
| `manual` | — pinned agent only — | | | | |

`local-first` and `privacy-first` additionally *filter* remote agents.

## Operator controls

```toml
[router]
strategy = "balanced"
pin_agent = "claude-code"          # force one agent
pin_model = "sonnet"               # force one model
exclude_agents = ["expensive-one"]
exclude_models = ["deprecated-x"]
adaptive_weight = 0.3              # 0.0 disables learning entirely
adaptive_min_samples = 10          # evidence needed before history counts

[router.weights]                   # an explicit block overrides the preset
quality = 0.30
cost = 0.25
latency = 0.10
reliability = 0.25
context = 0.10
```

Per invocation:

```bash
cuma run "..." --strategy cost-first
cuma run "..." --agent codex --model gpt-x
cuma run "..." --max-cost 2.50
```

## Adaptive routing

History is bucketed by `(agent, model, task_type)`, because agent quality is not
one number:

```
Rust debugging     Codex 92%   Claude 96%   Gemini 73%
Documentation      Codex 71%   Claude 88%   Gemini 94%
```

The router will favour Claude for debugging and Gemini for documentation, even
if a generic benchmark ranks them otherwise.

Three guards against noise: a minimum sample count, a weight cap, and confidence
that ramps with evidence. Set `adaptive_weight = 0.0` to disable.

When history moves a score, the explanation says so.

## Debugging a decision

```bash
cuma explain "your goal"     # the plan, and how task 1 would route
cuma agents list             # health and capabilities
cuma usage                   # what history the router is working from
cuma doctor                  # what is unreachable and why
```

If an agent is never selected, check the `Rejected:` section first — it is
almost always a capability mismatch or an open breaker, not a low score.
