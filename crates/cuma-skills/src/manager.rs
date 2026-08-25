//! The skill manager: gap detection through to registration.

use crate::validation::{ValidationReport, may_auto_install, validate};
use cuma_config::SkillsConfig;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{SkillManifest, SkillRegistry};
use cuma_core::{Capability, CapabilitySet, SkillId};
use std::sync::Arc;
use tokio::sync::RwLock;

/// What happened when the manager tried to satisfy a capability gap.
#[derive(Debug, Clone)]
pub enum SkillOutcome {
    /// A skill was installed and registered.
    Installed {
        /// The skill.
        manifest: Box<SkillManifest>,
    },
    /// A suitable skill was found but policy requires a human to approve it.
    NeedsApproval {
        /// The candidate.
        manifest: Box<SkillManifest>,
        /// What validation concluded.
        report: ValidationReport,
    },
    /// A candidate was found and refused outright.
    Refused {
        /// The candidate.
        manifest: Box<SkillManifest>,
        /// Why.
        report: ValidationReport,
    },
    /// No registry had anything for this capability.
    NotFound {
        /// The capability that is still missing.
        capability: Capability,
    },
}

/// Finds, validates and installs skills.
pub struct SkillManager {
    config: SkillsConfig,
    registries: Vec<Arc<dyn SkillRegistry>>,
    installed: Arc<RwLock<Vec<SkillManifest>>>,
}

impl SkillManager {
    /// A manager over `registries`.
    pub fn new(config: SkillsConfig, registries: Vec<Arc<dyn SkillRegistry>>) -> Self {
        Self {
            config,
            registries,
            installed: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Skills currently installed.
    pub async fn installed(&self) -> Vec<SkillManifest> {
        self.installed.read().await.clone()
    }

    /// The union of every installed skill's capabilities.
    pub async fn installed_capabilities(&self) -> CapabilitySet {
        let mut set = CapabilitySet::new();
        for skill in self.installed.read().await.iter() {
            for capability in skill.capabilities.iter() {
                set.insert(capability.clone());
            }
        }
        set
    }

    /// Search every registry.
    ///
    /// A failing registry is skipped rather than aborting the search: one
    /// unreachable remote registry should not hide the built-in skills.
    pub async fn search(&self, query: &str) -> Vec<SkillManifest> {
        let mut found = Vec::new();

        for registry in &self.registries {
            match registry.search(query).await {
                Ok(skills) => found.extend(skills),
                Err(err) => {
                    tracing::warn!(
                        registry = registry.name(),
                        error = %err,
                        "a skill registry failed; continuing with the others"
                    );
                }
            }
        }

        found
    }

    /// Try to satisfy `capability` by finding and installing a skill.
    ///
    /// This is the "capability missing → search → validate → install" flow.
    /// Note what it does *not* do: it never installs something that failed
    /// validation, and never installs above the configured trust bar without
    /// returning [`SkillOutcome::NeedsApproval`] for a human to decide.
    pub async fn satisfy(&self, capability: &Capability) -> SkillOutcome {
        if !self.config.enabled {
            return SkillOutcome::NotFound {
                capability: capability.clone(),
            };
        }

        let query = capability.to_string();
        let candidates = self.search(&query).await;

        let Some(manifest) = candidates
            .into_iter()
            .find(|skill| skill.capabilities.contains(capability))
        else {
            return SkillOutcome::NotFound {
                capability: capability.clone(),
            };
        };

        let report = validate(&manifest);

        if !report.permitted {
            tracing::warn!(
                skill = %manifest.id,
                blockers = ?report.blockers,
                "refusing a skill that failed validation"
            );
            return SkillOutcome::Refused {
                manifest: Box::new(manifest),
                report,
            };
        }

        if !may_auto_install(report.trust, self.config.auto_install) {
            return SkillOutcome::NeedsApproval {
                manifest: Box::new(manifest),
                report,
            };
        }

        match self.install(&manifest.id).await {
            Ok(installed) => SkillOutcome::Installed {
                manifest: Box::new(installed),
            },
            Err(err) => {
                tracing::warn!(skill = %manifest.id, error = %err, "skill installation failed");
                SkillOutcome::Refused {
                    manifest: Box::new(manifest),
                    report,
                }
            }
        }
    }

    /// Install a skill by id, validating first.
    ///
    /// Called directly by `cuma skills install`, which is how a human approves
    /// something [`SkillManager::satisfy`] would not auto-install.
    pub async fn install(&self, id: &SkillId) -> Result<SkillManifest> {
        for registry in &self.registries {
            let Ok(manifest) = registry.inspect(id).await else {
                continue;
            };

            let report = validate(&manifest);
            if !report.permitted {
                return Err(MetaAgentError::Security(format!(
                    "refusing to install {id}: {}",
                    report.blockers.join("; ")
                )));
            }

            let installed = registry.install(id).await?;
            self.installed.write().await.push(installed.clone());
            return Ok(installed);
        }

        Err(MetaAgentError::Skill(format!(
            "no registry provides a skill named {id}"
        )))
    }

    /// Remove an installed skill.
    pub async fn remove(&self, id: &SkillId) -> bool {
        let mut installed = self.installed.write().await;
        let before = installed.len();
        installed.retain(|s| &s.id != id);
        installed.len() != before
    }

    /// Capabilities in `required` that neither the agents nor installed skills
    /// provide.
    pub async fn gaps(&self, required: &CapabilitySet, available: &CapabilitySet) -> Vec<Capability> {
        let installed = self.installed_capabilities().await;

        required
            .iter()
            .filter(|c| !available.contains(c) && !installed.contains(c))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::local::LocalSkillRegistry;
    use cuma_config::SkillAutoInstall;

    fn manager(auto_install: SkillAutoInstall) -> SkillManager {
        SkillManager::new(
            SkillsConfig {
                enabled: true,
                auto_install,
                ..SkillsConfig::default()
            },
            vec![Arc::new(LocalSkillRegistry::new())],
        )
    }

    #[tokio::test]
    async fn a_trusted_builtin_skill_is_installed_automatically() {
        let manager = manager(SkillAutoInstall::TrustedOnly);
        let outcome = manager.satisfy(&Capability::VersionControl).await;

        assert!(
            matches!(outcome, SkillOutcome::Installed { .. }),
            "got {outcome:?}"
        );
        assert_eq!(manager.installed().await.len(), 1);
    }

    #[tokio::test]
    async fn the_never_policy_asks_for_approval_even_for_a_builtin() {
        let manager = manager(SkillAutoInstall::Never);
        let outcome = manager.satisfy(&Capability::VersionControl).await;

        assert!(matches!(outcome, SkillOutcome::NeedsApproval { .. }));
        assert!(
            manager.installed().await.is_empty(),
            "nothing may be installed under a never policy"
        );
    }

    #[tokio::test]
    async fn a_capability_nothing_provides_is_reported_as_not_found() {
        let manager = manager(SkillAutoInstall::TrustedOnly);
        let outcome = manager
            .satisfy(&Capability::Custom("time-travel".into()))
            .await;

        assert!(matches!(outcome, SkillOutcome::NotFound { .. }));
    }

    #[tokio::test]
    async fn skills_disabled_means_nothing_is_ever_installed() {
        let manager = SkillManager::new(
            SkillsConfig {
                enabled: false,
                ..SkillsConfig::default()
            },
            vec![Arc::new(LocalSkillRegistry::new())],
        );

        assert!(matches!(
            manager.satisfy(&Capability::VersionControl).await,
            SkillOutcome::NotFound { .. }
        ));
    }

    #[tokio::test]
    async fn installing_an_unknown_skill_is_an_error() {
        let manager = manager(SkillAutoInstall::TrustedOnly);
        assert!(manager.install(&SkillId::new("nonexistent")).await.is_err());
    }

    #[tokio::test]
    async fn installed_skills_contribute_their_capabilities() {
        let manager = manager(SkillAutoInstall::TrustedOnly);
        assert!(manager.installed_capabilities().await.is_empty());

        manager.install(&SkillId::new("git-workflow")).await.unwrap();
        assert!(
            manager
                .installed_capabilities()
                .await
                .contains(&Capability::VersionControl)
        );
    }

    #[tokio::test]
    async fn a_gap_is_only_a_gap_when_neither_agents_nor_skills_cover_it() {
        let manager = manager(SkillAutoInstall::TrustedOnly);

        let required = CapabilitySet::new()
            .with(Capability::CodeEditing)
            .with(Capability::VersionControl)
            .with(Capability::Vision);
        let agents_provide = CapabilitySet::new().with(Capability::CodeEditing);

        // Before installing anything, two capabilities are missing.
        assert_eq!(manager.gaps(&required, &agents_provide).await.len(), 2);

        manager.install(&SkillId::new("git-workflow")).await.unwrap();

        // The skill closes one of them.
        assert_eq!(
            manager.gaps(&required, &agents_provide).await,
            vec![Capability::Vision]
        );
    }

    #[tokio::test]
    async fn a_skill_can_be_removed() {
        let manager = manager(SkillAutoInstall::TrustedOnly);
        manager.install(&SkillId::new("git-workflow")).await.unwrap();

        assert!(manager.remove(&SkillId::new("git-workflow")).await);
        assert!(manager.installed().await.is_empty());
        assert!(!manager.remove(&SkillId::new("git-workflow")).await);
    }
}
