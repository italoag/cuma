//! Git-backed workspace safety.
//!
//! Two mechanisms:
//!
//! **Checkpoints.** Before a task that may write, the working tree is stashed
//! into a named commit that is not on any branch. If the task goes wrong, the
//! user has something to go back to. Uncommitted work is the thing an agent
//! can destroy that git cannot otherwise recover.
//!
//! **Worktrees.** A task can be given its own checkout of the repository, so
//! two tasks writing concurrently write to different directories. This is what
//! turns the dependency-independent frontier into a genuinely parallel one.

use cuma_core::error::{MetaAgentError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a git invocation may take.
///
/// Generous, because a checkpoint in a large repository is not instant, but
/// bounded so a hung git cannot stall the whole session.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// A saved state the user can return to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// The commit holding the checkpoint.
    pub commit: String,
    /// What it was taken before.
    pub label: String,
    /// Whether there was anything uncommitted to save.
    pub had_changes: bool,
    /// When it was taken.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl Checkpoint {
    /// How a user restores this checkpoint.
    ///
    /// Printed rather than performed: restoring is destructive in the other
    /// direction, and that is the user's decision to make.
    pub fn restore_hint(&self) -> String {
        if !self.had_changes {
            return "nothing was uncommitted, so there is nothing to restore".to_owned();
        }
        format!("git stash apply {}", self.commit)
    }
}

/// An isolated checkout for one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Where it lives.
    pub path: PathBuf,
    /// The branch it is on.
    pub branch: String,
    /// The repository it belongs to.
    pub repository: PathBuf,
}

/// Git operations against one repository.
#[derive(Debug, Clone)]
pub struct GitWorkspace {
    root: PathBuf,
    is_repository: bool,
}

impl GitWorkspace {
    /// Detect whether `root` is inside a git repository.
    ///
    /// Not being one is not an error: CUMA must work in a plain directory. It
    /// only means checkpoints and worktrees are unavailable, which the caller
    /// learns from [`GitWorkspace::is_repository`].
    pub async fn detect(root: &Path) -> Self {
        let is_repository = run_git(root, &["rev-parse", "--git-dir"]).await.is_ok();

        if !is_repository {
            tracing::info!(
                root = %root.display(),
                "not a git repository; checkpoints and worktrees are unavailable"
            );
        }

        Self {
            root: root.to_path_buf(),
            is_repository,
        }
    }

    /// Whether this is a git repository.
    pub fn is_repository(&self) -> bool {
        self.is_repository
    }

    /// The repository root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the working tree has uncommitted changes.
    pub async fn has_uncommitted_changes(&self) -> Result<bool> {
        if !self.is_repository {
            return Ok(false);
        }

        let output = run_git(&self.root, &["status", "--porcelain"]).await?;
        Ok(!output.trim().is_empty())
    }

    /// Files with uncommitted changes.
    pub async fn dirty_files(&self) -> Result<Vec<String>> {
        if !self.is_repository {
            return Ok(Vec::new());
        }

        let output = run_git(&self.root, &["status", "--porcelain"]).await?;
        Ok(output
            .lines()
            .filter_map(|line| line.get(3..).map(str::trim).map(str::to_owned))
            .filter(|path| !path.is_empty())
            .collect())
    }

    /// Save the working tree so the user can get back to it.
    ///
    /// Uses `git stash create`, which writes a commit **without** touching the
    /// working tree or the stash list. The agent then works on exactly what was
    /// there — a checkpoint that changed what the agent sees would alter the
    /// task it was given.
    pub async fn checkpoint(&self, label: &str) -> Result<Checkpoint> {
        if !self.is_repository {
            return Err(MetaAgentError::Configuration(
                "cannot checkpoint outside a git repository".to_owned(),
            ));
        }

        let had_changes = self.has_uncommitted_changes().await?;

        if !had_changes {
            // HEAD is already the recovery point.
            let head = run_git(&self.root, &["rev-parse", "HEAD"])
                .await
                .unwrap_or_default();

            return Ok(Checkpoint {
                commit: head.trim().to_owned(),
                label: label.to_owned(),
                had_changes: false,
                created_at: chrono::Utc::now(),
            });
        }

        let commit = run_git(&self.root, &["stash", "create", label]).await?;
        let commit = commit.trim().to_owned();

        if commit.is_empty() {
            return Err(MetaAgentError::Other(
                "git reported changes but produced no checkpoint commit".to_owned(),
            ));
        }

        // Anchor it so garbage collection cannot reclaim it before the user
        // looks. An unreferenced commit is not a safety net.
        let reference = format!("refs/cuma/checkpoints/{}", sanitize_ref(label));
        run_git(&self.root, &["update-ref", &reference, &commit]).await?;

        tracing::info!(commit = commit, label, "created a workspace checkpoint");

        Ok(Checkpoint {
            commit,
            label: label.to_owned(),
            had_changes: true,
            created_at: chrono::Utc::now(),
        })
    }

    /// Create an isolated checkout for a task.
    ///
    /// The branch is derived from the task id so two tasks cannot collide, and
    /// the worktree is placed outside the repository so it never appears as an
    /// untracked directory in the user's own status output.
    pub async fn create_worktree(&self, task_id: &str, base: &Path) -> Result<Worktree> {
        if !self.is_repository {
            return Err(MetaAgentError::Configuration(
                "cannot create a worktree outside a git repository".to_owned(),
            ));
        }

        let slug = sanitize_ref(task_id);
        let branch = format!("cuma/{slug}");
        let path = base.join(&slug);

        if path.exists() {
            return Err(MetaAgentError::Other(format!(
                "worktree path {} already exists",
                path.display()
            )));
        }

        let path_text = path.to_string_lossy().to_string();
        run_git(
            &self.root,
            &["worktree", "add", "-b", &branch, &path_text, "HEAD"],
        )
        .await?;

        tracing::info!(path = %path.display(), branch, "created an isolated worktree");

        Ok(Worktree {
            path,
            branch,
            repository: self.root.clone(),
        })
    }

    /// Remove a worktree and its branch.
    ///
    /// Best effort: a worktree left behind is untidy, but failing a session
    /// because cleanup did not work would be worse.
    pub async fn remove_worktree(&self, worktree: &Worktree) -> Result<()> {
        let path = worktree.path.to_string_lossy().to_string();

        if let Err(err) = run_git(&self.root, &["worktree", "remove", "--force", &path]).await {
            tracing::warn!(path, error = %err, "could not remove a worktree");
            return Ok(());
        }

        let _ = run_git(&self.root, &["branch", "-D", &worktree.branch]).await;
        Ok(())
    }

    /// Merge a worktree's branch back into the current branch.
    ///
    /// Returns the merge output on success. A conflict is an error rather than
    /// a partially merged tree: leaving conflict markers in the user's working
    /// copy is worse than reporting that the merge needs a human.
    pub async fn merge_worktree(&self, worktree: &Worktree) -> Result<String> {
        // Commit whatever the agent left in the worktree, or there is nothing
        // to merge.
        let _ = run_git(&worktree.path, &["add", "-A"]).await;
        let _ = run_git(
            &worktree.path,
            &["commit", "-m", &format!("cuma: {}", worktree.branch)],
        )
        .await;

        match run_git(&self.root, &["merge", "--no-ff", &worktree.branch]).await {
            Ok(output) => Ok(output),
            Err(err) => {
                // Leave the user's tree as it was.
                let _ = run_git(&self.root, &["merge", "--abort"]).await;

                Err(MetaAgentError::Other(format!(
                    "merging {} needs a human: {err}",
                    worktree.branch
                )))
            }
        }
    }

    /// Checkpoints CUMA has created in this repository.
    pub async fn checkpoints(&self) -> Result<Vec<String>> {
        if !self.is_repository {
            return Ok(Vec::new());
        }

        let output = run_git(
            &self.root,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/cuma/checkpoints",
            ],
        )
        .await?;

        Ok(output.lines().map(str::to_owned).collect())
    }
}

/// Make a string safe to use in a git ref.
///
/// Git refs disallow spaces, `~`, `^`, `:`, `?`, `*`, `[`, `\` and `..`, and a
/// task description can contain any of them.
fn sanitize_ref(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let trimmed = cleaned.trim_matches('-');
    let bounded: String = trimmed.chars().take(60).collect();

    if bounded.is_empty() {
        "unnamed".to_owned()
    } else {
        bounded
    }
}

/// Run git in `directory` and return stdout.
async fn run_git(directory: &Path, args: &[&str]) -> Result<String> {
    let mut command = tokio::process::Command::new("git");
    command.current_dir(directory).args(args);
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| MetaAgentError::Timeout {
            operation: format!("git {}", args.join(" ")),
            elapsed_ms: GIT_TIMEOUT.as_millis() as u64,
        })?
        .map_err(|err| MetaAgentError::Other(format!("cannot run git: {err}")))?;

    if !output.status.success() {
        return Err(MetaAgentError::Other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// A throwaway git repository with one commit.
    async fn repository() -> Option<(tempfile::TempDir, GitWorkspace)> {
        let directory = tempfile::tempdir().ok()?;
        let path = directory.path();

        run_git(path, &["init", "-q"]).await.ok()?;
        run_git(path, &["config", "user.email", "test@example.invalid"])
            .await
            .ok()?;
        run_git(path, &["config", "user.name", "Test"]).await.ok()?;

        tokio::fs::write(path.join("README.md"), "hello")
            .await
            .ok()?;
        run_git(path, &["add", "-A"]).await.ok()?;
        run_git(path, &["commit", "-qm", "initial"]).await.ok()?;

        let workspace = GitWorkspace::detect(path).await;
        Some((directory, workspace))
    }

    #[tokio::test]
    async fn a_plain_directory_is_not_a_repository_and_that_is_not_an_error() {
        let directory =
            std::env::temp_dir().join(format!("cuma-not-a-repo-{}", std::process::id()));
        tokio::fs::create_dir_all(&directory).await.ok();

        let workspace = GitWorkspace::detect(&directory).await;

        // CUMA must work in a directory that is not under version control.
        assert!(!workspace.has_uncommitted_changes().await.unwrap());
        assert!(workspace.dirty_files().await.unwrap().is_empty());
        assert!(workspace.checkpoints().await.unwrap().is_empty());

        tokio::fs::remove_dir_all(&directory).await.ok();
    }

    #[tokio::test]
    async fn checkpointing_outside_a_repository_is_refused_rather_than_silently_skipped() {
        let workspace = GitWorkspace {
            root: std::env::temp_dir(),
            is_repository: false,
        };

        let err = workspace.checkpoint("before writing").await.unwrap_err();
        assert_eq!(err.class(), cuma_core::ErrorClass::Configuration);
    }

    #[tokio::test]
    async fn a_clean_repository_checkpoints_to_head_with_nothing_to_restore() {
        let Some((_dir, workspace)) = repository().await else {
            return; // git unavailable
        };

        assert!(!workspace.has_uncommitted_changes().await.unwrap());

        let checkpoint = workspace.checkpoint("before writing").await.unwrap();
        assert!(!checkpoint.had_changes);
        assert!(checkpoint.restore_hint().contains("nothing"));
    }

    #[tokio::test]
    async fn uncommitted_work_is_saved_without_disturbing_the_working_tree() {
        let Some((dir, workspace)) = repository().await else {
            return;
        };

        tokio::fs::write(dir.path().join("README.md"), "edited")
            .await
            .unwrap();
        assert!(workspace.has_uncommitted_changes().await.unwrap());

        let checkpoint = workspace.checkpoint("before writing").await.unwrap();

        assert!(checkpoint.had_changes);
        assert!(!checkpoint.commit.is_empty());
        assert!(checkpoint.restore_hint().contains("stash apply"));

        // The agent must see exactly what was there: a checkpoint that reverted
        // the tree would change the task it was given.
        let content = tokio::fs::read_to_string(dir.path().join("README.md"))
            .await
            .unwrap();
        assert_eq!(content, "edited", "the checkpoint must not revert anything");
        assert!(workspace.has_uncommitted_changes().await.unwrap());
    }

    #[tokio::test]
    async fn a_checkpoint_is_anchored_so_gc_cannot_reclaim_it() {
        let Some((dir, workspace)) = repository().await else {
            return;
        };

        tokio::fs::write(dir.path().join("README.md"), "edited")
            .await
            .unwrap();
        workspace.checkpoint("task-1").await.unwrap();

        let refs = workspace.checkpoints().await.unwrap();
        assert!(
            refs.iter().any(|r| r.contains("task-1")),
            "an unreferenced commit is not a safety net: {refs:?}"
        );
    }

    #[tokio::test]
    async fn dirty_files_are_listed() {
        let Some((dir, workspace)) = repository().await else {
            return;
        };

        tokio::fs::write(dir.path().join("new.txt"), "x")
            .await
            .unwrap();

        let dirty = workspace.dirty_files().await.unwrap();
        assert!(dirty.iter().any(|f| f.contains("new.txt")), "got {dirty:?}");
    }

    #[tokio::test]
    async fn a_worktree_is_an_independent_checkout() {
        let Some((dir, workspace)) = repository().await else {
            return;
        };

        let base = dir
            .path()
            .parent()
            .unwrap()
            .join(format!("cuma-worktrees-{}", std::process::id()));
        tokio::fs::create_dir_all(&base).await.unwrap();

        let worktree = workspace
            .create_worktree("task_abc123", &base)
            .await
            .unwrap();

        assert!(worktree.path.exists());
        assert!(worktree.path.join("README.md").exists());
        assert_eq!(worktree.branch, "cuma/task_abc123");

        // Writing in the worktree must not touch the main checkout — that is
        // the whole point.
        tokio::fs::write(worktree.path.join("README.md"), "worktree edit")
            .await
            .unwrap();

        let main = tokio::fs::read_to_string(dir.path().join("README.md"))
            .await
            .unwrap();
        assert_eq!(main, "hello", "the main checkout must be untouched");

        workspace.remove_worktree(&worktree).await.unwrap();
        tokio::fs::remove_dir_all(&base).await.ok();
    }

    #[tokio::test]
    async fn creating_a_worktree_outside_a_repository_is_refused() {
        let workspace = GitWorkspace {
            root: std::env::temp_dir(),
            is_repository: false,
        };

        assert!(
            workspace
                .create_worktree("t1", &std::env::temp_dir())
                .await
                .is_err()
        );
    }

    // --- ref sanitization -------------------------------------------------

    #[test]
    fn task_descriptions_are_made_safe_for_a_git_ref() {
        // Git refs disallow spaces, `~`, `^`, `:`, `?`, `*`, `[`, `\` and `..`.
        assert_eq!(
            sanitize_ref("implement OAuth: phase 2"),
            "implement-OAuth--phase-2"
        );
        assert_eq!(sanitize_ref("a~b^c:d?e*f[g"), "a-b-c-d-e-f-g");
        assert_eq!(sanitize_ref("../../etc/passwd"), "etc-passwd");
    }

    #[test]
    fn an_empty_or_symbol_only_name_still_produces_a_usable_ref() {
        assert_eq!(sanitize_ref(""), "unnamed");
        assert_eq!(sanitize_ref("///"), "unnamed");
    }

    #[test]
    fn a_very_long_name_is_bounded() {
        assert!(sanitize_ref(&"x".repeat(500)).len() <= 60);
    }
}
