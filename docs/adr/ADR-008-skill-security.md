# ADR-008 — Trust derived from evidence, never from a claim

**Status:** Accepted

## Context

The skill system is the most dangerous thing in the product. Its purpose is to
find code the harness does not have, fetch it from somewhere, and run it. Done
carelessly, it is a remote code execution feature with a nice name.

## Decision

Nothing is installed or executed until it has been validated, and validation
defaults to refusing.

### Trust is derived, then floored by the claim

A manifest can write `trust = "trusted"` in a file anyone can publish. So the
claim is *evidence*, not a decision:

| Evidence | Derived trust |
|---|---|
| Ships in the binary (`builtin:`) | `Trusted` |
| Signature **and** checksum published | `Verified` |
| Fetched over TLS, unsigned | `Community` |
| Anything else | `Untrusted` |

The final level is the **lower** of derived and claimed. A manifest can never
talk itself up; a manifest that marks itself provisional is believed.

Pinned by `a_manifest_cannot_talk_itself_up`.

### Refusals

- Unrestricted permissions: `shell:unrestricted`, `filesystem:write:/`,
  `network:unrestricted`, `credentials`, `keychain`, `env:read:*`
- Scope-escape patterns in a permission: `..`, `$(`, backticks, control characters
- Non-TLS, non-local sources
- Missing name or source
- Unknown keys in a manifest — in a security-sensitive file, an unrecognized key
  is loud rather than ignored

### Auto-install defaults to `trusted-only`

```toml
[skills]
auto_install = "trusted-only"   # never | trusted-only | verified
allow_creation = false
```

`SkillOutcome::NeedsApproval` is returned rather than silently installing.
Generating new code unprompted is the highest-risk operation available, so it is
off by default.

### Installation never runs skill code

Validation is a document inspection. Anything that would execute belongs behind
the sandbox, not in the install path.

## Consequences

**Good.** A hostile manifest cannot escalate itself. Every refusal explains
itself. Defaults are safe without configuration.

**Costs.** Most community skills are unsigned, so most will need explicit
approval under the default policy. That is the intended trade. Signature
*verification* is not yet implemented — presence is checked, cryptographic
validation is not, which is why `Verified` currently means "claims integrity
metadata" rather than "integrity proven". This is a known gap, not a claim.
