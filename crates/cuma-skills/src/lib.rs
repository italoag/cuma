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
mod local;
mod manager;
mod validation;

pub use creation::{CreationRefusal, GeneratedSkill, SkillFactory};
pub use local::LocalSkillRegistry;
pub use manager::{SkillManager, SkillOutcome};
pub use validation::{ValidationReport, validate};
