# ADR-006 — Filter, then weighted score, then explain

**Status:** Accepted

## Context

Routing is the component that justifies the product. Get it wrong and CUMA is an
expensive indirection layer.

The naive approach — always use the most capable model — is wrong on cost, wrong
on latency, and wrong on quality, because the most capable model in general is
often not the best at a specific kind of work.

## Decision

Three stages.

### 1. Filter — hard constraints

Pins, exclusions, health, circuit breakers, missing capabilities, budget
ceilings, and targets that already failed this task.

**A filtered candidate can never be rescued by a high score.** An agent that
cannot do the work does not become able to by being cheap, fast and reliable.
This is the single most important property of the design and it is pinned by a
test: `an_agent_lacking_a_required_capability_is_never_selected` gives the
incapable agent free pricing, 1ms latency and a perfect success record, and it
still loses.

### 2. Score — five weighted dimensions

Each normalized to `[0,1]`, **higher always better**, so the combination is a
plain dot product rather than a pile of sign conventions:

```
score = quality·w_q + cost·w_c + latency·w_l + reliability·w_r + context·w_x
```

- **quality** — capability coverage (70%) blended with model skill (30%), where
  the skill blend shifts from coding toward reasoning as task complexity rises
- **cost** — inverted price, output weighted above input because coding
  generates far more output than chat
- **latency** — inverted, preferring a live measurement over an advertised average
- **reliability** — historical success × a health multiplier
- **context** — window headroom, saturating at two-thirds free, because an agent
  needs room for its own tool output

Strategies (`balanced`, `quality-first`, `cost-first`, `latency-first`,
`local-first`, `privacy-first`, `manual`) are weight presets. `local-first` and
`privacy-first` are additionally *filters*: under a privacy policy a remote agent
is not slightly worse, it is disqualified.

### 3. Explain

Every decision carries its full breakdown, its runners-up, and why each rejected
candidate was rejected.

## Unknown metrics score neutrally, never optimally

An agent that reports no pricing scores 0.5 on cost, not 1.0. Otherwise silence
would be the winning strategy and every agent would learn to disclose nothing.

The explanation says so: *"pricing unknown; cost scored neutrally"*.

## Adaptive routing without machine learning

History is bucketed by `(agent, model, task_type)` — the insight being that
agent quality is not one number. An agent mediocre at documentation may be the
best available at Rust debugging, and a generic ranking would route around that
strength forever.

Three guards keep this from amplifying noise:

- Buckets below `adaptive_min_samples` are ignored. One lucky success is not
  evidence.
- `adaptive_weight` caps how far history can move the prior.
- Influence ramps with sample count, so a bucket that has just crossed the
  threshold does not immediately dominate.

**Deliberately not ML.** The product's selling point is explainable decisions;
introducing an opaque model into the component whose job is explaining itself
would be self-defeating. The blend point is where something more sophisticated
would slot in later.

## Consequences

**Good.** Weights are tunable and their effect is visible. Rejections are
auditable. The stages are independently testable.

**Costs.** Every agent+model pair is scored on every decision — fine at tens,
and `CapabilityIndex` exists to narrow the set at hundreds. The cost ceiling
(\$30/Mtok) and latency ceiling (120s) are calibration constants that will need
revisiting as pricing moves.

Scores are only comparable *within one decision*. They are not a quality rating
of an agent in the abstract.
