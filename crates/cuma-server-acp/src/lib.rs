//! CUMA as an ACP agent.
//!
//! This is the architectural goal the whole design points at. An editor
//! selects **one** agent; behind it, CUMA plans, routes across Codex, Claude
//! Code, a remote A2A reviewer and whatever else is configured, retries,
//! reroutes and accounts for it.
//!
//! ```text
//! JetBrains / Zed / VS Code
//!         │
//!        ACP
//!         ▼
//!       CUMA ──┬── ACP ──> Codex
//!              ├── ACP ──> Claude Code
//!              ├── A2A ──> remote architect
//!              └── MCP ──> git, docs, browser
//! ```
//!
//! The client half lives in `cuma-protocol-acp`. This crate is the mirror
//! image: it *implements* the agent role and forwards prompts to the
//! orchestrator, translating the orchestrator's event stream back into ACP
//! session notifications as it goes.
//!
//! Nothing ACP-shaped reaches the orchestrator. This crate is an adapter like
//! any other, just pointing the other way.

mod session;
mod translate;

pub use session::{SessionRegistry, SessionState, Turn};
pub use translate::{advertised_capabilities, event_to_session_update, stop_reason_for};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Stdio};
use cuma_core::error::{MetaAgentError, Result};
use cuma_orchestrator::Orchestrator;
use std::sync::Arc;

/// The name CUMA reports to clients during `initialize`.
pub const AGENT_NAME: &str = "cuma";

/// How long to wait for queued session notifications to reach the client
/// after a session finishes.
///
/// Bounded so a wedged client cannot hold the turn open indefinitely.
const NOTIFICATION_DRAIN: std::time::Duration = std::time::Duration::from_secs(5);

/// Serve the orchestrator as an ACP agent over stdio, with in-memory sessions.
///
/// Stdio is the transport every ACP client already speaks, and it means the
/// editor owns the process lifetime — when the editor closes, CUMA exits.
///
/// **Logging must go to stderr.** Stdout is the protocol channel; a stray
/// `println!` corrupts the JSON-RPC stream.
pub async fn serve_stdio(orchestrator: Orchestrator) -> Result<()> {
    serve(orchestrator, Stdio::new()).await
}

/// Serve over stdio with a caller-supplied session registry — a persistent
/// one makes `session/load` available.
pub async fn serve_stdio_with(orchestrator: Orchestrator, sessions: SessionRegistry) -> Result<()> {
    serve_with(orchestrator, sessions, Stdio::new()).await
}

/// Serve over an arbitrary transport, so tests can drive it in-process.
pub async fn serve<T>(orchestrator: Orchestrator, transport: T) -> Result<()>
where
    T: agent_client_protocol::ConnectTo<Agent>,
{
    serve_with(orchestrator, SessionRegistry::new(), transport).await
}

/// Wrap text as a content chunk.
fn chunk(text: &str) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_owned())))
}

/// Serve over an arbitrary transport with a caller-supplied session registry.
pub async fn serve_with<T>(
    orchestrator: Orchestrator,
    sessions: SessionRegistry,
    transport: T,
) -> Result<()>
where
    T: agent_client_protocol::ConnectTo<Agent>,
{
    let orchestrator = Arc::new(orchestrator);
    let loadable = sessions.is_persistent();

    let new_session_sessions = sessions.clone();
    let load_sessions = sessions.clone();
    let cancel_sessions = sessions.clone();
    let prompt_orchestrator = Arc::clone(&orchestrator);
    let prompt_sessions = sessions.clone();

    Agent
        .builder()
        .name(AGENT_NAME)
        // --- initialize ---------------------------------------------------
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                tracing::info!(version = ?request.protocol_version, "ACP client connected");

                // Echo the client's version rather than insisting on our own:
                // the SDK negotiates, and refusing a version we can speak
                // would turn a compatible client away.
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(advertised_capabilities().load_session(loadable)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        // --- session/new --------------------------------------------------
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _connection| {
                let session_id = new_session_sessions.create(request.cwd.clone()).await;
                tracing::info!(session = %session_id, cwd = ?request.cwd, "ACP session created");

                responder.respond(NewSessionResponse::new(session_id))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // --- session/load -------------------------------------------------
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, connection| {
                let Some(state) = load_sessions
                    .load(&request.session_id, request.cwd.clone())
                    .await
                else {
                    return responder.respond_with_error(
                        agent_client_protocol::Error::resource_not_found(Some(
                            request.session_id.to_string(),
                        )),
                    );
                };

                // The protocol asks for the whole conversation to be replayed
                // as notifications before the response.
                for turn in &state.turns {
                    let _ = connection.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::UserMessageChunk(chunk(&turn.prompt)),
                    ));
                    let _ = connection.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(chunk(&turn.response)),
                    ));
                }

                tracing::info!(session = %request.session_id, turns = state.turns.len(), "ACP session loaded");
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        // --- session/cancel -----------------------------------------------
        .on_receive_notification(
            async move |notification: CancelNotification, _connection| {
                if cancel_sessions.cancel(&notification.session_id).await {
                    tracing::info!(session = %notification.session_id, "prompt cancelled by the client");
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // --- session/prompt -----------------------------------------------
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                let goal = translate::prompt_to_goal(&request);
                let session_id = request.session_id.clone();

                if goal.trim().is_empty() {
                    // An empty prompt is not an error; there is simply nothing
                    // to plan. Ending the turn is the honest response.
                    return responder.respond(PromptResponse::new(StopReason::EndTurn));
                }

                let orchestrator = Arc::clone(&prompt_orchestrator);
                let sessions = prompt_sessions.clone();

                // The connection handles one message at a time; a prompt that
                // ran here would stall every other session — and the very
                // `session/cancel` meant to stop it — until it finished.
                connection.clone().spawn(async move {
                    let stop = run_prompt(&orchestrator, &sessions, &connection, &session_id, &goal)
                        .await;
                    if let Err(err) = responder.respond(PromptResponse::new(stop)) {
                        tracing::warn!(error = %err, "could not answer a prompt");
                    }
                    Ok(())
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport)
        .await
        .map_err(|err| MetaAgentError::protocol_msg("acp", format!("ACP server failed: {err}")))
}

/// Run one prompt, streaming its progress, and return how the turn ended.
async fn run_prompt(
    orchestrator: &Arc<Orchestrator>,
    sessions: &SessionRegistry,
    connection: &agent_client_protocol::ConnectionTo<agent_client_protocol::Client>,
    session_id: &agent_client_protocol::schema::v1::SessionId,
    goal: &str,
) -> StopReason {
    // Each prompt runs as its own orchestrator session. Its id is chosen here,
    // before anything starts, so the pump can keep this prompt's events and
    // drop every other session's: the bus is shared by every client.
    let run_id = cuma_core::SessionId::generate();
    let mut events = orchestrator.events().subscribe();

    let pump_connection = connection.clone();
    let pump_session = session_id.clone();
    let pump_run = run_id.clone();
    let pump = tokio::spawn(async move {
        let mut transcript = String::new();
        while let Ok(event) = events.recv().await {
            if event.session_id != pump_run {
                continue;
            }
            let finished = matches!(event.kind, cuma_core::EventKind::SessionCompleted { .. });

            if let cuma_core::EventKind::AgentOutputReceived { chunk } = &event.kind {
                transcript.push_str(chunk);
            }
            if let Some(update) = event_to_session_update(&event) {
                let _ = pump_connection
                    .send_notification(SessionNotification::new(pump_session.clone(), update));
            }
            if finished {
                break;
            }
        }
        transcript
    });

    let run = {
        let orchestrator = Arc::clone(orchestrator);
        let goal = goal.to_owned();
        tokio::spawn(async move { orchestrator.run_session(run_id, &goal).await })
    };

    if !sessions.start(session_id, run.abort_handle()).await {
        run.abort();
        pump.abort();
        let _ = connection.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::AgentMessageChunk(chunk(
                "A prompt is already running in this session; cancel it first.\n",
            )),
        ));
        return StopReason::Refusal;
    }

    let outcome = match run.await {
        Ok(outcome) => Some(outcome),
        Err(err) if err.is_cancelled() => None,
        Err(err) => Some(Err(MetaAgentError::protocol_msg(
            "acp",
            format!("the session task failed: {err}"),
        ))),
    };
    sessions.finish(session_id).await;

    let Some(outcome) = outcome else {
        // Cancelled: nothing more is coming, so the pump is stopped rather
        // than left waiting for a completion event that will never arrive.
        pump.abort();
        sessions.record(session_id, goal, "(cancelled)").await;
        return StopReason::Cancelled;
    };

    // Let the pump drain rather than aborting it. The bus is buffered, so a
    // fast session can finish with notifications still queued; aborting here
    // would deliver the client a stop reason and none of the work that led to
    // it. The pump exits on its own when it sees `SessionCompleted`.
    let transcript = match tokio::time::timeout(NOTIFICATION_DRAIN, pump).await {
        Ok(Ok(transcript)) => transcript,
        _ => {
            tracing::warn!("session notifications did not drain before the deadline");
            String::new()
        }
    };

    let response = match &outcome {
        Ok(result) if transcript.trim().is_empty() => result.summary.clone(),
        Ok(result) => format!("{}\n\n{}", transcript.trim_end(), result.summary),
        Err(err) => err.to_string(),
    };
    sessions.record(session_id, goal, &response).await;

    stop_reason_for(&outcome)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn the_agent_advertises_a_stable_name() {
        assert_eq!(AGENT_NAME, "cuma");
    }
}
