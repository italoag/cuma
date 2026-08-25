//! SQLite persistence for runtime state.
//!
//! ## What lives here, and what does not
//!
//! This database holds *operational* state: sessions, tasks, attempts, usage,
//! routing decisions and health. It exists so that routing history survives a
//! restart and so `cuma usage` can report on more than the current process.
//!
//! It deliberately does **not** hold project knowledge — architectural
//! decisions, conventions, findings. That belongs to the memory system, which
//! is shared across agents and tools (see ADR-005). Duplicating it here would
//! create two sources of truth that immediately diverge.
//!
//! | Data | Owner |
//! |---|---|
//! | Sessions, tasks, attempts | this database |
//! | Usage, cost, latency | this database |
//! | Routing decisions and history | this database |
//! | Agent health | this database |
//! | Project knowledge, decisions, conventions | `ai-memory` |

mod schema;
mod store;

pub use store::RuntimeStore;
