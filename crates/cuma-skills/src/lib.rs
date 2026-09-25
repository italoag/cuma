//! Skills: discovery, validation and installation.
//!
//! When the planner finds a capability nothing provides, the skill manager
//! looks for something that does. The dangerous part is what happens next, so
//! the design rule is blunt: **nothing is installed or executed until it has
//! been validated, and validation defaults to refusing.**
//!
//! ```text
//! capability gap -> search -> validate -> policy check -> install -> register
//!                                 |
//!                                 └── refuse, with a reason
//! ```
//!
//! Validation is not a signature check bolted on at the end. It inspects
//! declared permissions, origin, integrity and the shape of the manifest
//! itself, and it downgrades trust on anything it cannot verify.

mod creation;
mod git;
mod index;
pub mod integrity;
mod local;
mod manager;
pub mod package;
mod validation;

pub use creation::{CreationRefusal, GeneratedSkill, SkillFactory};
pub use git::GitSkillRegistry;
pub use index::IndexSkillRegistry;
pub use local::LocalSkillRegistry;
pub use manager::{InstalledSkill, SkillManager, SkillOutcome, UpdateOutcome};
pub use validation::{ValidationReport, assess_fetched, validate, validate_with};

/// Whether a skill matches a lowercase search query.
pub(crate) fn matches_query(skill: &cuma_core::ports::SkillManifest, query: &str) -> bool {
    skill.id.as_str().to_ascii_lowercase().contains(query)
        || skill.name.to_ascii_lowercase().contains(query)
        || skill.description.to_ascii_lowercase().contains(query)
        || skill
            .capabilities
            .iter()
            .any(|c| c.to_string().contains(query))
}

/// Build the skill manager a configuration describes, for `workspace`.
///
/// Installs go to `skills.install_dir`, or `.cuma/skills` in the workspace.
/// A registry entry that cannot be used — an `http://` index, a `git+ssh://`
/// repository — is reported and skipped rather than failing the whole
/// manager, and a mistyped trusted key is an error.
pub fn from_config(
    config: &cuma_config::SkillsConfig,
    workspace: &std::path::Path,
) -> cuma_core::Result<(SkillManager, Vec<String>)> {
    use std::sync::Arc;

    let dot_cuma = workspace.join(".cuma");
    let mut warnings = Vec::new();
    let mut registries: Vec<Arc<dyn cuma_core::ports::SkillRegistry>> = Vec::new();

    for entry in &config.registries {
        let entry = entry.trim();
        let registry: cuma_core::Result<Arc<dyn cuma_core::ports::SkillRegistry>> = match entry {
            "builtin" => Ok(Arc::new(LocalSkillRegistry::new())),
            "local" => Ok(Arc::new(
                LocalSkillRegistry::without_builtins()
                    .with_directory(dot_cuma.join("skill-sources")),
            )),
            git if git.starts_with("git+") => {
                GitSkillRegistry::new(git, &dot_cuma.join("cache").join("skills"))
                    .map(|r| Arc::new(r) as _)
            }
            index if index.starts_with("https://") || index.starts_with("http://") => {
                IndexSkillRegistry::new(index).map(|r| Arc::new(r) as _)
            }
            other => Err(cuma_core::MetaAgentError::Configuration(format!(
                "unknown skill registry {other:?}"
            ))),
        };
        match registry {
            Ok(registry) => registries.push(registry),
            Err(err) => warnings.push(format!("skills.registries: skipping {entry:?}: {err}")),
        }
    }

    let install_dir = config
        .install_dir
        .as_ref()
        .map_or_else(|| dot_cuma.join("skills"), std::path::PathBuf::from);
    let keys = integrity::TrustedKeys::from_config(&config.trusted_keys)?;

    let manager = SkillManager::new(config.clone(), registries)
        .with_install_dir(install_dir)?
        .with_trusted_keys(keys);
    Ok((manager, warnings))
}
