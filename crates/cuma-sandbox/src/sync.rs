//! Bringing an agent's work back from a sandbox that holds a copy of the
//! workspace.
//!
//! Before the agent starts, [`Snapshot::take`] records what every file in the
//! workspace contained and archives it for upload. Afterwards,
//! [`Snapshot::merge`] compares the sandbox's final copy with that record,
//! file by file:
//!
//! | In the sandbox | Here | Result |
//! |---|---|---|
//! | unchanged | anything | nothing to do |
//! | changed, created or deleted | as recorded | applied |
//! | changed, created or deleted | the same change | nothing to do |
//! | changed, created or deleted | changed otherwise | conflict |
//!
//! A single conflict applies nothing: the sandbox's copy is kept for a manual
//! merge and the task fails, rather than half of the agent's change landing.
//!
//! In a git work tree only what git tracks or would track is uploaded, so
//! dependencies and build output stay behind, and a new file the sandbox
//! created that git ignores is not brought back. `.git` travels, so the agent
//! can use git, but is never merged back: commits made in a sandbox stay
//! there, as nothing is committed on the user's behalf. CUMA's own `.cuma`
//! neither goes nor comes back.

use cuma_core::error::{MetaAgentError, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};

/// What one file held.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A regular file.
    File {
        /// SHA-256 of its contents.
        digest: [u8; 32],
        /// Whether any execute bit was set.
        executable: bool,
    },
    /// A symbolic link, and where it points.
    Link(PathBuf),
}

/// The workspace as it was handed to a sandbox.
#[derive(Debug)]
pub struct Snapshot {
    workspace: PathBuf,
    files: BTreeMap<PathBuf, Entry>,
    archive: Option<PathBuf>,
    git: bool,
}

/// What a merge changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Files created or changed here.
    pub written: usize,
    /// Files deleted here.
    pub deleted: usize,
}

impl std::fmt::Display for MergeReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} written, {} deleted", self.written, self.deleted)
    }
}

/// Run blocking file work — hashing, archiving, merging — off the async
/// runtime's workers.
async fn off_runtime<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| MetaAgentError::Other(format!("workspace transfer interrupted: {e}")))?
}

impl Snapshot {
    /// [`take`](Self::take), without blocking the async runtime.
    pub async fn take_async(workspace: &Path, dir: &Path) -> Result<std::sync::Arc<Self>> {
        let (workspace, dir) = (workspace.to_path_buf(), dir.to_path_buf());
        off_runtime(move || Self::take(&workspace, &dir).map(std::sync::Arc::new)).await
    }

    /// [`record`](Self::record), without blocking the async runtime.
    pub async fn record_async(workspace: &Path) -> Result<std::sync::Arc<Self>> {
        let workspace = workspace.to_path_buf();
        off_runtime(move || Self::record(&workspace).map(std::sync::Arc::new)).await
    }

    /// [`merge_archive`](Self::merge_archive), without blocking the async
    /// runtime.
    pub async fn merge_archive_async(
        self: std::sync::Arc<Self>,
        archive: PathBuf,
        dir: PathBuf,
        keep: PathBuf,
    ) -> Result<MergeReport> {
        off_runtime(move || self.merge_archive(&archive, &dir, &keep)).await
    }

    /// [`merge`](Self::merge), without blocking the async runtime.
    pub async fn merge_async(
        self: std::sync::Arc<Self>,
        result: PathBuf,
        keep: PathBuf,
    ) -> Result<MergeReport> {
        off_runtime(move || self.merge(&result, &keep)).await
    }

    /// Record the workspace and archive it as `dir/workspace.tar`.
    pub fn take(workspace: &Path, dir: &Path) -> Result<Self> {
        let mut snapshot = Self::record(workspace)?;
        let list = dir.join("files.list");
        let mut names: Vec<u8> = Vec::new();
        for path in snapshot.files.keys() {
            names.extend_from_slice(path.as_os_str().as_encoded_bytes());
            names.push(0);
        }
        if snapshot.workspace.join(".git").exists() {
            names.extend_from_slice(b".git\0");
        }
        std::fs::write(&list, names).map_err(io("writing the archive list"))?;

        let archive = dir.join("workspace.tar");
        let status = std::process::Command::new("tar")
            .arg("-c")
            .arg("-f")
            .arg(&archive)
            .arg("-C")
            .arg(&snapshot.workspace)
            .arg("--null")
            .arg("-T")
            .arg(&list)
            .env("COPYFILE_DISABLE", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(io("running tar"))?;
        if !status.status.success() {
            return Err(MetaAgentError::Other(format!(
                "archiving {} failed: {}",
                snapshot.workspace.display(),
                String::from_utf8_lossy(&status.stderr).trim()
            )));
        }
        snapshot.archive = Some(archive);
        Ok(snapshot)
    }

    /// Record the workspace without archiving it, for a sandbox that copies
    /// it by itself.
    pub fn record(workspace: &Path) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace).map_err(io("reading the workspace"))?;
        let git = is_git_work_tree(&workspace);
        let candidates = if git {
            git_listed(&workspace)?
        } else {
            let mut found = Vec::new();
            walk(&workspace, Path::new(""), &mut found)?;
            found
        };
        let mut files = BTreeMap::new();
        for relative in candidates {
            if excluded(&relative) {
                continue;
            }
            let full = workspace.join(&relative);
            if full.is_dir() && !full.is_symlink() {
                // A submodule, listed by git as one path.
                let mut inner = Vec::new();
                walk(&full, &relative, &mut inner)?;
                for path in inner {
                    if let Some(entry) = entry_of(&workspace.join(&path))? {
                        files.insert(path, entry);
                    }
                }
            } else if let Some(entry) = entry_of(&full)? {
                files.insert(relative, entry);
            }
        }
        Ok(Self {
            workspace,
            files,
            archive: None,
            git,
        })
    }

    /// The archive to upload, when [`take`](Self::take) made one.
    pub fn archive(&self) -> Option<&Path> {
        self.archive.as_deref()
    }

    /// How many files were handed over.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing was handed over.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Unpack `archive` — the sandbox's final copy — into `dir`, then
    /// [`merge`](Self::merge) it.
    pub fn merge_archive(&self, archive: &Path, dir: &Path, keep: &Path) -> Result<MergeReport> {
        std::fs::create_dir_all(dir).map_err(io("creating the unpack directory"))?;
        let unpacked = std::process::Command::new("tar")
            .arg("-x")
            .arg("-f")
            .arg(archive)
            .arg("-C")
            .arg(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(io("running tar"))?;
        if !unpacked.status.success() {
            return Err(MetaAgentError::Other(format!(
                "unpacking the sandbox's copy failed: {}",
                String::from_utf8_lossy(&unpacked.stderr).trim()
            )));
        }
        self.merge(dir, keep)
    }

    /// Merge the sandbox's final copy of the workspace, unpacked in
    /// `result`. On a conflict nothing is applied, `result` is moved to
    /// `keep`, and the error lists the conflicting files.
    pub fn merge(&self, result: &Path, keep: &Path) -> Result<MergeReport> {
        let mut theirs = BTreeMap::new();
        let mut found = Vec::new();
        walk(result, Path::new(""), &mut found)?;
        for relative in found.into_iter().filter(|p| !excluded(p)) {
            if let Some(entry) = entry_of(&result.join(&relative))? {
                theirs.insert(relative, entry);
            }
        }

        let created: Vec<&PathBuf> = theirs
            .keys()
            .filter(|p| !self.files.contains_key(*p))
            .collect();
        let ignored = if self.git {
            git_ignored(&self.workspace, &created)?
        } else {
            Vec::new()
        };

        let mut writes: Vec<(&PathBuf, &Entry)> = Vec::new();
        let mut deletes: Vec<&PathBuf> = Vec::new();
        let mut conflicts: Vec<&PathBuf> = Vec::new();

        for (path, entry) in &theirs {
            let base = self.files.get(path);
            if base == Some(entry) || (base.is_none() && ignored.contains(path)) {
                continue;
            }
            let ours = entry_of(&self.workspace.join(path))?;
            if ours.as_ref() == Some(entry) {
                continue;
            }
            if ours.as_ref() == base {
                writes.push((path, entry));
            } else {
                conflicts.push(path);
            }
        }
        for (path, base) in &self.files {
            if theirs.contains_key(path) {
                continue;
            }
            match entry_of(&self.workspace.join(path))? {
                None => {}
                Some(ours) if &ours == base => deletes.push(path),
                Some(_) => conflicts.push(path),
            }
        }

        if !conflicts.is_empty() {
            keep_copy(result, keep)?;
            let shown: Vec<String> = conflicts
                .iter()
                .take(10)
                .map(|p| p.display().to_string())
                .collect();
            return Err(MetaAgentError::Other(format!(
                "{} file(s) changed both in the sandbox and in the workspace ({}{}); nothing was \
                 applied, and the sandbox's copy is kept in {} for a manual merge",
                conflicts.len(),
                shown.join(", "),
                if conflicts.len() > shown.len() {
                    ", …"
                } else {
                    ""
                },
                keep.display()
            )));
        }

        // Every target is checked before anything is written, so a refused
        // path cannot leave half of the change applied.
        for path in writes
            .iter()
            .map(|(path, _)| *path)
            .chain(deletes.iter().copied())
        {
            self.inside(path)?;
        }
        for (path, entry) in &writes {
            let target = self.inside(path)?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(io("creating a directory"))?;
                self.check_inside(parent)?;
            }
            if target.is_symlink() || target.exists() {
                std::fs::remove_file(&target).map_err(io("replacing a file"))?;
            }
            match entry {
                Entry::File { executable, .. } => {
                    std::fs::copy(result.join(path), &target).map_err(io("writing a file"))?;
                    let mode = if *executable { 0o755 } else { 0o644 };
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                        .map_err(io("setting permissions"))?;
                }
                Entry::Link(points_to) => {
                    std::os::unix::fs::symlink(points_to, &target)
                        .map_err(io("creating a link"))?;
                }
            }
        }
        for path in &deletes {
            let target = self.inside(path)?;
            std::fs::remove_file(&target).map_err(io("deleting a file"))?;
        }
        Ok(MergeReport {
            written: writes.len(),
            deleted: deletes.len(),
        })
    }

    /// `relative` within the workspace, refusing anything that would leave it.
    fn inside(&self, relative: &Path) -> Result<PathBuf> {
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(MetaAgentError::Security(format!(
                "the sandbox's copy names {}, outside the workspace",
                relative.display()
            )));
        }
        let target = self.workspace.join(relative);
        // The nearest directory that exists decides where the rest would be
        // created: a link anywhere along the way is caught here.
        if let Some(existing) = target.ancestors().skip(1).find(|p| p.exists()) {
            self.check_inside(existing)?;
        }
        Ok(target)
    }

    /// Refuse a directory that resolves outside the workspace — reached
    /// through a symbolic link, say.
    fn check_inside(&self, directory: &Path) -> Result<()> {
        let resolved = std::fs::canonicalize(directory).map_err(io("resolving a directory"))?;
        if resolved.starts_with(&self.workspace) {
            Ok(())
        } else {
            Err(MetaAgentError::Security(format!(
                "{} resolves outside the workspace",
                directory.display()
            )))
        }
    }
}

/// CUMA's state and git's directory never take part in a merge.
fn excluded(relative: &Path) -> bool {
    matches!(
        relative.components().next(),
        Some(Component::Normal(first)) if first == ".cuma" || first == ".git"
    )
}

fn entry_of(path: &Path) -> Result<Option<Entry>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(MetaAgentError::Other(format!("{}: {err}", path.display()))),
    };
    if metadata.file_type().is_symlink() {
        return Ok(Some(Entry::Link(
            std::fs::read_link(path).map_err(io("reading a link"))?,
        )));
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    let mut file = std::fs::File::open(path).map_err(io("reading a file"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io("reading a file"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&hasher.finalize());
    Ok(Some(Entry::File {
        digest,
        executable: metadata.permissions().mode() & 0o111 != 0,
    }))
}

/// Every file and link under `dir`, relative to the walk's root, without
/// following links.
fn walk(dir: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(io("listing a directory"))?;
    for entry in entries {
        let entry = entry.map_err(io("listing a directory"))?;
        let name = relative.join(entry.file_name());
        if excluded(&name) {
            continue;
        }
        let kind = entry.file_type().map_err(io("listing a directory"))?;
        if kind.is_dir() {
            walk(&entry.path(), &name, out)?;
        } else if kind.is_file() || kind.is_symlink() {
            out.push(name);
        }
    }
    Ok(())
}

fn is_git_work_tree(workspace: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout.starts_with(b"true"))
}

/// What git tracks or would track: tracked files and untracked ones it does
/// not ignore, as they exist on disk.
fn git_listed(workspace: &Path) -> Result<Vec<PathBuf>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .map_err(io("running git"))?;
    if !out.status.success() {
        return Err(MetaAgentError::Other(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut paths: Vec<PathBuf> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| PathBuf::from(String::from_utf8_lossy(name).into_owned()))
        .filter(|p| workspace.join(p).symlink_metadata().is_ok())
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Which of `paths` git ignores.
fn git_ignored(workspace: &Path, paths: &[&PathBuf]) -> Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_os_str().as_encoded_bytes());
        input.push(0);
    }
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(io("running git"))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        stdin.write_all(&input).map_err(io("running git"))?;
    }
    let out = child.wait_with_output().map_err(io("running git"))?;
    // Exit 1 means "none ignored"; anything above is an error, which is
    // treated as "none ignored" — bringing a file back is the safe side.
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| PathBuf::from(String::from_utf8_lossy(name).into_owned()))
        .collect())
}

/// Keep `result` at `keep`, moving it when possible.
fn keep_copy(result: &Path, keep: &Path) -> Result<()> {
    if let Some(parent) = keep.parent() {
        std::fs::create_dir_all(parent).map_err(io("keeping the sandbox's copy"))?;
    }
    if std::fs::rename(result, keep).is_ok() {
        return Ok(());
    }
    let status = std::process::Command::new("cp")
        .arg("-R")
        .arg(result)
        .arg(keep)
        .status()
        .map_err(io("keeping the sandbox's copy"))?;
    if status.success() {
        Ok(())
    } else {
        Err(MetaAgentError::Other(format!(
            "could not keep the sandbox's copy in {}",
            keep.display()
        )))
    }
}

fn io(what: &'static str) -> impl Fn(std::io::Error) -> MetaAgentError {
    move |err| MetaAgentError::Other(format!("{what}: {err}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }

    fn read(root: &Path, path: &str) -> Option<String> {
        std::fs::read_to_string(root.join(path)).ok()
    }

    /// A workspace, its snapshot, and the sandbox's copy unpacked from the
    /// snapshot's archive — what a copied sandbox starts from.
    fn handed_over(
        setup: impl Fn(&Path),
    ) -> (tempfile::TempDir, Snapshot, tempfile::TempDir, PathBuf) {
        let ws = tempfile::tempdir().unwrap();
        setup(ws.path());
        let work = tempfile::tempdir().unwrap();
        let snapshot = Snapshot::take(ws.path(), work.path()).unwrap();
        let sandbox = work.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        let ok = std::process::Command::new("tar")
            .arg("-xf")
            .arg(snapshot.archive().unwrap())
            .arg("-C")
            .arg(&sandbox)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        (ws, snapshot, work, sandbox)
    }

    fn plain(root: &Path) {
        write(root, "keep.txt", "unchanged\n");
        write(root, "edit.txt", "before\n");
        write(root, "gone.txt", "delete me\n");
        write(root, ".cuma/runtime.db", "state");
    }

    #[test]
    fn work_done_in_the_sandbox_is_applied() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        assert!(
            !sandbox.join(".cuma").exists(),
            "CUMA's state is not uploaded"
        );
        write(&sandbox, "edit.txt", "after\n");
        write(&sandbox, "src/new.rs", "fn main() {}\n");
        std::fs::remove_file(sandbox.join("gone.txt")).unwrap();

        let report = snapshot.merge(&sandbox, &work.path().join("keep")).unwrap();

        assert_eq!(
            report,
            MergeReport {
                written: 2,
                deleted: 1
            }
        );
        assert_eq!(read(ws.path(), "edit.txt").unwrap(), "after\n");
        assert_eq!(read(ws.path(), "src/new.rs").unwrap(), "fn main() {}\n");
        assert_eq!(read(ws.path(), "gone.txt"), None);
        assert_eq!(read(ws.path(), "keep.txt").unwrap(), "unchanged\n");
        assert_eq!(read(ws.path(), ".cuma/runtime.db").unwrap(), "state");
    }

    #[test]
    fn a_file_changed_in_both_places_applies_nothing_and_keeps_the_sandbox_copy() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        write(&sandbox, "edit.txt", "agent's version\n");
        write(&sandbox, "src/new.rs", "new\n");
        write(ws.path(), "edit.txt", "user's version\n");
        let keep = ws.path().join(".cuma/sandbox-results/s1");

        let err = snapshot.merge(&sandbox, &keep).unwrap_err().to_string();

        assert!(err.contains("edit.txt"), "{err}");
        assert_eq!(read(ws.path(), "edit.txt").unwrap(), "user's version\n");
        assert_eq!(read(ws.path(), "src/new.rs"), None, "all or nothing");
        assert_eq!(read(&keep, "edit.txt").unwrap(), "agent's version\n");
        drop(work);
    }

    #[test]
    fn the_same_change_in_both_places_is_not_a_conflict() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        write(&sandbox, "edit.txt", "same\n");
        write(ws.path(), "edit.txt", "same\n");
        let report = snapshot.merge(&sandbox, &work.path().join("keep")).unwrap();
        assert_eq!(report, MergeReport::default());
    }

    #[test]
    fn a_file_deleted_in_the_sandbox_but_changed_here_is_a_conflict() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        std::fs::remove_file(sandbox.join("gone.txt")).unwrap();
        write(ws.path(), "gone.txt", "edited here\n");
        assert!(snapshot.merge(&sandbox, &work.path().join("keep")).is_err());
        assert_eq!(read(ws.path(), "gone.txt").unwrap(), "edited here\n");
    }

    #[test]
    fn the_executable_bit_comes_back() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        write(&sandbox, "run.sh", "#!/bin/sh\n");
        std::fs::set_permissions(
            sandbox.join("run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        snapshot.merge(&sandbox, &work.path().join("keep")).unwrap();
        let mode = std::fs::metadata(ws.path().join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }

    fn git(root: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn in_a_repository_ignored_files_neither_go_nor_come_back_and_git_is_not_merged() {
        let (ws, snapshot, work, sandbox) = handed_over(|root| {
            git(root, &["init", "-q"]);
            write(root, ".gitignore", "node_modules/\n");
            write(root, "app.js", "v1\n");
            write(root, "untracked.txt", "not added yet\n");
            write(root, "node_modules/dep/index.js", "huge\n");
            git(root, &["add", ".gitignore", "app.js"]);
            git(root, &["commit", "-qm", "init"]);
        });
        assert!(
            sandbox.join("untracked.txt").exists(),
            "untracked files travel"
        );
        assert!(
            !sandbox.join("node_modules").exists(),
            "ignored files stay behind"
        );
        assert!(sandbox.join(".git").exists(), "the agent can use git");

        write(&sandbox, "app.js", "v2\n");
        write(
            &sandbox,
            "node_modules/other/index.js",
            "installed in the sandbox\n",
        );
        git(&sandbox, &["commit", "-qam", "agent commit"]);

        let report = snapshot.merge(&sandbox, &work.path().join("keep")).unwrap();

        assert_eq!(
            report,
            MergeReport {
                written: 1,
                deleted: 0
            }
        );
        assert_eq!(read(ws.path(), "app.js").unwrap(), "v2\n");
        assert!(!ws.path().join("node_modules/other").exists());
        let log = std::process::Command::new("git")
            .arg("-C")
            .arg(ws.path())
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&log.stdout).contains("agent commit"),
            "commits made in the sandbox stay there"
        );
    }

    #[test]
    fn a_path_that_would_leave_the_workspace_is_refused() {
        let (ws, snapshot, work, sandbox) = handed_over(plain);
        // A directory here that is really a link to somewhere else.
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("escape")).unwrap();
        write(&sandbox, "escape/payload.txt", "x\n");
        // The sandbox's copy has a real directory where the host has a link.
        let err = snapshot.merge(&sandbox, &work.path().join("keep"));
        assert!(err.is_err());
        assert!(!outside.path().join("payload.txt").exists());
    }
}
