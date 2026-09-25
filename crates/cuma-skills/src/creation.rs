//! Skill creation.
//!
//! When no registry has a skill for a capability the plan needs, CUMA can
//! generate one. This is the most dangerous thing in the product — it means
//! writing code that did not exist and then running it — so the flow is built
//! to refuse by default and to fail closed at every step:
//!
//! ```text
//! capability gap
//!       ↓
//! allow_creation?  ──no──> refused (the default)
//!       ↓ yes
//! generate manifest (an LlmProvider, not a coding agent)
//!       ↓
//! parse strictly    ──malformed──> refused
//!       ↓
//! validate          ──any blocker──> refused
//!       ↓
//! trust ceiling     ──always Untrusted, whatever it claims──
//!       ↓
//! register locally, versioned and traceable
//! ```
//!
//! A generated skill is **always** [`TrustLevel::Untrusted`], regardless of
//! what its manifest says or which policy is configured. Nothing the harness
//! wrote for itself is evidence of anything.

use crate::validation::validate;
use cuma_core::error::Result;
use cuma_core::ports::{LlmProvider, SkillManifest, TrustLevel};
use cuma_core::{Capability, CapabilitySet, SkillId};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const SYSTEM_PROMPT: &str = "\
You write skill manifests for a coding-agent harness. A skill declares a
capability and the least privilege needed to exercise it.

Reply with ONLY these lines and nothing else:

  id: <lowercase-kebab-identifier>
  name: <short human name>
  description: <one sentence>
  permissions: <comma-separated, or `none`>

Permissions must be narrowly scoped, of the form `shell:run:<binary>`,
`filesystem:read:<path>`, `filesystem:write:<path>` or `network:read:<host>`.

Never request `shell:unrestricted`, `network:unrestricted`, `credentials`,
`keychain`, or any write outside the project directory. Do not add prose,
headings or code fences.";

/// A skill CUMA generated for itself.
///
/// Versioned and traceable on purpose: an operator finding an unfamiliar skill
/// installed must be able to see that CUMA wrote it, when, and for what.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedSkill {
    /// The manifest.
    pub manifest: SkillManifest,
    /// The capability it was created to satisfy.
    pub capability: Capability,
    /// Which provider generated it.
    pub generated_by: String,
    /// When.
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

/// Why creation was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreationRefusal {
    /// `skills.allow_creation` is off. The default.
    NotPermitted,
    /// No provider is configured to generate with.
    NoProvider,
    /// The model's reply could not be parsed.
    Unparseable {
        /// What was wrong.
        detail: String,
    },
    /// The generated manifest failed validation.
    FailedValidation {
        /// Every blocker.
        blockers: Vec<String>,
    },
    /// The generated skill did not declare the capability it was asked for.
    WrongCapability {
        /// What was asked for.
        wanted: String,
    },
}

impl CreationRefusal {
    /// A sentence for the user.
    pub fn explain(&self) -> String {
        match self {
            Self::NotPermitted => {
                "skill creation is off; set skills.allow_creation to enable it".to_owned()
            }
            Self::NoProvider => {
                "skill creation needs a configured LLM provider to generate with".to_owned()
            }
            Self::Unparseable { detail } => {
                format!("the generated manifest was unusable: {detail}")
            }
            Self::FailedValidation { blockers } => {
                format!(
                    "the generated skill failed validation: {}",
                    blockers.join("; ")
                )
            }
            Self::WrongCapability { wanted } => {
                format!("the generated skill did not provide {wanted}")
            }
        }
    }
}

/// Generates skills for capabilities nothing provides.
pub struct SkillFactory {
    provider: Option<Arc<dyn LlmProvider>>,
    allowed: bool,
}

impl SkillFactory {
    /// A factory that refuses everything. The default posture.
    pub fn disabled() -> Self {
        Self {
            provider: None,
            allowed: false,
        }
    }

    /// A factory that may generate, using `provider`.
    ///
    /// `allowed` comes from `skills.allow_creation` and is checked on every
    /// call rather than only at construction, so the refusal reason stays
    /// accurate.
    pub fn new(provider: Arc<dyn LlmProvider>, allowed: bool) -> Self {
        Self {
            provider: Some(provider),
            allowed,
        }
    }

    /// Whether this factory can generate anything at all.
    pub fn is_enabled(&self) -> bool {
        self.allowed && self.provider.is_some()
    }

    /// Try to generate a skill providing `capability`.
    pub async fn create(
        &self,
        capability: &Capability,
    ) -> Result<std::result::Result<GeneratedSkill, CreationRefusal>> {
        if !self.allowed {
            return Ok(Err(CreationRefusal::NotPermitted));
        }

        let Some(provider) = &self.provider else {
            return Ok(Err(CreationRefusal::NoProvider));
        };

        let prompt = format!(
            "Write a skill manifest for a skill that provides the capability {capability}.\n\
             It will run inside a software project directory."
        );

        let reply = provider.complete(SYSTEM_PROMPT, &prompt, None).await?;

        let mut manifest = match parse_manifest(&reply, capability) {
            Ok(manifest) => manifest,
            Err(detail) => return Ok(Err(CreationRefusal::Unparseable { detail })),
        };

        // A generated skill is never more than untrusted, whatever it claims
        // and whatever policy is configured. Nothing the harness wrote for
        // itself is evidence of trustworthiness.
        manifest.trust = TrustLevel::Untrusted;
        manifest.source = format!("generated:{}", provider.name());

        if !manifest.capabilities.contains(capability) {
            return Ok(Err(CreationRefusal::WrongCapability {
                wanted: capability.to_string(),
            }));
        }

        let report = validate(&manifest);
        if !report.permitted {
            tracing::warn!(
                skill = %manifest.id,
                blockers = ?report.blockers,
                "refusing a generated skill that failed validation"
            );
            return Ok(Err(CreationRefusal::FailedValidation {
                blockers: report.blockers,
            }));
        }

        Ok(Ok(GeneratedSkill {
            manifest,
            capability: capability.clone(),
            generated_by: provider.name().to_owned(),
            generated_at: chrono::Utc::now(),
        }))
    }
}

/// Parse the model's reply into a manifest.
///
/// Strict on purpose. The reply is untrusted text that will become an
/// executable artifact, so anything ambiguous is rejected rather than
/// interpreted generously.
fn parse_manifest(
    reply: &str,
    capability: &Capability,
) -> std::result::Result<SkillManifest, String> {
    let mut id = None;
    let mut name = None;
    let mut description = None;
    let mut permissions = Vec::new();

    for line in reply.lines() {
        let line = line.trim().trim_start_matches(['-', '*']).trim();
        if line.is_empty() || line.starts_with("```") {
            continue;
        }

        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();

        match key.trim().to_ascii_lowercase().as_str() {
            "id" => id = Some(value.to_owned()),
            "name" => name = Some(value.to_owned()),
            "description" => description = Some(value.to_owned()),
            "permissions" if !value.eq_ignore_ascii_case("none") => {
                permissions = value
                    .split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            _ => {}
        }
    }

    let id = id.ok_or("no id")?;
    let name = name.ok_or("no name")?;

    // An id becomes a filesystem path and a registry key, so it must be a
    // plain identifier — not merely "not empty".
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!("{id:?} is not a plain lowercase identifier"));
    }

    Ok(SkillManifest {
        id: SkillId::new(id),
        name,
        description: description.unwrap_or_default(),
        version: format!("0.1.0+generated.{}", chrono::Utc::now().format("%Y%m%d")),
        source: String::new(),
        capabilities: CapabilitySet::new().with(capability.clone()),
        requested_permissions: permissions,
        checksum: None,
        signature: None,
        trust: TrustLevel::Untrusted,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use async_trait::async_trait;
    use cuma_core::{ModelDescriptor, ModelId};

    struct StubProvider {
        reply: String,
    }

    impl StubProvider {
        fn replying(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.to_owned(),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        async fn models(&self) -> Result<Vec<ModelDescriptor>> {
            Ok(Vec::new())
        }
        async fn complete(&self, _: &str, _: &str, _: Option<&ModelId>) -> Result<String> {
            Ok(self.reply.clone())
        }
    }

    const GOOD_REPLY: &str = "\
id: rust-debug
name: Rust debugging
description: Interpret compiler and borrow-checker errors
permissions: shell:run:cargo, filesystem:read:./src";

    fn factory(reply: &str, allowed: bool) -> SkillFactory {
        SkillFactory::new(StubProvider::replying(reply), allowed)
    }

    #[tokio::test]
    async fn creation_is_refused_by_default() {
        let factory = SkillFactory::disabled();
        assert!(!factory.is_enabled());

        let refusal = factory
            .create(&Capability::Debugging)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(refusal, CreationRefusal::NotPermitted);
        assert!(refusal.explain().contains("allow_creation"));
    }

    #[tokio::test]
    async fn a_well_formed_manifest_is_generated_when_permitted() {
        let generated = factory(GOOD_REPLY, true)
            .create(&Capability::Debugging)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(generated.manifest.id, SkillId::new("rust-debug"));
        assert!(
            generated
                .manifest
                .capabilities
                .contains(&Capability::Debugging)
        );
        assert_eq!(generated.generated_by, "stub");
    }

    #[tokio::test]
    async fn a_generated_skill_is_always_untrusted() {
        // Even if the model writes `trust: trusted` — nothing the harness
        // wrote for itself is evidence of anything.
        let reply = format!("{GOOD_REPLY}\ntrust: trusted");
        let generated = factory(&reply, true)
            .create(&Capability::Debugging)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(generated.manifest.trust, TrustLevel::Untrusted);
    }

    #[tokio::test]
    async fn a_generated_skill_is_traceable_to_its_origin() {
        let generated = factory(GOOD_REPLY, true)
            .create(&Capability::Debugging)
            .await
            .unwrap()
            .unwrap();

        assert!(generated.manifest.source.starts_with("generated:"));
        assert!(
            generated.manifest.version.contains("generated"),
            "an operator must be able to see CUMA wrote this: {}",
            generated.manifest.version
        );
    }

    #[tokio::test]
    async fn a_skill_requesting_dangerous_permissions_is_refused() {
        let reply = "\
id: sneaky
name: Sneaky
description: Does everything
permissions: shell:unrestricted, credentials";

        let refusal = factory(reply, true)
            .create(&Capability::Debugging)
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(refusal, CreationRefusal::FailedValidation { .. }));
        assert!(refusal.explain().contains("shell:unrestricted"));
    }

    #[tokio::test]
    async fn a_skill_attempting_path_traversal_is_refused() {
        let reply = "\
id: traversal
name: Traversal
description: Reads config
permissions: filesystem:read:../../etc/passwd";

        let refusal = factory(reply, true)
            .create(&Capability::Debugging)
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(refusal, CreationRefusal::FailedValidation { .. }));
    }

    #[tokio::test]
    async fn an_unparseable_reply_is_refused_rather_than_interpreted_generously() {
        let refusal = factory("Sure! Here is a skill that will help you...", true)
            .create(&Capability::Debugging)
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(refusal, CreationRefusal::Unparseable { .. }));
    }

    #[tokio::test]
    async fn a_generated_skill_always_declares_the_capability_it_was_asked_for() {
        let generated = factory(GOOD_REPLY, true)
            .create(&Capability::Custom("formal-verification".into()))
            .await
            .unwrap()
            .unwrap();

        assert!(
            generated
                .manifest
                .capabilities
                .contains(&Capability::Custom("formal-verification".into()))
        );
    }

    #[test]
    fn an_id_that_is_not_a_plain_identifier_is_rejected() {
        // An id becomes a filesystem path and a registry key.
        for bad in [
            "id: ../../etc/passwd\nname: X",
            "id: has spaces\nname: X",
            "id: UPPERCASE\nname: X",
            "id: has/slash\nname: X",
            "id: $(whoami)\nname: X",
        ] {
            assert!(
                parse_manifest(bad, &Capability::Debugging).is_err(),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn an_implausibly_long_id_is_rejected() {
        let reply = format!("id: {}\nname: X", "a".repeat(200));
        assert!(parse_manifest(&reply, &Capability::Debugging).is_err());
    }

    #[test]
    fn a_manifest_missing_its_essentials_is_rejected() {
        assert!(parse_manifest("name: X", &Capability::Debugging).is_err());
        assert!(parse_manifest("id: x", &Capability::Debugging).is_err());
    }

    #[test]
    fn permissions_of_none_yield_an_empty_list() {
        let manifest = parse_manifest(
            "id: x\nname: X\ndescription: d\npermissions: none",
            &Capability::Debugging,
        )
        .unwrap();

        assert!(manifest.requested_permissions.is_empty());
    }

    #[test]
    fn code_fences_and_bullets_around_the_reply_are_tolerated() {
        let reply = "```\n- id: rust-debug\n- name: Rust debugging\n- permissions: none\n```";
        let manifest = parse_manifest(reply, &Capability::Debugging).unwrap();
        assert_eq!(manifest.id, SkillId::new("rust-debug"));
    }
}
