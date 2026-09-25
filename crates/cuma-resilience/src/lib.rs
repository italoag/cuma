//! Retry, fallback and circuit breaking.
//!
//! Three rules shape this crate:
//!
//! - **Retries are always bounded.** Every path out of [`RetryPolicy::decide`]
//!   terminates. There is no configuration that produces an infinite loop.
//! - **Failures are never silent.** Every decision is a value the caller must
//!   inspect and publish as an event, not a swallowed error.
//! - **Reaction follows classification.** The policy branches on
//!   [`cuma_core::ErrorClass`], never on an error's text.

mod breaker;
mod classify;
mod retry;

pub use breaker::{BreakerConfig, BreakerState, CircuitBreaker, CircuitBreakerRegistry};
pub use classify::classify_message;
pub use retry::{Backoff, RetryDecision, RetryPolicy};
