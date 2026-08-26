//! File ownership.
//!
//! The mechanism that makes parallel execution safe. Before a task may write a
//! path, it claims it. A second task claiming the same path is refused, and
//! the orchestrator serializes it behind the first instead of letting both
//! write and losing one's work.
//!
//! Claims are on *path prefixes*, not exact paths, because a task that owns
//! `src/auth/` owns everything under it — a sibling editing
//! `src/auth/token.rs` conflicts whether or not that exact file was named.

use cuma_core::TaskId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Two tasks wanting the same path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteConflict {
    /// The path both want.
    pub path: PathBuf,
    /// The task that already holds it.
    pub held_by: TaskId,
    /// The task that was refused.
    pub requested_by: TaskId,
}

impl std::fmt::Display for WriteConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is already claimed by task {}",
            self.path.display(),
            self.held_by
        )
    }
}

/// Who may write what.
#[derive(Debug, Clone, Default)]
pub struct OwnershipLedger {
    claims: Arc<Mutex<BTreeMap<PathBuf, TaskId>>>,
}

impl OwnershipLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalize a path so `./src`, `src` and `src/` compare equal.
    fn normalize(path: &Path) -> PathBuf {
        let text = path.to_string_lossy();
        let trimmed = text
            .trim()
            .trim_start_matches("./")
            .trim_end_matches('/')
            .trim_end_matches('\\');

        PathBuf::from(if trimmed.is_empty() { "." } else { trimmed })
    }

    /// Whether `candidate` and `held` overlap.
    ///
    /// True when either contains the other: a task owning `src/` conflicts
    /// with one owning `src/auth.rs`, in both directions.
    fn overlaps(candidate: &Path, held: &Path) -> bool {
        // `.` is the workspace root and contains everything. `Path::starts_with`
        // compares components, so `src/a.rs` does *not* start with `.` — without
        // this case the pessimistic whole-workspace claim would conflict with
        // nothing at all, which is the opposite of what it is for.
        let root = Path::new(".");
        if candidate == root || held == root {
            return true;
        }

        candidate == held || candidate.starts_with(held) || held.starts_with(candidate)
    }

    /// Claim `paths` for `task`, or report the first conflict.
    ///
    /// All-or-nothing: a partial claim would leave the ledger holding paths
    /// for a task the orchestrator then declines to run.
    pub fn claim(
        &self,
        task: &TaskId,
        paths: &[PathBuf],
    ) -> std::result::Result<(), WriteConflict> {
        let Ok(mut claims) = self.claims.lock() else {
            // A poisoned ledger cannot prove a claim is safe, so refuse rather
            // than let two tasks write the same file.
            return Err(WriteConflict {
                path: paths.first().cloned().unwrap_or_default(),
                held_by: TaskId::new("unknown"),
                requested_by: task.clone(),
            });
        };

        let normalized: Vec<PathBuf> = paths.iter().map(|p| Self::normalize(p)).collect();

        for candidate in &normalized {
            for (held, holder) in claims.iter() {
                if holder != task && Self::overlaps(candidate, held) {
                    return Err(WriteConflict {
                        path: candidate.clone(),
                        held_by: holder.clone(),
                        requested_by: task.clone(),
                    });
                }
            }
        }

        for path in normalized {
            claims.insert(path, task.clone());
        }

        Ok(())
    }

    /// Whether `task` could claim `paths` right now.
    pub fn would_conflict(&self, task: &TaskId, paths: &[PathBuf]) -> Option<WriteConflict> {
        let Ok(claims) = self.claims.lock() else {
            return None;
        };

        for candidate in paths.iter().map(|p| Self::normalize(p)) {
            for (held, holder) in claims.iter() {
                if holder != task && Self::overlaps(&candidate, held) {
                    return Some(WriteConflict {
                        path: candidate,
                        held_by: holder.clone(),
                        requested_by: task.clone(),
                    });
                }
            }
        }

        None
    }

    /// Release everything `task` holds.
    ///
    /// Called when a task reaches a terminal state, successful or not. A task
    /// that failed still has to release, or its paths stay locked for the rest
    /// of the session.
    pub fn release(&self, task: &TaskId) {
        if let Ok(mut claims) = self.claims.lock() {
            claims.retain(|_, holder| holder != task);
        }
    }

    /// Paths currently held by `task`.
    pub fn held_by(&self, task: &TaskId) -> Vec<PathBuf> {
        let Ok(claims) = self.claims.lock() else {
            return Vec::new();
        };
        claims
            .iter()
            .filter(|(_, holder)| *holder == task)
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// How many paths are claimed.
    pub fn len(&self) -> usize {
        self.claims.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// Whether nothing is claimed.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Guess which paths a task will write, from its description.
///
/// A heuristic, and a deliberately *pessimistic* one: when nothing
/// path-shaped can be found, the task is assumed to want the whole workspace,
/// which serializes it against everything else. Guessing narrowly would let
/// two tasks run concurrently and corrupt each other, which is exactly the
/// failure this exists to prevent.
pub fn predicted_writes(description: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();

    for token in description.split_whitespace() {
        let cleaned = token.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-'
        });

        if cleaned.len() < 3 || !cleaned.contains('/') && !cleaned.contains('.') {
            continue;
        }

        // A bare word with a dot is usually a sentence ending, not a filename.
        let looks_like_a_path = cleaned.contains('/')
            || cleaned.rsplit('.').next().is_some_and(|ext| {
                (1..=5).contains(&ext.len()) && ext.chars().all(char::is_alphanumeric)
            });

        if looks_like_a_path && !cleaned.ends_with('.') {
            let path = PathBuf::from(cleaned);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }

    if paths.is_empty() {
        // The whole workspace: serialize against everything.
        paths.push(PathBuf::from("."));
    }

    paths
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn task(id: &str) -> TaskId {
        TaskId::new(id)
    }

    fn paths(entries: &[&str]) -> Vec<PathBuf> {
        entries.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_claim_on_a_free_path_succeeds() {
        let ledger = OwnershipLedger::new();
        assert!(ledger.claim(&task("t1"), &paths(&["src/auth.rs"])).is_ok());
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn two_tasks_cannot_claim_the_same_file() {
        let ledger = OwnershipLedger::new();
        ledger.claim(&task("t1"), &paths(&["src/auth.rs"])).unwrap();

        let conflict = ledger
            .claim(&task("t2"), &paths(&["src/auth.rs"]))
            .unwrap_err();

        assert_eq!(conflict.held_by, task("t1"));
        assert_eq!(conflict.requested_by, task("t2"));
    }

    #[test]
    fn claiming_a_directory_conflicts_with_a_file_inside_it() {
        // This is the case that makes dependency independence insufficient:
        // neither task names the same path, and they still collide.
        let ledger = OwnershipLedger::new();
        ledger.claim(&task("t1"), &paths(&["src/auth/"])).unwrap();

        assert!(
            ledger
                .claim(&task("t2"), &paths(&["src/auth/token.rs"]))
                .is_err()
        );
    }

    #[test]
    fn claiming_a_file_conflicts_with_a_directory_claimed_around_it() {
        let ledger = OwnershipLedger::new();
        ledger
            .claim(&task("t1"), &paths(&["src/auth/token.rs"]))
            .unwrap();

        assert!(ledger.claim(&task("t2"), &paths(&["src/auth"])).is_err());
    }

    #[test]
    fn sibling_directories_do_not_conflict() {
        let ledger = OwnershipLedger::new();
        ledger.claim(&task("t1"), &paths(&["src/auth"])).unwrap();

        assert!(
            ledger.claim(&task("t2"), &paths(&["src/router"])).is_ok(),
            "unrelated paths must be able to run in parallel"
        );
    }

    #[test]
    fn path_spelling_does_not_defeat_conflict_detection() {
        let ledger = OwnershipLedger::new();
        ledger.claim(&task("t1"), &paths(&["./src/auth/"])).unwrap();

        assert!(ledger.claim(&task("t2"), &paths(&["src/auth"])).is_err());
    }

    #[test]
    fn a_task_may_re_claim_what_it_already_holds() {
        let ledger = OwnershipLedger::new();
        ledger.claim(&task("t1"), &paths(&["src/auth.rs"])).unwrap();

        assert!(
            ledger.claim(&task("t1"), &paths(&["src/auth.rs"])).is_ok(),
            "a retry must not deadlock against its own claim"
        );
    }

    #[test]
    fn a_partial_conflict_claims_nothing_at_all() {
        let ledger = OwnershipLedger::new();
        ledger.claim(&task("t1"), &paths(&["src/auth.rs"])).unwrap();

        assert!(
            ledger
                .claim(&task("t2"), &paths(&["src/router.rs", "src/auth.rs"]))
                .is_err()
        );

        assert!(
            ledger.held_by(&task("t2")).is_empty(),
            "a refused claim must not leave a partial hold behind"
        );
    }

    #[test]
    fn releasing_frees_every_path_a_task_held() {
        let ledger = OwnershipLedger::new();
        ledger
            .claim(&task("t1"), &paths(&["src/a.rs", "src/b.rs"]))
            .unwrap();
        assert_eq!(ledger.held_by(&task("t1")).len(), 2);

        ledger.release(&task("t1"));

        assert!(ledger.is_empty());
        assert!(ledger.claim(&task("t2"), &paths(&["src/a.rs"])).is_ok());
    }

    #[test]
    fn releasing_one_task_does_not_free_another() {
        let ledger = OwnershipLedger::new();
        ledger.claim(&task("t1"), &paths(&["src/a.rs"])).unwrap();
        ledger.claim(&task("t2"), &paths(&["src/b.rs"])).unwrap();

        ledger.release(&task("t1"));

        assert_eq!(ledger.held_by(&task("t2")).len(), 1);
    }

    #[test]
    fn conflicts_can_be_checked_without_claiming() {
        let ledger = OwnershipLedger::new();
        ledger.claim(&task("t1"), &paths(&["src/a.rs"])).unwrap();

        assert!(
            ledger
                .would_conflict(&task("t2"), &paths(&["src/a.rs"]))
                .is_some()
        );
        assert!(
            ledger
                .would_conflict(&task("t2"), &paths(&["src/b.rs"]))
                .is_none()
        );
        assert_eq!(ledger.len(), 1, "checking must not claim");
    }

    // --- prediction -------------------------------------------------------

    #[test]
    fn a_description_naming_files_predicts_those_files() {
        let predicted = predicted_writes("Update src/auth.rs and tests/auth_test.rs");
        assert!(predicted.contains(&PathBuf::from("src/auth.rs")));
        assert!(predicted.contains(&PathBuf::from("tests/auth_test.rs")));
    }

    #[test]
    fn a_description_naming_nothing_claims_the_whole_workspace() {
        // Pessimistic on purpose: guessing narrowly lets two tasks corrupt
        // each other, which is the failure this exists to prevent.
        let predicted = predicted_writes("Implement OAuth authentication");
        assert_eq!(predicted, vec![PathBuf::from(".")]);
    }

    #[test]
    fn a_whole_workspace_claim_serializes_against_everything() {
        let ledger = OwnershipLedger::new();
        ledger
            .claim(&task("broad"), &predicted_writes("Implement OAuth"))
            .unwrap();

        assert!(
            ledger
                .claim(&task("other"), &paths(&["src/anything.rs"]))
                .is_err(),
            "an unpredictable task must not run beside anything"
        );
    }

    #[test]
    fn sentence_punctuation_is_not_mistaken_for_a_filename() {
        let predicted = predicted_writes("Run the tests. Then review them.");
        assert_eq!(predicted, vec![PathBuf::from(".")], "got {predicted:?}");
    }

    #[test]
    fn prediction_deduplicates_repeated_paths() {
        let predicted = predicted_writes("Edit src/a.rs then edit src/a.rs again");
        assert_eq!(predicted.len(), 1);
    }
}
