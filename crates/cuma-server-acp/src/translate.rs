//! Translating between ACP and the domain.
//!
//! Both directions live here so the mapping is in one place: an ACP prompt
//! becomes a goal, and orchestrator events become ACP session notifications.

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, McpCapabilities, PromptCapabilities,
    PromptRequest, SessionUpdate, StopReason, TextContent,
};
use cuma_core::{Event, EventKind};
use cuma_orchestrator::SessionResult;

/// What CUMA tells a client it can do.
///
/// Deliberately conservative. Every capability advertised here is one an
/// editor may rely on, and claiming something CUMA does not yet implement
/// produces a worse experience than not claiming it.
pub fn advertised_capabilities() -> AgentCapabilities {
    AgentCapabilities::new()
        // `session/load` would mean restoring a plan mid-flight. The
        // orchestrator has no resume path yet, so this stays false.
        .load_session(false)
        .prompt_capabilities(
            PromptCapabilities::new()
                // Text only: CUMA forwards the prompt to whichever agent it
                // routes to, and cannot promise that agent accepts images.
                .image(false)
                .audio(false)
                .embedded_context(true),
        )
        // CUMA is an MCP *client*; it does not accept MCP servers from its own
        // client yet.
        .mcp_capabilities(McpCapabilities::new())
}

/// Flatten an ACP prompt into a goal string.
///
/// Only text blocks contribute. Resource links and embedded context are
/// deliberately ignored rather than stringified: an agent downstream will read
/// the workspace itself, and pasting a URI into a goal would just confuse the
/// planner's keyword matching.
pub fn prompt_to_goal(request: &PromptRequest) -> String {
    request
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wrap text as an agent message chunk.
fn message(text: impl Into<String>) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text.into(),
    ))))
}

/// Translate an orchestrator event into an ACP session notification.
///
/// Returns `None` for events an editor has no use for. Forwarding everything
/// would fill the client's transcript with bookkeeping — the whole point of
/// the meta-agent is that the user sees one coherent agent, not the internals
/// of a routing layer.
///
/// What *is* forwarded is chosen so a user can follow the work: the plan, which
/// agent took each task, streamed output, and anything that went wrong.
pub fn event_to_session_update(event: &Event) -> Option<SessionUpdate> {
    match &event.kind {
        // The agent's actual output is the main event.
        EventKind::AgentOutputReceived { chunk } => Some(message(chunk.clone())),

        EventKind::TaskPlanned { task_count } => Some(message(format!(
            "Planned {task_count} task{}.\n",
            if *task_count == 1 { "" } else { "s" }
        ))),

        // Routing is CUMA's distinguishing behaviour, so it is surfaced —
        // but as one line, not as the full scoring breakdown.
        EventKind::AgentSelected { agent, model, .. } => {
            let model = model.as_ref().map_or(String::new(), |m| format!(" / {m}"));
            Some(message(format!("→ delegating to {agent}{model}\n")))
        }

        EventKind::AgentFailed { agent, class, .. } => {
            Some(message(format!("⚠ {agent} failed ({class:?})\n")))
        }

        EventKind::FallbackSelected { from, reason, .. } => {
            Some(message(format!("↻ falling back from {from}: {reason}\n")))
        }

        EventKind::TaskFailed { reason } => Some(message(format!("✗ {reason}\n"))),

        EventKind::TaskSkipped { .. } => Some(message("⊘ skipped, a dependency failed\n")),

        // Retries, breaker transitions, usage records and task bookkeeping are
        // the harness talking to itself.
        _ => None,
    }
}

/// Map a session outcome onto an ACP stop reason.
///
/// The distinction that matters: a session that ran and failed is still
/// `EndTurn` — the turn genuinely ended, and the failure is in the transcript.
/// `Refusal` would tell the editor CUMA declined to work at all, which is a
/// different thing and would mislead the user.
pub fn stop_reason_for(outcome: &cuma_core::error::Result<SessionResult>) -> StopReason {
    match outcome {
        Ok(_) => StopReason::EndTurn,
        Err(err) => match err.class() {
            cuma_core::ErrorClass::Cancelled => StopReason::Cancelled,
            cuma_core::ErrorClass::ContextOverflow => StopReason::MaxTokens,
            // Everything else ended the turn; the reason is in the transcript.
            _ => StopReason::EndTurn,
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use agent_client_protocol::schema::v1::SessionId;
    use cuma_core::{AgentId, ErrorClass, MetaAgentError, ModelId, TaskId, TokenUsage};

    fn prompt(blocks: Vec<ContentBlock>) -> PromptRequest {
        PromptRequest::new(SessionId::new("s1"), blocks)
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(text.to_owned()))
    }

    fn event(kind: EventKind) -> Event {
        Event::session(cuma_core::SessionId::new("s1"), kind)
    }

    /// The text carried by a session update, for assertions.
    fn text_of(update: &SessionUpdate) -> String {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                ContentBlock::Text(text) => text.text.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        }
    }

    // --- prompt → goal ----------------------------------------------------

    #[test]
    fn a_text_prompt_becomes_the_goal() {
        let request = prompt(vec![text_block("implement OAuth")]);
        assert_eq!(prompt_to_goal(&request), "implement OAuth");
    }

    #[test]
    fn several_text_blocks_are_joined() {
        let request = prompt(vec![
            text_block("implement OAuth"),
            text_block("and fix the tests"),
        ]);
        assert_eq!(
            prompt_to_goal(&request),
            "implement OAuth\nand fix the tests"
        );
    }

    #[test]
    fn empty_blocks_are_dropped_rather_than_producing_blank_lines() {
        let request = prompt(vec![text_block("  "), text_block("do it"), text_block("")]);
        assert_eq!(prompt_to_goal(&request), "do it");
    }

    #[test]
    fn a_prompt_with_no_text_yields_an_empty_goal() {
        assert!(prompt_to_goal(&prompt(vec![])).is_empty());
    }

    // --- capabilities -----------------------------------------------------

    #[test]
    fn capabilities_do_not_claim_what_is_not_implemented() {
        let capabilities = advertised_capabilities();
        assert!(
            !capabilities.load_session,
            "there is no resume path yet; claiming one would break editors that use it"
        );
        assert!(!capabilities.prompt_capabilities.image);
    }

    // --- events → notifications -------------------------------------------

    #[test]
    fn streamed_output_is_forwarded_verbatim() {
        let update = event_to_session_update(&event(EventKind::AgentOutputReceived {
            chunk: "the answer is 42".into(),
        }))
        .unwrap();

        assert_eq!(text_of(&update), "the answer is 42");
    }

    #[test]
    fn routing_is_surfaced_as_one_line_not_a_scoring_table() {
        let update = event_to_session_update(&event(EventKind::AgentSelected {
            agent: AgentId::new("claude-code"),
            model: Some(ModelId::new("sonnet")),
            score: 0.87,
            explanation: "a very long breakdown\n".repeat(20),
        }))
        .unwrap();

        let text = text_of(&update);
        assert!(text.contains("claude-code"));
        assert!(text.contains("sonnet"));
        assert!(text.len() < 100, "the editor gets a summary, not internals");
    }

    #[test]
    fn failures_and_fallbacks_reach_the_client() {
        let failure = event_to_session_update(&event(EventKind::AgentFailed {
            agent: AgentId::new("codex"),
            class: ErrorClass::RateLimit,
            message: "429".into(),
        }))
        .unwrap();
        assert!(text_of(&failure).contains("codex"));

        let fallback = event_to_session_update(&event(EventKind::FallbackSelected {
            from: AgentId::new("codex"),
            to: AgentId::new("claude"),
            reason: "rate limited".into(),
        }))
        .unwrap();
        assert!(text_of(&fallback).contains("rate limited"));
    }

    #[test]
    fn internal_bookkeeping_is_not_forwarded() {
        // A user in an editor does not want retry counters and breaker
        // transitions in their transcript.
        for kind in [
            EventKind::RetryScheduled {
                attempt: 2,
                delay_ms: 500,
                reason: "transient".into(),
            },
            EventKind::CircuitBreakerChanged {
                agent: AgentId::new("a"),
                state: "Open".into(),
            },
            EventKind::UsageRecorded {
                tokens: TokenUsage::reported(10, 10),
                estimated_cost_usd: Some(0.01),
            },
            EventKind::TaskStatusChanged {
                status: cuma_core::TaskStatus::Running,
            },
        ] {
            assert!(
                event_to_session_update(&event(kind)).is_none(),
                "internal event leaked to the client"
            );
        }
    }

    #[test]
    fn a_skipped_task_is_explained_rather_than_silently_omitted() {
        let update = event_to_session_update(&event(EventKind::TaskSkipped {
            blocked_by: TaskId::new("t1"),
        }))
        .unwrap();
        assert!(text_of(&update).contains("dependency"));
    }

    #[test]
    fn the_plan_size_is_announced_with_correct_pluralisation() {
        let one =
            event_to_session_update(&event(EventKind::TaskPlanned { task_count: 1 })).unwrap();
        assert!(text_of(&one).contains("1 task."));

        let many =
            event_to_session_update(&event(EventKind::TaskPlanned { task_count: 6 })).unwrap();
        assert!(text_of(&many).contains("6 tasks."));
    }

    // --- stop reasons -----------------------------------------------------

    #[test]
    fn a_failed_session_still_ends_the_turn_rather_than_refusing() {
        // `Refusal` would tell the editor CUMA declined to work at all.
        let outcome = Err(MetaAgentError::Other("2/6 tasks completed".into()));
        assert_eq!(stop_reason_for(&outcome), StopReason::EndTurn);
    }

    #[test]
    fn cancellation_and_overflow_map_to_their_own_stop_reasons() {
        assert_eq!(
            stop_reason_for(&Err(MetaAgentError::Cancelled("user".into()))),
            StopReason::Cancelled
        );
        assert_eq!(
            stop_reason_for(&Err(MetaAgentError::agent(
                AgentId::new("a"),
                "too long",
                ErrorClass::ContextOverflow
            ))),
            StopReason::MaxTokens
        );
    }
}
