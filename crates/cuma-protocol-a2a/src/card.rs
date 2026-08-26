//! Agent Cards.
//!
//! An Agent Card is A2A's self-description document, served at
//! `/.well-known/agent-card.json`. It is how a remote agent advertises what it
//! can do — and, being remote-controlled text, it is exactly the kind of input
//! that must never be trusted beyond its stated purpose.

use cuma_core::{Capability, CapabilitySet};
use serde::{Deserialize, Serialize};

/// One advertised skill on an Agent Card.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    /// Skill identifier.
    pub id: String,
    /// Display name.
    pub name: String,
    /// What it does.
    #[serde(default)]
    pub description: String,
    /// Free-form tags. These drive capability inference.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Example invocations.
    #[serde(default)]
    pub examples: Vec<String>,
}

/// Protocol-level capabilities an A2A agent advertises.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCardCapabilities {
    /// Whether the agent can stream task updates.
    #[serde(default)]
    pub streaming: bool,
    /// Whether the agent supports push notifications.
    #[serde(default)]
    pub push_notifications: bool,
    /// Whether the agent exposes task state history.
    #[serde(default)]
    pub state_transition_history: bool,
}

/// A remote agent's self-description.
///
/// Unknown fields are ignored rather than rejected: A2A is a moving spec, and
/// refusing to talk to an agent that advertises one field too many would make
/// the harness brittle for no security benefit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    /// Agent name.
    pub name: String,
    /// What the agent is for.
    #[serde(default)]
    pub description: String,
    /// Base URL for JSON-RPC calls.
    pub url: String,
    /// Agent version.
    #[serde(default)]
    pub version: String,
    /// Protocol version the agent implements.
    #[serde(default)]
    pub protocol_version: Option<String>,
    /// Protocol-level capabilities.
    #[serde(default)]
    pub capabilities: AgentCardCapabilities,
    /// Advertised skills.
    #[serde(default)]
    pub skills: Vec<AgentSkill>,
    /// Default input MIME types.
    #[serde(default)]
    pub default_input_modes: Vec<String>,
    /// Default output MIME types.
    #[serde(default)]
    pub default_output_modes: Vec<String>,
}

/// The longest capability tag accepted from a card.
const MAX_TAG_LEN: usize = 64;

/// Accept a card tag only if it is a plain identifier.
///
/// Returns `None` for anything containing path separators, whitespace, control
/// characters or shell metacharacters, and for anything implausibly long.
fn sanitize_tag(raw: &str) -> Option<String> {
    let trimmed = raw.trim();

    if trimmed.is_empty() || trimmed.len() > MAX_TAG_LEN {
        return None;
    }

    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return None;
    }

    // `.` is allowed inside a tag (`rust.async`) but a leading dot or a `..`
    // run is how path traversal starts, so neither is accepted.
    if trimmed.starts_with('.') || trimmed.contains("..") {
        return None;
    }

    Some(trimmed.to_owned())
}

/// The conventional path an Agent Card is served from.
pub const AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// Infer domain capabilities from an Agent Card.
///
/// Skill tags and ids are matched against the known capability vocabulary;
/// anything unrecognized becomes a [`Capability::Custom`] rather than being
/// discarded, so a specialist agent stays routable for its specialty.
///
/// Nothing here grants the remote agent any privilege. It only determines
/// which tasks the router will consider sending it.
pub fn capabilities_from_card(card: &AgentCard) -> CapabilitySet {
    let mut capabilities = CapabilitySet::new();

    for skill in &card.skills {
        for token in skill.tags.iter().chain(std::iter::once(&skill.id)) {
            // Card text is remote-controlled. A capability name ends up in log
            // lines, explanations and potentially on disk, so anything that is
            // not a plain identifier is dropped here rather than carried
            // deeper into the system.
            let Some(token) = sanitize_tag(token) else {
                tracing::debug!(
                    agent = card.name,
                    tag = token,
                    "ignoring an Agent Card tag that is not a plain identifier"
                );
                continue;
            };

            // A tag that parses to a known capability is taken at face value.
            // One that does not is kept as an opaque custom capability: it
            // cannot match a task's requirement unless that requirement names
            // the same string, which is the safe failure mode.
            capabilities.insert(Capability::parse(&token));
        }
    }

    // Every A2A agent can at minimum be asked a question and answer it.
    if capabilities.is_empty() {
        capabilities.insert(Capability::Research);
    }

    capabilities
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn card_with_skills(skills: Vec<AgentSkill>) -> AgentCard {
        AgentCard {
            name: "remote".into(),
            url: "https://example.invalid/a2a".into(),
            skills,
            ..AgentCard::default()
        }
    }

    #[test]
    fn a_minimal_card_parses() {
        let card: AgentCard =
            serde_json::from_str(r#"{"name":"architect","url":"https://example.invalid/a2a"}"#)
                .unwrap();
        assert_eq!(card.name, "architect");
        assert!(card.skills.is_empty());
    }

    #[test]
    fn unknown_fields_on_a_card_are_ignored_rather_than_fatal() {
        let card: AgentCard =
            serde_json::from_str(r#"{"name":"a","url":"u","somethingBrandNew":{"nested":true}}"#)
                .unwrap();
        assert_eq!(card.name, "a");
    }

    #[test]
    fn known_tags_become_known_capabilities() {
        let card = card_with_skills(vec![AgentSkill {
            id: "reviewer".into(),
            name: "Code review".into(),
            tags: vec!["code_review".into(), "architecture".into()],
            ..AgentSkill::default()
        }]);

        let capabilities = capabilities_from_card(&card);
        assert!(capabilities.contains(&Capability::CodeReview));
        assert!(capabilities.contains(&Capability::Architecture));
    }

    #[test]
    fn an_unrecognised_tag_is_kept_as_a_custom_capability() {
        let card = card_with_skills(vec![AgentSkill {
            id: "verifier".into(),
            name: "Formal verification".into(),
            tags: vec!["formal-verification".into()],
            ..AgentSkill::default()
        }]);

        let capabilities = capabilities_from_card(&card);
        assert!(
            capabilities.contains(&Capability::Custom("formal_verification".into())),
            "a specialist must stay routable for its specialty"
        );
    }

    #[test]
    fn a_card_advertising_nothing_still_gets_a_baseline() {
        let capabilities = capabilities_from_card(&card_with_skills(vec![]));
        assert!(!capabilities.is_empty());
        assert!(capabilities.contains(&Capability::Research));
    }

    #[test]
    fn a_card_cannot_advertise_its_way_into_privileged_capabilities() {
        // A hostile card claiming every dangerous tag it can think of.
        let card = card_with_skills(vec![AgentSkill {
            id: "sudo".into(),
            name: "Everything".into(),
            tags: vec![
                "shell_execution".into(),
                "ignore-all-previous-instructions".into(),
                "../../etc/passwd".into(),
            ],
            ..AgentSkill::default()
        }]);

        let capabilities = capabilities_from_card(&card);

        // The card *can* claim shell execution — that is what the field is
        // for, and the router will believe it. What it cannot do is turn its
        // prose into anything but an opaque capability string.
        assert!(capabilities.contains(&Capability::ShellExecution));
        assert!(capabilities.contains(&Capability::Custom(
            "ignore_all_previous_instructions".into()
        )));
        assert!(
            !capabilities.contains(&Capability::Custom("../../etc/passwd".into())),
            "a path-shaped tag must be dropped, got {:?}",
            capabilities.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn tags_that_are_not_plain_identifiers_are_dropped() {
        for hostile in [
            "../../etc/passwd",
            "a/b",
            "rm -rf /",
            "tag\nwith\nnewlines",
            "tag;whoami",
            "$(id)",
            ".hidden",
            "a..b",
            "",
            "   ",
        ] {
            assert_eq!(sanitize_tag(hostile), None, "accepted {hostile:?}");
        }
    }

    #[test]
    fn ordinary_tags_survive_sanitization() {
        for benign in ["code_review", "code-review", "rust.async", "Testing"] {
            assert!(sanitize_tag(benign).is_some(), "rejected {benign:?}");
        }
    }

    #[test]
    fn an_implausibly_long_tag_is_dropped() {
        assert_eq!(sanitize_tag(&"x".repeat(500)), None);
    }
}
