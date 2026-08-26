//! Best-effort classification of failures that arrive as free text.
//!
//! Adapters classify what they can from structured protocol data. What is left
//! is a message from an agent that died with a string, and this module maps
//! the common shapes onto [`ErrorClass`]. It is a fallback, not the primary
//! path: an adapter that can read a 429 status code must not rely on matching
//! the word "rate".

use cuma_core::ErrorClass;

/// Guess a class from an error message.
///
/// Returns [`ErrorClass::Unknown`] when nothing matches, which the retry
/// policy treats conservatively (bounded retry, then reroute) rather than
/// optimistically.
pub fn classify_message(message: &str) -> ErrorClass {
    let m = message.to_ascii_lowercase();

    // Order matters: "quota exceeded" contains neither "429" nor "rate limit",
    // but "rate limit exceeded" contains "exceeded". Check the more specific
    // quota phrasing first.
    if m.contains("quota") || m.contains("insufficient_quota") || m.contains("billing") {
        return ErrorClass::QuotaExceeded;
    }
    if m.contains("rate limit")
        || m.contains("rate_limit")
        || m.contains("429")
        || m.contains("too many requests")
    {
        return ErrorClass::RateLimit;
    }
    if m.contains("unauthorized")
        || m.contains("401")
        || m.contains("403")
        || m.contains("forbidden")
        || m.contains("invalid api key")
        || m.contains("authentication")
    {
        return ErrorClass::AuthenticationFailure;
    }
    if m.contains("context length")
        || m.contains("context window")
        || m.contains("too many tokens")
        || m.contains("maximum context")
    {
        return ErrorClass::ContextOverflow;
    }
    if m.contains("timeout") || m.contains("timed out") || m.contains("deadline exceeded") {
        return ErrorClass::Timeout;
    }
    if m.contains("connection refused")
        || m.contains("connection reset")
        || m.contains("broken pipe")
        || m.contains("dns")
        || m.contains("unreachable")
    {
        return ErrorClass::ConnectionFailure;
    }
    if m.contains("exited with")
        || m.contains("killed")
        || m.contains("signal")
        || m.contains("panicked")
        || m.contains("crash")
    {
        return ErrorClass::AgentCrash;
    }
    if m.contains("model not found")
        || m.contains("unknown model")
        || m.contains("model unavailable")
        || m.contains("overloaded")
    {
        return ErrorClass::ModelUnavailable;
    }
    if m.contains("jsonrpc") || m.contains("json-rpc") || m.contains("protocol") {
        return ErrorClass::ProtocolError;
    }
    if m.contains("cancel") {
        return ErrorClass::Cancelled;
    }

    ErrorClass::Unknown
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn http_429_is_a_rate_limit() {
        assert_eq!(
            classify_message("HTTP 429 Too Many Requests"),
            ErrorClass::RateLimit
        );
    }

    #[test]
    fn quota_wins_over_the_word_exceeded() {
        assert_eq!(
            classify_message("You exceeded your current quota"),
            ErrorClass::QuotaExceeded
        );
    }

    #[test]
    fn auth_failures_are_recognised_from_status_codes_and_prose() {
        assert_eq!(
            classify_message("401 Unauthorized"),
            ErrorClass::AuthenticationFailure
        );
        assert_eq!(
            classify_message("Invalid API key provided"),
            ErrorClass::AuthenticationFailure
        );
    }

    #[test]
    fn context_overflow_is_distinguished_from_a_generic_failure() {
        assert_eq!(
            classify_message("This model's maximum context length is 200000 tokens"),
            ErrorClass::ContextOverflow
        );
    }

    #[test]
    fn a_dead_child_process_is_a_crash() {
        assert_eq!(
            classify_message("agent process exited with code 139"),
            ErrorClass::AgentCrash
        );
    }

    #[test]
    fn an_unrecognised_message_is_unknown_rather_than_a_guess() {
        assert_eq!(
            classify_message("the flux capacitor is misaligned"),
            ErrorClass::Unknown
        );
    }

    #[test]
    fn unknown_is_conservatively_retryable_and_reroutable() {
        let class = classify_message("something odd happened");
        assert!(class.is_retryable_on_same_target());
        assert!(class.is_reroutable());
    }
}
