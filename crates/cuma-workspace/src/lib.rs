//! Workspace isolation and safety.
//!
//! Two related problems, both about agents writing where they should not.
//!
//! **Parallelism.** [`TaskGraph::ready_tasks`] computes a set of tasks with no
//! dependency between them. That does not make them safe to run concurrently:
//! two tasks with no edge can both edit `src/auth.rs`, and whichever writes
//! second silently discards the other's work. Dependency independence is not
//! workspace independence.
//!
//! **Destruction.** An agent that runs `git reset --hard` in the user's
//! repository can destroy hours of uncommitted work.
//!
//! This crate provides the mechanisms that make both safe:
//!
//! | Module | Mechanism |
//! |---|---|
//! | [`git`] | Repository detection, checkpoints, worktrees |
//! | [`ownership`] | Which task may write which path |
//! | [`guard`] | Which commands are permitted to run at all |
//! | [`sandbox`] | Confining what a command can reach |
//! | [`rtk`] | Spending fewer tokens on command output |
//!
//! [`TaskGraph::ready_tasks`]: cuma_core::TaskGraph::ready_tasks

pub mod git;
pub mod guard;
pub mod ownership;
pub mod rtk;
pub mod sandbox;

pub use git::{Checkpoint, GitWorkspace, Worktree};
pub use guard::{CommandGuard, CommandVerdict};
pub use ownership::{OwnershipLedger, WriteConflict};
pub use rtk::{Rtk, RtkStatus, Saving, estimate_tokens};
pub use sandbox::{Sandbox, SandboxStatus};
