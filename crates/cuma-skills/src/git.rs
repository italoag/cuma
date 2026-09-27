//! Skills from a git repository.
//!
//! `git+https://github.com/acme/skills` names a repository whose skill
//! packages live anywhere up to three directories deep. It is cloned shallowly
//! into a cache and refreshed on each [`GitSkillRegistry::refresh`].
//!
//! A git registry publishes no digest of its own, so its skills are
//! `Community` unless signed by a configured key.

use crate::package;
use async_trait::async_trait;
use cuma_core::SkillId;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{FetchedSkill, SkillManifest, SkillRegistry, TrustLevel};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::Mutex;

/// How long a clone or fetch may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// A skill registry backed by a git repository.
pub struct GitSkillRegistry {
    label: String,
    url: String,
    checkout: PathBuf,
    refreshed: Mutex<bool>,
}

impl GitSkillRegistry {
    /// A registry for `spec` (`git+https://…`), cached under `cache`.
    ///
    /// Only HTTPS is accepted: a skill is instructions an agent will follow,
    /// and fetching instructions over a channel anyone on the path can
    /// rewrite would make every other check moot.
    pub fn new(spec: &str, cache: &Path) -> Result<Self> {
        let url = spec.strip_prefix("git+").unwrap_or(spec);
        if !url.starts_with("https://") {
            return Err(MetaAgentError::Security(format!(
                "skill registry {spec:?}: only git+https:// repositories are accepted"
            )));
        }
        Ok(Self::unchecked(spec, url, cache))
    }

    fn unchecked(spec: &str, url: &str, cache: &Path) -> Self {
        let slug: String = url
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        Self {
            label: spec.to_owned(),
            url: url.to_owned(),
            checkout: cache.join(slug),
            refreshed: Mutex::new(false),
        }
    }

    /// Clone or update the cached checkout. Done once per process unless
    /// asked again.
    pub async fn refresh(&self) -> Result<()> {
        let mut refreshed = self.refreshed.lock().await;
        let checkout = self.checkout.to_string_lossy().to_string();

        if self.checkout.join(".git").is_dir() {
            git(&self.checkout, &["fetch", "--depth", "1", "origin", "HEAD"]).await?;
            git(&self.checkout, &["reset", "--hard", "FETCH_HEAD"]).await?;
        } else {
            if let Some(parent) = self.checkout.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|err| MetaAgentError::Skill(err.to_string()))?;
            }
            git(
                self.checkout.parent().unwrap_or(Path::new(".")),
                &["clone", "--depth", "1", "--quiet", &self.url, &checkout],
            )
            .await?;
        }
        *refreshed = true;
        Ok(())
    }

    async fn ensure_checkout(&self) -> Result<()> {
        if *self.refreshed.lock().await || self.checkout.join(".git").is_dir() {
            return Ok(());
        }
        self.refresh().await
    }

    /// The commit the checkout is at.
    pub async fn revision(&self) -> Option<String> {
        git(&self.checkout, &["rev-parse", "HEAD"])
            .await
            .ok()
            .map(|r| r.trim().to_owned())
    }

    async fn packages(&self) -> Result<Vec<(PathBuf, SkillManifest)>> {
        self.ensure_checkout().await?;
        let root = self.checkout.clone();
        let url = self.url.clone();
        let found = tokio::task::spawn_blocking(move || package::scan(&root, 3))
            .await
            .map_err(|err| MetaAgentError::Skill(err.to_string()))?;

        Ok(found
            .into_iter()
            .map(|(dir, mut manifest)| {
                let relative = dir.strip_prefix(&self.checkout).unwrap_or(&dir);
                manifest.source = format!("git+{url}#{}", relative.display());
                manifest.trust = TrustLevel::Community;
                (dir, manifest)
            })
            .collect())
    }
}

async fn git(directory: &Path, args: &[&str]) -> Result<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(directory)
        .args(args)
        // A registry that asks for credentials is misconfigured; hanging on a
        // prompt nobody can see is worse than failing.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| MetaAgentError::Timeout {
            operation: format!("git {}", args.join(" ")),
            elapsed_ms: GIT_TIMEOUT.as_millis() as u64,
        })?
        .map_err(|err| MetaAgentError::Skill(format!("cannot run git: {err}")))?;

    if !output.status.success() {
        return Err(MetaAgentError::Skill(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[async_trait]
impl SkillRegistry for GitSkillRegistry {
    fn name(&self) -> &str {
        &self.label
    }

    async fn search(&self, query: &str) -> Result<Vec<SkillManifest>> {
        let query = query.trim().to_ascii_lowercase();
        Ok(self
            .packages()
            .await?
            .into_iter()
            .map(|(_, m)| m)
            .filter(|m| query.is_empty() || crate::matches_query(m, &query))
            .collect())
    }

    async fn inspect(&self, id: &SkillId) -> Result<SkillManifest> {
        self.packages()
            .await?
            .into_iter()
            .map(|(_, m)| m)
            .find(|m| &m.id == id)
            .ok_or_else(|| MetaAgentError::Skill(format!("{} has no skill named {id}", self.label)))
    }

    async fn install(&self, id: &SkillId) -> Result<SkillManifest> {
        self.inspect(id).await
    }

    async fn fetch(&self, id: &SkillId, into: &Path) -> Result<FetchedSkill> {
        let Some((dir, manifest)) = self
            .packages()
            .await?
            .into_iter()
            .find(|(_, m)| &m.id == id)
        else {
            return Err(MetaAgentError::Skill(format!(
                "{} has no skill named {id}",
                self.label
            )));
        };
        package::copy_package(&dir, into)?;
        Ok(FetchedSkill {
            manifest,
            published_digest: None,
            has_files: true,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A local repository with one skill, standing in for a remote.
    fn origin() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.email", "t@example.invalid"]);
        run(root, &["config", "user.name", "T"]);
        std::fs::create_dir_all(root.join("skills/rust-debug")).unwrap();
        std::fs::write(
            root.join("skills/rust-debug/skill.toml"),
            "id = 'rust-debug'\nname = 'Rust debugging'\ncapabilities = ['debugging']\n",
        )
        .unwrap();
        std::fs::write(
            root.join("skills/rust-debug/SKILL.md"),
            "Use RUST_BACKTRACE=1.\n",
        )
        .unwrap();
        run(root, &["add", "-A"]);
        run(root, &["commit", "-qm", "skills"]);
        dir
    }

    #[test]
    fn only_https_repositories_are_accepted() {
        let cache = tempfile::tempdir().unwrap();
        assert!(GitSkillRegistry::new("git+http://example.invalid/skills", cache.path()).is_err());
        assert!(GitSkillRegistry::new("git+ssh://example.invalid/skills", cache.path()).is_err());
        assert!(GitSkillRegistry::new("git+https://example.invalid/skills", cache.path()).is_ok());
    }

    #[tokio::test]
    async fn skills_in_a_repository_are_found_and_fetched() {
        let origin = origin();
        let cache = tempfile::tempdir().unwrap();
        let url = origin.path().display().to_string();
        let registry = GitSkillRegistry::unchecked("test", &url, cache.path());

        let found = registry.search("rust").await.unwrap();
        assert_eq!(found.len(), 1);
        assert!(
            found[0].source.ends_with("#skills/rust-debug"),
            "{}",
            found[0].source
        );
        assert_eq!(found[0].trust, TrustLevel::Community);

        let into = tempfile::tempdir().unwrap();
        let target = into.path().join("rust-debug");
        let fetched = registry
            .fetch(&SkillId::new("rust-debug"), &target)
            .await
            .unwrap();
        assert!(fetched.has_files);
        assert!(target.join("SKILL.md").is_file());
        assert!(!target.join(".git").exists());
        assert!(registry.revision().await.is_some());
    }
}
