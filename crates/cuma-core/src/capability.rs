//! Capabilities: the vocabulary shared by tasks and agents.
//!
//! A [`Capability`] is deliberately coarse. It is *not* a model name and it is
//! *not* a protocol feature — it is "the kind of work this is". The router
//! matches what a [`crate::task::Task`] requires against what an
//! [`crate::agent::AgentDescriptor`] advertises, and neither side ever names
//! the other.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// A single unit of competence an agent may advertise or a task may require.
///
/// [`Capability::Custom`] is the escape hatch for capabilities discovered at
/// runtime — an A2A Agent Card or an ACP capability negotiation can surface
/// names this enum has never heard of, and dropping them on the floor would
/// silently degrade routing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Read and reason about an existing codebase.
    CodeComprehension,
    /// Write new code.
    CodeGeneration,
    /// Modify existing code in place.
    CodeEditing,
    /// Diagnose and fix defects.
    Debugging,
    /// Restructure code without changing behaviour.
    Refactoring,
    /// Write or repair tests.
    Testing,
    /// Run commands in a shell.
    ShellExecution,
    /// Read and write files in the workspace.
    FileSystem,
    /// Drive git.
    VersionControl,
    /// Look things up on the web or in documentation.
    Research,
    /// Produce or update prose documentation.
    Documentation,
    /// High-level design and architectural reasoning.
    Architecture,
    /// Review a diff.
    CodeReview,
    /// Long-horizon multi-step reasoning.
    Planning,
    /// Call tools / functions.
    ToolUse,
    /// Accept images as input.
    Vision,
    /// Emit schema-constrained output.
    StructuredOutput,
    /// A capability name discovered at runtime.
    Custom(String),
}

impl Capability {
    /// Parse a capability from a free-form string, falling back to
    /// [`Capability::Custom`] for anything unrecognized.
    ///
    /// Discovery surfaces (Agent Cards, ACP negotiation, config files) all go
    /// through here, so an unknown name degrades to an opaque-but-matchable
    /// capability instead of an error.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "code_comprehension" => Self::CodeComprehension,
            "code_generation" => Self::CodeGeneration,
            "code_editing" => Self::CodeEditing,
            "debugging" => Self::Debugging,
            "refactoring" => Self::Refactoring,
            "testing" => Self::Testing,
            "shell_execution" => Self::ShellExecution,
            "file_system" => Self::FileSystem,
            "version_control" => Self::VersionControl,
            "research" => Self::Research,
            "documentation" => Self::Documentation,
            "architecture" => Self::Architecture,
            "code_review" => Self::CodeReview,
            "planning" => Self::Planning,
            "tool_use" => Self::ToolUse,
            "vision" => Self::Vision,
            "structured_output" => Self::StructuredOutput,
            other => Self::Custom(other.to_owned()),
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Custom(name) => f.write_str(name),
            other => {
                // The serde representation is already snake_case; reuse it so
                // Display and the wire format can never drift apart.
                let json = serde_json::to_string(other).unwrap_or_default();
                f.write_str(json.trim_matches('"'))
            }
        }
    }
}

/// An unordered, deduplicated set of capabilities.
///
/// `BTreeSet` rather than `HashSet` so that serialized descriptors and routing
/// explanations are stable across runs — an explanation that reorders itself
/// between invocations is useless for debugging.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    /// An empty set.
    pub fn new() -> Self {
        Self(BTreeSet::new())
    }

    /// Add a capability, returning `self` for chaining.
    #[must_use]
    pub fn with(mut self, capability: Capability) -> Self {
        self.0.insert(capability);
        self
    }

    /// Insert a capability.
    pub fn insert(&mut self, capability: Capability) -> bool {
        self.0.insert(capability)
    }

    /// Whether this set advertises `capability`.
    pub fn contains(&self, capability: &Capability) -> bool {
        self.0.contains(capability)
    }

    /// Number of capabilities in the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate the capabilities in stable order.
    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.0.iter()
    }

    /// Score how well this set (an agent's) covers `required` (a task's).
    ///
    /// The score is the fraction of required capabilities that are present, so
    /// it is always in `[0.0, 1.0]`. A task that requires nothing is trivially
    /// satisfied by every agent and scores `1.0` — that is intentional, since
    /// the router still has cost, latency and health to discriminate on.
    pub fn match_against(&self, required: &CapabilitySet) -> CapabilityMatch {
        if required.is_empty() {
            return CapabilityMatch {
                score: 1.0,
                missing: Vec::new(),
            };
        }

        let missing: Vec<Capability> = required
            .iter()
            .filter(|c| !self.contains(c))
            .cloned()
            .collect();

        let matched = required.len() - missing.len();
        #[allow(clippy::cast_precision_loss)]
        let score = matched as f64 / required.len() as f64;

        CapabilityMatch { score, missing }
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// The result of matching an agent's capabilities against a task's requirements.
///
/// `missing` is carried alongside `score` because the router must be able to
/// *explain* a rejection, not just produce a number.
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityMatch {
    /// Fraction of required capabilities satisfied, in `[0.0, 1.0]`.
    pub score: f64,
    /// Required capabilities the agent does not advertise.
    pub missing: Vec<Capability>,
}

impl CapabilityMatch {
    /// Whether every required capability is present.
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn unknown_capability_names_survive_as_custom() {
        assert_eq!(
            Capability::parse("quantum-annealing"),
            Capability::Custom("quantum_annealing".into())
        );
    }

    #[test]
    fn known_names_parse_case_and_separator_insensitively() {
        assert_eq!(Capability::parse("Code-Editing"), Capability::CodeEditing);
        assert_eq!(Capability::parse("  testing "), Capability::Testing);
    }

    #[test]
    fn display_matches_serde_representation() {
        assert_eq!(Capability::CodeReview.to_string(), "code_review");
        assert_eq!(Capability::Custom("foo".into()).to_string(), "foo");
    }

    #[test]
    fn full_coverage_scores_one_and_reports_nothing_missing() {
        let agent = CapabilitySet::new()
            .with(Capability::Debugging)
            .with(Capability::Testing)
            .with(Capability::Vision);
        let required = CapabilitySet::new()
            .with(Capability::Debugging)
            .with(Capability::Testing);

        let m = agent.match_against(&required);
        assert!((m.score - 1.0).abs() < f64::EPSILON);
        assert!(m.is_complete());
    }

    #[test]
    fn partial_coverage_scores_proportionally_and_names_the_gap() {
        let agent = CapabilitySet::new().with(Capability::Debugging);
        let required = CapabilitySet::new()
            .with(Capability::Debugging)
            .with(Capability::Vision);

        let m = agent.match_against(&required);
        assert!((m.score - 0.5).abs() < f64::EPSILON);
        assert_eq!(m.missing, vec![Capability::Vision]);
    }

    #[test]
    fn a_task_requiring_nothing_is_satisfied_by_an_empty_agent() {
        let m = CapabilitySet::new().match_against(&CapabilitySet::new());
        assert!(m.is_complete());
        assert!((m.score - 1.0).abs() < f64::EPSILON);
    }
}
