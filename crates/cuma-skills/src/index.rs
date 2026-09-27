//! Skills from an HTTPS index.
//!
//! An index is one JSON document listing skills, the files each consists of,
//! and the digest those files must hash to:
//!
//! ```json
//! { "skills": [ {
//!     "id": "rust-debug",
//!     "name": "Rust debugging",
//!     "description": "…",
//!     "version": "1.2.0",
//!     "capabilities": ["debugging"],
//!     "permissions": ["shell:run:cargo"],
//!     "sha256": "sha256:9f86d0…",
//!     "base_url": "https://skills.example.com/rust-debug/",
//!     "files": ["skill.toml", "SKILL.md", "skill.sig"]
//! } ] }
//! ```
//!
//! Because the index is fetched over TLS from a registry the operator
//! configured, files that hash to its published digest are `Verified`.
//! Files that do not are refused.

use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{FetchedSkill, SkillManifest, SkillRegistry, TrustLevel};
use cuma_core::{Capability, SkillId};
use serde::Deserialize;
use std::path::{Component, Path};
use std::time::Duration;
use tokio::sync::OnceCell;

/// The largest index or file accepted.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// The most files one skill may list.
const MAX_FILES: usize = 64;

#[derive(Debug, Clone, Deserialize)]
struct Index {
    #[serde(default)]
    skills: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize)]
struct Entry {
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
    sha256: String,
    base_url: String,
    files: Vec<String>,
}

/// A skill registry published as an HTTPS index.
pub struct IndexSkillRegistry {
    url: String,
    http: reqwest::Client,
    index: OnceCell<Index>,
}

impl IndexSkillRegistry {
    /// A registry for the index at `url`, which must be HTTPS.
    pub fn new(url: &str) -> Result<Self> {
        if !url.starts_with("https://") {
            return Err(MetaAgentError::Security(format!(
                "skill registry {url:?}: only https:// indexes are accepted"
            )));
        }
        Self::unchecked(url)
    }

    fn unchecked(url: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("cuma/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| {
                MetaAgentError::Configuration(format!("cannot build an HTTP client: {err}"))
            })?;
        Ok(Self {
            url: url.to_owned(),
            http,
            index: OnceCell::new(),
        })
    }

    async fn get(&self, url: &str) -> Result<Vec<u8>> {
        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| MetaAgentError::Skill(format!("cannot fetch {url}: {err}")))?;
        if !response.status().is_success() {
            return Err(MetaAgentError::Skill(format!(
                "{url} returned {}",
                response.status()
            )));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| MetaAgentError::Skill(format!("reading {url}: {err}")))?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_BYTES {
                return Err(MetaAgentError::Security(format!(
                    "{url} exceeds {MAX_BYTES} bytes"
                )));
            }
        }
        Ok(body)
    }

    async fn index(&self) -> Result<&Index> {
        self.index
            .get_or_try_init(|| async {
                let body = self.get(&self.url).await?;
                serde_json::from_slice::<Index>(&body).map_err(|err| {
                    MetaAgentError::Skill(format!("{} is not a skill index: {err}", self.url))
                })
            })
            .await
    }

    fn manifest(&self, entry: &Entry) -> SkillManifest {
        SkillManifest {
            id: SkillId::new(entry.id.clone()),
            name: entry.name.clone(),
            description: entry.description.clone(),
            version: entry.version.clone(),
            source: format!("{}#{}", self.url, entry.id),
            capabilities: entry
                .capabilities
                .iter()
                .map(|c| Capability::parse(c))
                .collect(),
            requested_permissions: entry.permissions.clone(),
            checksum: Some(entry.sha256.clone()),
            signature: None,
            trust: TrustLevel::Community,
        }
    }

    async fn entry(&self, id: &SkillId) -> Result<Entry> {
        self.index()
            .await?
            .skills
            .iter()
            .find(|e| e.id == id.as_str())
            .cloned()
            .ok_or_else(|| MetaAgentError::Skill(format!("{} lists no skill named {id}", self.url)))
    }
}

/// Whether `path` is a plain relative path that stays inside its directory.
fn is_contained(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

#[async_trait]
impl SkillRegistry for IndexSkillRegistry {
    fn name(&self) -> &str {
        &self.url
    }

    async fn search(&self, query: &str) -> Result<Vec<SkillManifest>> {
        let query = query.trim().to_ascii_lowercase();
        Ok(self
            .index()
            .await?
            .skills
            .iter()
            .map(|e| self.manifest(e))
            .filter(|m| query.is_empty() || crate::matches_query(m, &query))
            .collect())
    }

    async fn inspect(&self, id: &SkillId) -> Result<SkillManifest> {
        Ok(self.manifest(&self.entry(id).await?))
    }

    async fn install(&self, id: &SkillId) -> Result<SkillManifest> {
        self.inspect(id).await
    }

    async fn fetch(&self, id: &SkillId, into: &Path) -> Result<FetchedSkill> {
        let entry = self.entry(id).await?;
        crate::package::check_id(&entry.id)?;

        if entry.files.len() > MAX_FILES {
            return Err(MetaAgentError::Security(format!(
                "{id} lists more than {MAX_FILES} files"
            )));
        }
        let base = if entry.base_url.ends_with('/') {
            entry.base_url.clone()
        } else {
            format!("{}/", entry.base_url)
        };
        if !base.starts_with("https://") && !self.url.starts_with("http://127.0.0.1") {
            return Err(MetaAgentError::Security(format!(
                "{id}: files must be fetched over https"
            )));
        }

        for file in &entry.files {
            // An index is remote-controlled; a path like `../../.bashrc` would
            // write outside the staging directory.
            if !is_contained(file) {
                return Err(MetaAgentError::Security(format!(
                    "{id}: refusing the file path {file:?}"
                )));
            }
            let bytes = self.get(&format!("{base}{file}")).await?;
            let target = into.join(file);
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|err| MetaAgentError::Skill(err.to_string()))?;
            }
            tokio::fs::write(&target, bytes)
                .await
                .map_err(|err| MetaAgentError::Skill(format!("cannot write {file}: {err}")))?;
        }

        Ok(FetchedSkill {
            manifest: self.manifest(&entry),
            published_digest: Some(entry.sha256),
            has_files: true,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use axum::routing::get;

    #[test]
    fn only_https_indexes_are_accepted() {
        assert!(IndexSkillRegistry::new("http://example.invalid/index.json").is_err());
        assert!(IndexSkillRegistry::new("https://example.invalid/index.json").is_ok());
    }

    #[test]
    fn file_paths_must_stay_inside_the_skill() {
        for bad in ["../x", "/etc/passwd", "a/../../b", ""] {
            assert!(!is_contained(bad), "{bad}");
        }
        assert!(is_contained("scripts/run.sh"));
    }

    /// Serve an index whose one skill hashes to `published`.
    async fn serve(published: String, files: Vec<&'static str>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let index = serde_json::json!({ "skills": [{
            "id": "rust-debug", "name": "Rust debugging", "capabilities": ["debugging"],
            "sha256": published, "base_url": format!("{base}/files/"), "files": files,
        }]});
        let app = axum::Router::new()
            .route(
                "/index.json",
                get(move || {
                    let index = index.clone();
                    async move { axum::Json(index) }
                }),
            )
            .route(
                "/files/skill.toml",
                get(|| async { "id = 'rust-debug'\nname = 'Rust debugging'\n" }),
            )
            .route(
                "/files/SKILL.md",
                get(|| async { "Use RUST_BACKTRACE=1.\n" }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("{base}/index.json")
    }

    fn digest_of_served_files() -> String {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("skill.toml"),
            "id = 'rust-debug'\nname = 'Rust debugging'\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "Use RUST_BACKTRACE=1.\n").unwrap();
        crate::integrity::content_digest(dir.path()).unwrap()
    }

    #[tokio::test]
    async fn fetched_files_that_match_the_published_digest_are_verified() {
        let url = serve(digest_of_served_files(), vec!["skill.toml", "SKILL.md"]).await;
        let registry = IndexSkillRegistry::unchecked(&url).unwrap();

        let into = tempfile::tempdir().unwrap();
        let fetched = registry
            .fetch(&SkillId::new("rust-debug"), into.path())
            .await
            .unwrap();
        let evidence = crate::integrity::Evidence::gather(
            into.path(),
            fetched.published_digest,
            &crate::integrity::TrustedKeys::default(),
        )
        .unwrap();
        assert_eq!(evidence.digest_matches(), Some(true));

        let mut manifest = fetched.manifest;
        manifest.source = "https://registry.example/index.json#rust-debug".into();
        let report = crate::validation::assess_fetched(&manifest, &evidence);
        assert_eq!(report.trust, TrustLevel::Verified);
    }

    #[tokio::test]
    async fn fetched_files_that_do_not_match_are_refused() {
        let url = serve("sha256:0000".into(), vec!["skill.toml", "SKILL.md"]).await;
        let registry = IndexSkillRegistry::unchecked(&url).unwrap();

        let into = tempfile::tempdir().unwrap();
        let fetched = registry
            .fetch(&SkillId::new("rust-debug"), into.path())
            .await
            .unwrap();
        let evidence = crate::integrity::Evidence::gather(
            into.path(),
            fetched.published_digest,
            &crate::integrity::TrustedKeys::default(),
        )
        .unwrap();
        let report = crate::validation::assess_fetched(&fetched.manifest, &evidence);
        assert!(!report.permitted);
    }

    #[tokio::test]
    async fn an_index_listing_a_path_outside_the_skill_is_refused() {
        let url = serve("sha256:0000".into(), vec!["../../escape"]).await;
        let registry = IndexSkillRegistry::unchecked(&url).unwrap();
        let into = tempfile::tempdir().unwrap();
        let err = registry
            .fetch(&SkillId::new("rust-debug"), into.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing"), "{err}");
    }
}
