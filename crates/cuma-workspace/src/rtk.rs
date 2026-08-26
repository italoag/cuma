//! RTK integration: fewer tokens spent on command output.
//!
//! [RTK](https://github.com/rtk-ai/rtk) is a proxy that wraps development
//! commands and filters their output down before it reaches an agent's
//! context. A `cargo test` that prints 4,000 lines of passing tests costs real
//! money when all the agent needed was the three failures.
//!
//! Integration is deliberately shallow: RTK is a *command prefix*, not a
//! library, so CUMA detects it on `PATH` and wraps commands with it. Nothing
//! breaks when it is absent — the command runs unwrapped and the saving is
//! zero.
//!
//! ## Measuring the saving honestly
//!
//! Reporting "RTK saved 48k tokens" requires knowing what the output would
//! have been *without* it, which cannot be known without running the command
//! twice. So [`Rtk::record_saving`] takes both sizes when a caller genuinely
//! has them, and [`estimate_tokens`] is explicitly an estimate — it feeds the
//! usage tracker's estimated column, never the reported one.

use cuma_config::{RtkConfig, RtkMode};
use serde::{Deserialize, Serialize};

/// Commands whose output is worth filtering.
///
/// Chosen because each is both common in agent work and prone to producing far
/// more output than the agent needs.
const WRAPPED_COMMANDS: &[&str] = &[
    "cargo", "go", "npm", "pnpm", "yarn", "make", "pytest", "jest", "gradle", "mvn", "git", "grep",
    "rg", "find", "ls", "tree", "docker", "kubectl",
];

/// Characters per token, for estimating a saving.
///
/// The same rough average the context manager uses. Precise tokenization would
/// need a tokenizer per model, and a saving figure only needs to be
/// approximately right to be useful.
const CHARS_PER_TOKEN: usize = 4;

/// Whether RTK is in play.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RtkStatus {
    /// Turned off by configuration.
    Disabled,
    /// Found on `PATH` and in use.
    Active {
        /// The binary.
        binary: String,
    },
    /// Wanted but not installed.
    ///
    /// Under `auto` this is fine and commands run unwrapped. Under `always` it
    /// is a configuration error, because the operator asked for it explicitly.
    Missing {
        /// What was looked for.
        binary: String,
        /// Whether the operator required it.
        required: bool,
    },
}

impl RtkStatus {
    /// Whether commands are actually being wrapped.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    /// A line for `cuma doctor`.
    pub fn describe(&self) -> String {
        match self {
            Self::Disabled => "RTK: disabled".to_owned(),
            Self::Active { binary } => {
                format!("RTK: detected ({binary}); token optimization enabled")
            }
            Self::Missing {
                binary,
                required: false,
            } => format!("RTK: not found ({binary}); running without token optimization"),
            Self::Missing {
                binary,
                required: true,
            } => format!("RTK: REQUIRED but {binary} is not on PATH"),
        }
    }

    /// Whether this status should fail startup.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Missing { required: true, .. })
    }
}

/// Estimate the tokens a piece of text occupies.
///
/// An estimate, and labelled as one wherever it surfaces.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() / CHARS_PER_TOKEN) as u64
}

/// Wraps commands with RTK when it is available.
#[derive(Debug, Clone)]
pub struct Rtk {
    status: RtkStatus,
}

impl Rtk {
    /// Detect RTK under `config`.
    pub fn detect(config: &RtkConfig) -> Self {
        let binary = config.binary.clone().unwrap_or_else(|| "rtk".to_owned());

        let status = match config.enabled {
            RtkMode::Never => RtkStatus::Disabled,
            RtkMode::Auto | RtkMode::Always => {
                if which::which(&binary).is_ok() {
                    RtkStatus::Active { binary }
                } else {
                    RtkStatus::Missing {
                        binary,
                        required: config.enabled == RtkMode::Always,
                    }
                }
            }
        };

        if let RtkStatus::Missing { required: true, .. } = &status {
            tracing::error!("{}", status.describe());
        } else {
            tracing::debug!("{}", status.describe());
        }

        Self { status }
    }

    /// An RTK that wraps nothing, for tests.
    pub fn disabled() -> Self {
        Self {
            status: RtkStatus::Disabled,
        }
    }

    /// What RTK is doing.
    pub fn status(&self) -> &RtkStatus {
        &self.status
    }

    /// Whether `command` is one worth wrapping.
    ///
    /// Wrapping everything would put RTK in front of the agent's own binary,
    /// which is neither useful nor safe.
    pub fn should_wrap(command: &str) -> bool {
        let Some(binary) = command.split_whitespace().next() else {
            return false;
        };

        // Strip any path: `/usr/bin/cargo` is still cargo.
        let name = binary.rsplit(['/', '\\']).next().unwrap_or(binary);

        WRAPPED_COMMANDS.contains(&name)
    }

    /// Wrap `command` with RTK, if it is active and the command is one to wrap.
    ///
    /// Returns the command unchanged otherwise, so a caller runs the same
    /// string either way.
    pub fn wrap(&self, command: &str) -> String {
        let RtkStatus::Active { binary } = &self.status else {
            return command.to_owned();
        };

        if !Self::should_wrap(command) {
            return command.to_owned();
        }

        format!("{binary} {command}")
    }
}

/// A measured saving from one filtered command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saving {
    /// Tokens the raw output would have cost.
    pub raw_tokens: u64,
    /// Tokens the filtered output actually cost.
    pub filtered_tokens: u64,
}

impl Saving {
    /// Compute a saving from the two output sizes.
    pub fn between(raw: &str, filtered: &str) -> Self {
        Self {
            raw_tokens: estimate_tokens(raw),
            filtered_tokens: estimate_tokens(filtered),
        }
    }

    /// Tokens saved.
    ///
    /// Saturating: a filter that somehow produced *more* output saved nothing,
    /// and reporting a negative saving as a huge positive one via wraparound
    /// would be worse than reporting zero.
    pub fn tokens_saved(&self) -> u64 {
        self.raw_tokens.saturating_sub(self.filtered_tokens)
    }

    /// The saving as a fraction, or `None` when there was no raw output.
    pub fn ratio(&self) -> Option<f64> {
        if self.raw_tokens == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.tokens_saved() as f64 / self.raw_tokens as f64)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn active() -> Rtk {
        Rtk {
            status: RtkStatus::Active {
                binary: "rtk".into(),
            },
        }
    }

    #[test]
    fn rtk_disabled_leaves_commands_untouched() {
        let rtk = Rtk::detect(&RtkConfig {
            enabled: RtkMode::Never,
            binary: None,
        });

        assert_eq!(rtk.status(), &RtkStatus::Disabled);
        assert_eq!(rtk.wrap("cargo test"), "cargo test");
    }

    #[test]
    fn an_active_rtk_wraps_development_commands() {
        assert_eq!(active().wrap("cargo test"), "rtk cargo test");
        assert_eq!(active().wrap("git log --oneline"), "rtk git log --oneline");
    }

    #[test]
    fn commands_not_worth_filtering_are_left_alone() {
        // Wrapping everything would put RTK in front of the agent's own
        // binary, which is neither useful nor safe.
        for command in ["node server.js", "python main.py", "./my-agent --acp"] {
            assert_eq!(active().wrap(command), command, "{command:?}");
        }
    }

    #[test]
    fn a_full_path_is_still_recognised() {
        assert!(Rtk::should_wrap("/usr/local/bin/cargo build"));
        assert!(!Rtk::should_wrap("/usr/local/bin/something-else"));
    }

    #[test]
    fn an_empty_command_is_not_wrapped() {
        assert!(!Rtk::should_wrap(""));
        assert_eq!(active().wrap(""), "");
    }

    #[test]
    fn auto_mode_degrades_quietly_when_rtk_is_absent() {
        let rtk = Rtk::detect(&RtkConfig {
            enabled: RtkMode::Auto,
            binary: Some("definitely-not-rtk-7f3a".into()),
        });

        assert!(!rtk.status().is_active());
        assert!(!rtk.status().is_fatal(), "auto must not fail startup");
        assert_eq!(rtk.wrap("cargo test"), "cargo test");
    }

    #[test]
    fn always_mode_treats_a_missing_rtk_as_fatal() {
        // The operator asked for it explicitly; silently proceeding without it
        // would mean spending tokens they thought they were saving.
        let rtk = Rtk::detect(&RtkConfig {
            enabled: RtkMode::Always,
            binary: Some("definitely-not-rtk-7f3a".into()),
        });

        assert!(rtk.status().is_fatal());
        assert!(rtk.status().describe().contains("REQUIRED"));
    }

    #[test]
    fn every_status_describes_itself_for_doctor() {
        for status in [
            RtkStatus::Disabled,
            RtkStatus::Active {
                binary: "rtk".into(),
            },
            RtkStatus::Missing {
                binary: "rtk".into(),
                required: false,
            },
        ] {
            assert!(!status.describe().is_empty());
        }
    }

    // --- savings ----------------------------------------------------------

    #[test]
    fn a_saving_is_the_difference_between_raw_and_filtered_output() {
        let raw = "passing test\n".repeat(1000);
        let filtered = "3 failures:\n  test_a\n  test_b\n  test_c\n".to_owned();

        let saving = Saving::between(&raw, &filtered);

        assert!(
            saving.tokens_saved() > 2_000,
            "saved {}",
            saving.tokens_saved()
        );
        assert!(saving.ratio().unwrap() > 0.9);
    }

    #[test]
    fn a_filter_that_produced_more_output_reports_no_saving_rather_than_wrapping_around() {
        let saving = Saving {
            raw_tokens: 10,
            filtered_tokens: 100,
        };

        assert_eq!(saving.tokens_saved(), 0);
    }

    #[test]
    fn a_command_with_no_output_has_no_ratio_rather_than_dividing_by_zero() {
        assert_eq!(Saving::between("", "").ratio(), None);
    }

    #[test]
    fn token_estimation_scales_with_length() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens(&"x".repeat(400)), 100);
    }
}
