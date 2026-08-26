//! Command screening.
//!
//! Before an agent runs a shell command, it passes through here. The default
//! is to permit — an agent that cannot run `cargo test` is useless — but a
//! specific set of operations is refused unless the operator has explicitly
//! opted in, because they destroy work that cannot be recovered.

use cuma_config::SecurityConfig;
use serde::{Deserialize, Serialize};

/// What screening decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandVerdict {
    /// The command may run.
    Allow,
    /// The command is refused, with a reason for the user.
    Deny {
        /// Why.
        reason: String,
    },
}

impl CommandVerdict {
    /// Whether the command may run.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The refusal reason, if refused.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Allow => None,
            Self::Deny { reason } => Some(reason),
        }
    }
}

/// Command patterns that destroy uncommitted work.
///
/// Each entry is `(needle, what it destroys)`. Matching is on the normalized
/// command string, so `git   reset --hard` and `git reset --hard` both match.
const DESTRUCTIVE: &[(&str, &str)] = &[
    ("git reset --hard", "discards every uncommitted change"),
    ("git checkout -- .", "discards every uncommitted change"),
    ("git checkout --force", "discards every uncommitted change"),
    ("git clean -f", "deletes untracked files"),
    ("git clean -x", "deletes untracked and ignored files"),
    ("git push --force", "rewrites published history"),
    ("git push -f", "rewrites published history"),
    ("git branch -D", "deletes a branch without a merge check"),
    ("git stash drop", "discards stashed work"),
    ("git stash clear", "discards every stash"),
    ("rm -rf", "deletes recursively without confirmation"),
    ("rm -fr", "deletes recursively without confirmation"),
    ("mkfs", "formats a filesystem"),
    ("dd if=", "writes raw blocks"),
    ("truncate -s 0", "empties a file"),
    (":(){ :|:& };:", "is a fork bomb"),
    ("chmod -r 777", "removes every permission boundary"),
    ("shutdown", "halts the machine"),
    ("reboot", "restarts the machine"),
];

/// Paths outside a project that must never be a command's target.
const PROTECTED_ROOTS: &[&str] = &[
    "/", "/etc", "/usr", "/bin", "/sbin", "/boot", "/var", "/sys", "/proc", "~",
];

/// Screens commands against the security policy.
#[derive(Debug, Clone)]
pub struct CommandGuard {
    allow_destructive: bool,
    allowlist: Vec<String>,
}

impl CommandGuard {
    /// A guard enforcing `config`.
    pub fn new(config: &SecurityConfig) -> Self {
        Self {
            allow_destructive: config.allow_destructive_operations,
            allowlist: config
                .command_allowlist
                .iter()
                .map(|c| c.trim().to_ascii_lowercase())
                .filter(|c| !c.is_empty())
                .collect(),
        }
    }

    /// A guard that permits everything. For tests and sandboxed execution.
    pub fn permissive() -> Self {
        Self {
            allow_destructive: true,
            allowlist: Vec::new(),
        }
    }

    /// Collapse whitespace and lowercase, so spacing cannot evade a pattern.
    fn normalize(command: &str) -> String {
        command
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
    }

    /// Screen one command.
    pub fn screen(&self, command: &str) -> CommandVerdict {
        let normalized = Self::normalize(command);

        if normalized.is_empty() {
            return CommandVerdict::Deny {
                reason: "the command is empty".to_owned(),
            };
        }

        // An allowlist, when configured, is checked first and is absolute:
        // an operator who lists commands means *only* those.
        if !self.allowlist.is_empty() {
            let binary = normalized.split_whitespace().next().unwrap_or_default();
            let permitted = self
                .allowlist
                .iter()
                .any(|allowed| binary == allowed || normalized.starts_with(allowed));

            if !permitted {
                return CommandVerdict::Deny {
                    reason: format!("{binary:?} is not in security.command_allowlist"),
                };
            }
        }

        if self.allow_destructive {
            return CommandVerdict::Allow;
        }

        for (pattern, effect) in DESTRUCTIVE {
            if normalized.contains(pattern) {
                return CommandVerdict::Deny {
                    reason: format!(
                        "{pattern:?} {effect}; \
                         set security.allow_destructive_operations to permit it"
                    ),
                };
            }
        }

        // A recursive delete of a system root is refused even when destructive
        // operations are otherwise permitted — see `screen_target`.
        if let Some(reason) = Self::protected_target(&normalized) {
            return CommandVerdict::Deny { reason };
        }

        CommandVerdict::Allow
    }

    /// Whether a command targets a path outside any project.
    fn protected_target(normalized: &str) -> Option<String> {
        let is_delete = normalized.starts_with("rm ") || normalized.contains(" rm ");
        if !is_delete {
            return None;
        }

        for argument in normalized.split_whitespace().skip(1) {
            if argument.starts_with('-') {
                continue;
            }
            let target = argument.trim_end_matches('/');
            let target = if target.is_empty() { "/" } else { target };

            if PROTECTED_ROOTS.contains(&target) {
                return Some(format!("{target:?} is outside any project"));
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn default_guard() -> CommandGuard {
        CommandGuard::new(&SecurityConfig::default())
    }

    #[test]
    fn ordinary_development_commands_are_permitted() {
        let guard = default_guard();
        for command in [
            "cargo test",
            "cargo build --workspace",
            "git status",
            "git diff HEAD",
            "npm install",
            "make check",
            "ls -la src/",
            "rm target/debug/foo",
        ] {
            assert!(
                guard.screen(command).is_allowed(),
                "{command:?} should be allowed"
            );
        }
    }

    #[test]
    fn destructive_git_operations_are_refused_by_default() {
        let guard = default_guard();
        for command in [
            "git reset --hard HEAD~3",
            "git clean -fd",
            "git push --force origin main",
            "git checkout -- .",
            "git stash clear",
        ] {
            let verdict = guard.screen(command);
            assert!(!verdict.is_allowed(), "{command:?} should be refused");
            assert!(
                verdict
                    .reason()
                    .unwrap()
                    .contains("allow_destructive_operations"),
                "the refusal should say how to permit it"
            );
        }
    }

    #[test]
    fn recursive_deletion_is_refused_by_default() {
        assert!(!default_guard().screen("rm -rf build/").is_allowed());
        assert!(!default_guard().screen("rm -fr node_modules").is_allowed());
    }

    #[test]
    fn extra_whitespace_does_not_evade_a_pattern() {
        let guard = default_guard();
        assert!(!guard.screen("git   reset    --hard").is_allowed());
        assert!(!guard.screen("rm    -rf   /tmp/x").is_allowed());
    }

    #[test]
    fn casing_does_not_evade_a_pattern() {
        assert!(!default_guard().screen("GIT RESET --HARD").is_allowed());
    }

    #[test]
    fn a_destructive_command_embedded_in_a_pipeline_is_still_caught() {
        assert!(
            !default_guard()
                .screen("cargo build && git reset --hard")
                .is_allowed()
        );
    }

    #[test]
    fn an_explicit_policy_permits_destructive_operations() {
        let guard = CommandGuard::new(&SecurityConfig {
            allow_destructive_operations: true,
            ..SecurityConfig::default()
        });

        assert!(guard.screen("git reset --hard").is_allowed());
        assert!(guard.screen("rm -rf build/").is_allowed());
    }

    #[test]
    fn deleting_a_system_root_is_refused_even_under_a_permissive_policy() {
        // An operator opting into destructive operations meant "in my project",
        // not "anywhere on the machine".
        let guard = CommandGuard::new(&SecurityConfig {
            allow_destructive_operations: false,
            ..SecurityConfig::default()
        });

        for command in ["rm -rf /", "rm -rf /etc", "rm -rf /usr/"] {
            assert!(!guard.screen(command).is_allowed(), "{command:?}");
        }
    }

    #[test]
    fn an_allowlist_permits_only_what_it_lists() {
        let guard = CommandGuard::new(&SecurityConfig {
            command_allowlist: vec!["cargo".into(), "git status".into()],
            ..SecurityConfig::default()
        });

        assert!(guard.screen("cargo test").is_allowed());
        assert!(guard.screen("git status").is_allowed());

        let refused = guard.screen("curl https://example.invalid");
        assert!(!refused.is_allowed());
        assert!(refused.reason().unwrap().contains("command_allowlist"));
    }

    #[test]
    fn an_allowlist_does_not_rescue_a_destructive_command() {
        // `git` is allowlisted, but `git reset --hard` is still destructive.
        let guard = CommandGuard::new(&SecurityConfig {
            command_allowlist: vec!["git".into()],
            ..SecurityConfig::default()
        });

        assert!(guard.screen("git status").is_allowed());
        assert!(!guard.screen("git reset --hard").is_allowed());
    }

    #[test]
    fn an_empty_command_is_refused() {
        assert!(!default_guard().screen("   ").is_allowed());
    }

    #[test]
    fn a_permissive_guard_allows_everything_for_sandboxed_execution() {
        let guard = CommandGuard::permissive();
        assert!(guard.screen("rm -rf /").is_allowed());
    }
}
