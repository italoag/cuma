//! Skill packages on disk.
//!
//! A skill is a directory:
//!
//! ```text
//! rust-debugging/
//! ├── skill.toml   CUMA's manifest: id, capabilities, permissions
//! ├── SKILL.md     the instructions an agent is given
//! ├── skill.sig    optional signature over the rest (see `integrity`)
//! └── …            anything else the instructions refer to
//! ```
//!
//! A directory with only a `SKILL.md` — the Agent Skills layout, whose YAML
//! front matter names and describes the skill — is accepted too. It declares
//! no capabilities, so it is never installed to fill a capability gap; once a
//! person enables it, its instructions accompany every task.

use crate::integrity::skill_files;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{SkillManifest, TrustLevel};
use cuma_core::{Capability, SkillId};
use std::path::{Path, PathBuf};

/// The manifest file.
pub const MANIFEST_FILE: &str = "skill.toml";

/// The instructions file.
pub const INSTRUCTIONS_FILE: &str = "SKILL.md";

/// Parse a `skill.toml`.
pub fn parse_manifest(text: &str) -> Result<SkillManifest> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        id: String,
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        version: String,
        #[serde(default)]
        capabilities: Vec<String>,
        #[serde(default)]
        permissions: Vec<String>,
        #[serde(default)]
        checksum: Option<String>,
        #[serde(default)]
        signature: Option<String>,
    }

    let raw: Raw = toml::from_str(text)
        .map_err(|err| MetaAgentError::Skill(format!("invalid skill manifest: {err}")))?;
    check_id(&raw.id)?;

    Ok(SkillManifest {
        id: SkillId::new(raw.id),
        name: raw.name,
        description: raw.description,
        version: raw.version,
        source: String::new(),
        capabilities: raw
            .capabilities
            .iter()
            .map(|c| Capability::parse(c))
            .collect(),
        requested_permissions: raw.permissions,
        checksum: raw.checksum,
        signature: raw.signature,
        trust: TrustLevel::Community,
    })
}

/// A skill id becomes a directory name; keep it one.
pub fn check_id(id: &str) -> Result<()> {
    let plain = !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if plain && !id.contains("..") {
        Ok(())
    } else {
        Err(MetaAgentError::Security(format!(
            "skill id {id:?} is not a plain identifier"
        )))
    }
}

/// Read the `name` and `description` from `SKILL.md` front matter.
fn front_matter(text: &str) -> Option<(String, String)> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let (mut name, mut description) = (None, None);
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_owned();
            match key.trim() {
                "name" => name = Some(value),
                "description" => description = Some(value),
                _ => {}
            }
        }
    }
    Some((name?, description.unwrap_or_default()))
}

/// Read the skill in `dir`.
pub fn read_package(dir: &Path) -> Result<SkillManifest> {
    let manifest_path = dir.join(MANIFEST_FILE);
    if manifest_path.is_file() {
        let text = std::fs::read_to_string(&manifest_path).map_err(|err| {
            MetaAgentError::Skill(format!("cannot read {}: {err}", manifest_path.display()))
        })?;
        return parse_manifest(&text);
    }

    let instructions = std::fs::read_to_string(dir.join(INSTRUCTIONS_FILE)).map_err(|_| {
        MetaAgentError::Skill(format!(
            "{} has neither {MANIFEST_FILE} nor {INSTRUCTIONS_FILE}",
            dir.display()
        ))
    })?;
    let (name, description) = front_matter(&instructions).ok_or_else(|| {
        MetaAgentError::Skill(format!(
            "{INSTRUCTIONS_FILE} in {} has no front matter naming it",
            dir.display()
        ))
    })?;
    check_id(&name)?;

    Ok(SkillManifest {
        id: SkillId::new(name.clone()),
        name,
        description,
        version: String::new(),
        source: String::new(),
        capabilities: cuma_core::CapabilitySet::new(),
        requested_permissions: Vec::new(),
        checksum: None,
        signature: None,
        trust: TrustLevel::Community,
    })
}

/// The instructions in `dir`, if it has any.
pub fn read_instructions(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(INSTRUCTIONS_FILE)).ok()
}

/// Every skill package under `root`, at most `depth` directories down.
///
/// A package that does not parse is skipped with a warning: one broken skill
/// in a repository should not hide the others.
pub fn scan(root: &Path, depth: usize) -> Vec<(PathBuf, SkillManifest)> {
    let mut found = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];

    while let Some((dir, level)) = pending.pop() {
        if dir.join(MANIFEST_FILE).is_file() || dir.join(INSTRUCTIONS_FILE).is_file() {
            match read_package(&dir) {
                Ok(manifest) => found.push((dir, manifest)),
                Err(err) => {
                    tracing::warn!(dir = %dir.display(), error = %err, "skipping a skill package")
                }
            }
            continue;
        }
        if level >= depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            if kind.is_dir() && !name.to_string_lossy().starts_with('.') {
                pending.push((entry.path(), level + 1));
            }
        }
    }

    found.sort_by(|a, b| a.1.id.as_str().cmp(b.1.id.as_str()));
    found
}

/// Copy a package, refusing symlinks, into `to` (which must not exist).
pub fn copy_package(from: &Path, to: &Path) -> Result<()> {
    let files = skill_files(from)?;
    std::fs::create_dir_all(to)
        .map_err(|err| MetaAgentError::Skill(format!("cannot create {}: {err}", to.display())))?;
    let signature = from.join(crate::integrity::SIGNATURE_FILE);
    let signature_file = signature
        .is_file()
        .then(|| PathBuf::from(crate::integrity::SIGNATURE_FILE));

    for relative in files.into_iter().chain(signature_file) {
        let target = to.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| MetaAgentError::Skill(err.to_string()))?;
        }
        std::fs::copy(from.join(&relative), &target).map_err(|err| {
            MetaAgentError::Skill(format!("cannot copy {}: {err}", relative.display()))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn write(root: &Path, path: &str, contents: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    #[test]
    fn packages_are_found_in_either_layout() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "skills/rust/skill.toml",
            "id = 'rust-debug'\nname = 'Rust debugging'\ncapabilities = ['debugging']\n",
        );
        write(
            dir.path(),
            "skills/pdf/SKILL.md",
            "---\nname: pdf-tools\ndescription: \"Work with PDFs\"\n---\n# PDF\nUse pdftotext.\n",
        );
        write(dir.path(), "README.md", "not a skill");

        let found = scan(dir.path(), 3);
        let ids: Vec<_> = found
            .iter()
            .map(|(_, m)| m.id.as_str().to_owned())
            .collect();
        assert_eq!(ids, vec!["pdf-tools", "rust-debug"]);
        assert_eq!(found[0].1.description, "Work with PDFs");
        assert!(found[0].1.capabilities.is_empty());
    }

    #[test]
    fn an_id_that_would_escape_its_directory_is_refused() {
        for id in ["../etc", "a/b", ".hidden", "", "a..b"] {
            assert!(check_id(id).is_err(), "{id}");
        }
        assert!(parse_manifest("id = '../x'\nname = 'x'\n").is_err());
    }

    #[test]
    fn a_copy_carries_the_signature_and_nothing_else_extra() {
        let from = tempfile::tempdir().unwrap();
        write(from.path(), "skill.toml", "id = 'x'\nname = 'x'\n");
        write(from.path(), "skill.sig", "ed25519:k:AAAA");
        write(from.path(), "scripts/run.sh", "echo hi");
        let to = tempfile::tempdir().unwrap();
        let target = to.path().join("x");

        copy_package(from.path(), &target).unwrap();
        assert!(target.join("skill.sig").is_file());
        assert!(target.join("scripts/run.sh").is_file());
        assert_eq!(
            crate::integrity::content_digest(from.path()).unwrap(),
            crate::integrity::content_digest(&target).unwrap()
        );
    }
}
