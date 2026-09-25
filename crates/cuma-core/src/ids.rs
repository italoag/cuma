//! Correlatable identifiers.
//!
//! Every identifier is a newtype over a `String` rather than a bare `String`.
//! This is deliberate: routing, persistence and telemetry all pass identifiers
//! around, and the type system should refuse to let an `AgentId` be used where
//! a `ModelId` is expected.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap an existing string.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Generate a fresh identifier with this type's conventional prefix.
            pub fn generate() -> Self {
                Self(format!("{}_{}", $prefix, uuid::Uuid::new_v4().simple()))
            }

            /// Borrow the underlying string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

id_newtype!(
    /// Identifies a registered agent (`codex`, `claude-code`, `remote-architect`).
    AgentId, "agent");
id_newtype!(
    /// Identifies a model exposed by an agent.
    ModelId, "model");
id_newtype!(
    /// Identifies a task or subtask in the plan DAG.
    TaskId, "task");
id_newtype!(
    /// Identifies one execution attempt of one task against one agent+model.
    AttemptId, "attempt");
id_newtype!(
    /// Identifies a harness session (one user-facing conversation).
    SessionId, "session");
id_newtype!(
    /// Identifies an installable skill.
    SkillId, "skill");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_prefixed_and_unique() {
        let a = TaskId::generate();
        let b = TaskId::generate();
        assert!(a.as_str().starts_with("task_"));
        assert_ne!(a, b);
    }

    #[test]
    fn ids_round_trip_through_json_as_plain_strings() {
        let id = AgentId::new("claude-code");
        let json = serde_json::to_string(&id).unwrap_or_default();
        assert_eq!(json, "\"claude-code\"");
    }
}
