# Skills

A skill closes a capability gap: the planner needs something no registered agent
provides, so the harness looks for something that does.

This is also the most dangerous feature in the product — its purpose is to find
code the harness does not have, fetch it, and run it. The design rule is blunt:
**nothing is installed or executed until it has been validated, and validation
defaults to refusing.**

## The flow

```
capability gap
      ↓
  search registries
      ↓
  validate  ──────> refused, with a reason
      ↓
  policy check ───> needs approval
      ↓
  install (no code runs)
      ↓
  register
```

## Trust

Derived from evidence, then floored by the manifest's own claim. A manifest can
write `trust = "trusted"` in a file anyone can publish, so the claim is evidence,
not a decision.

| Evidence | Derived |
|---|---|
| Ships in the binary (`builtin:`) | `Trusted` |
| Signature **and** checksum published | `Verified` |
| Fetched over TLS, unsigned | `Community` |
| Anything else | `Untrusted` |

The final level is the **lower** of derived and claimed. A manifest can never
talk itself up; one that marks itself provisional is believed.

## Refusals

- Unrestricted permissions: `shell:unrestricted`, `filesystem:write:/`,
  `network:unrestricted`, `credentials`, `keychain`, `env:read:*`
- Scope-escape patterns: `..`, `$(`, backticks, control characters
- Non-TLS, non-local sources
- Missing name or source
- Unknown keys in a manifest — in a security-sensitive file, an unrecognized key
  is loud rather than ignored

## Policy

```toml
[skills]
enabled = true
auto_install = "trusted-only"   # never | trusted-only | verified
registries = ["builtin", "local"]
allow_creation = false
```

| Policy | Auto-installs |
|---|---|
| `never` | Nothing |
| `trusted-only` *(default)* | `Trusted` |
| `verified` | `Trusted`, `Verified` |

Anything above the bar returns `SkillOutcome::NeedsApproval` rather than being
installed silently.

## Built-in skills

| Skill | Capability | Permissions |
|---|---|---|
| `git-workflow` | `version_control` | `shell:run:git`, `filesystem:read:.` |
| `cargo-toolchain` | `testing`, `shell_execution` | `shell:run:cargo`, `filesystem:read:.` |
| `test-runner` | `testing` | `shell:run:make`, `shell:run:cargo` |
| `doc-search` | `research` | `network:read:docs.rs` |

## Manifest format

```toml
id = "rust-debug"
name = "Rust debugging"
description = "Interpret compiler and borrow-checker errors"
version = "1.0.0"
capabilities = ["debugging"]
permissions = ["filesystem:read:./src", "shell:run:cargo"]
checksum = "sha256:..."
signature = "ed25519:..."
```

A manifest read from disk is `Community` at best, whatever it claims — a
directory anyone can write to is not evidence of anything.

## Commands

```bash
cuma skills search rust
cuma skills inspect rust-debug     # what it declares, and what validation makes of it
cuma skills install rust-debug
cuma skills list
```

## Not yet built

- **Skill creation** — generating a skill that does not exist. The highest-risk
  feature in the brief, deliberately last.
- **Signature verification** — presence is checked; cryptographic validation is
  not. `Verified` currently means "claims integrity metadata", not "integrity
  proven".
- **Remote registries** — the `SkillRegistry` trait supports multiple backends;
  only built-in and local-directory registries exist.
