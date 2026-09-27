//! An index of the files in a workspace.
//!
//! Used to ground write prediction in what actually exists: a task that says
//! "fix auth.rs" and one that says "update src/auth.rs" mean the same file,
//! and only an index can tell.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Upper bound on files indexed, so a huge monorepo cannot stall a session.
const MAX_FILES: usize = 200_000;

/// Directories never worth indexing.
const SKIPPED: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".cuma",
    "dist",
    "build",
    ".venv",
];

/// The files in a workspace, by relative path and by file name.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceIndex {
    files: Vec<PathBuf>,
    by_name: BTreeMap<String, Vec<usize>>,
    truncated: bool,
}

impl WorkspaceIndex {
    /// Index `root`: `git ls-files` when it is a repository, a bounded walk
    /// otherwise. Never fails; an unreadable workspace yields an empty index.
    pub fn build(root: &Path) -> Self {
        let files = git_files(root).unwrap_or_else(|| walk(root));
        Self::from_files(files)
    }

    /// An index over an explicit list of relative paths.
    pub fn from_files(mut files: Vec<PathBuf>) -> Self {
        let truncated = files.len() > MAX_FILES;
        files.truncate(MAX_FILES);

        let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (position, file) in files.iter().enumerate() {
            if let Some(name) = file.file_name().and_then(|n| n.to_str()) {
                by_name.entry(name.to_owned()).or_default().push(position);
            }
        }

        Self {
            files,
            by_name,
            truncated,
        }
    }

    /// How many files are indexed.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing is indexed.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Whether the workspace had more files than were indexed.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Whether `path` is an indexed file, or a directory containing one.
    pub fn contains(&self, path: &Path) -> bool {
        self.files.iter().any(|f| f == path || f.starts_with(path))
    }

    /// Every indexed file named `name`.
    pub fn named(&self, name: &str) -> Vec<PathBuf> {
        self.by_name
            .get(name)
            .map(|positions| positions.iter().map(|&i| self.files[i].clone()).collect())
            .unwrap_or_default()
    }

    /// Every indexed file whose path ends with `suffix` (`auth/mod.rs`).
    pub fn ending_with(&self, suffix: &Path) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|f| f.ends_with(suffix))
            .cloned()
            .collect()
    }
}

fn git_files(root: &Path) -> Option<Vec<PathBuf>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|b| *b == 0)
            .filter(|entry| !entry.is_empty())
            .filter_map(|entry| std::str::from_utf8(entry).ok())
            .map(PathBuf::from)
            .take(MAX_FILES + 1)
            .collect(),
    )
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];

    while let Some(relative) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&relative)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = relative.join(name);

            // Symlinks are not followed: a link out of the workspace is not
            // the workspace.
            if kind.is_dir() {
                if !SKIPPED.contains(&name) {
                    pending.push(path);
                }
            } else if kind.is_file() {
                files.push(path);
                if files.len() > MAX_FILES {
                    return files;
                }
            }
        }
    }

    files
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn index(paths: &[&str]) -> WorkspaceIndex {
        WorkspaceIndex::from_files(paths.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn files_are_found_by_name_and_by_suffix() {
        let index = index(&[
            "src/auth.rs",
            "src/auth/mod.rs",
            "tests/auth.rs",
            "README.md",
        ]);
        assert_eq!(index.named("auth.rs").len(), 2);
        assert_eq!(
            index.ending_with(Path::new("auth/mod.rs")),
            vec![PathBuf::from("src/auth/mod.rs")]
        );
        assert!(index.contains(Path::new("src")));
        assert!(!index.contains(Path::new("lib")));
    }

    #[test]
    fn a_walk_skips_build_output_and_vcs_metadata() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "src/main.rs",
            "target/debug/cuma",
            ".git/HEAD",
            "node_modules/x/index.js",
        ] {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, "").unwrap();
        }

        let files = walk(dir.path());
        assert_eq!(files, vec![PathBuf::from("src/main.rs")]);
    }
}
