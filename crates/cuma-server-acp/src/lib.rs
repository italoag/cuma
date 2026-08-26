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

pub use session::{SessionRegistry, SessionState};
pub use translate::{advertised_capabilities, event_to_session_update, stop_reason_for};

use agent_client_protocol::schema::v1::{
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionNotification,
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

/// Serve the orchestrator as an ACP agent over stdio.
///
/// Stdio is the transport every ACP client already speaks, and it means the
/// editor owns the process lifetime — when the editor closes, CUMA exits.
///
/// **Logging must go to stderr.** Stdout is the protocol channel; a stray
/// `println!` corrupts the JSON-RPC stream.
pub async fn serve_stdio(orchestrator: Orchestrator) -> Result<()> {
    serve(orchestrator, Stdio::new()).await
}

/// Serve over an arbitrary transport, so tests can drive it in-process.
pub async fn serve<T>(orchestrator: Orchestrator, transport: T) -> Result<()>
where
    T: agent_client_protocol::ConnectTo<Agent>,
{
    let orchestrator = Arc::new(orchestrator);
    let sessions = SessionRegistry::new();

    let initialize_sessions = sessions.clone();
    let new_session_sessions = sessions.clone();
    let prompt_orchestrator = Arc::clone(&orchestrator);
    let prompt_sessions = sessions.clone();

    Agent
        .builder()
        .name(AGENT_NAME)
        // --- initialize ---------------------------------------------------
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                let _ = &initialize_sessions;
                tracing::info!(version = ?request.protocol_version, "ACP client connected");

                // Echo the client's version rather than insisting on our own:
                // the SDK negotiates, and refusing a version we can speak
                // would turn a compatible client away.
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(advertised_capabilities()),
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
        // --- session/prompt -----------------------------------------------
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                let goal = translate::prompt_to_goal(&request);
                let session_id = request.session_id.clone();

                if goal.trim().is_empty() {
                    // An empty prompt is not an error; there is simply nothing
                    // to plan. Ending the turn is the honest response.
                    return responder.respond(PromptResponse::new(
                        agent_client_protocol::schema::v1::StopReason::EndTurn,
                    ));
                }

                let cwd = prompt_sessions
                    .workspace(&session_id)
                    .await
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let _ = cwd;

                // Forward the orchestrator's events to the client as session
                // notifications while it works, so the editor shows progress
                // rather than a spinner.
                let mut events = prompt_orchestrator.events().subscribe();
                let notify_connection = connection.clone();
                let notify_session = session_id.clone();

                let pump = tokio::spawn(async move {
                    while let Ok(event) = events.recv().await {
                        let finished =
                            matches!(event.kind, cuma_core::EventKind::SessionCompleted { .. });

                        if let Some(update) = event_to_session_update(&event) {
                            let _ = notify_connection.send_notification(SessionNotification::new(
                                notify_session.clone(),
                                update,
                            ));
                        }

                        if finished {
                            break;
                        }
                    }
                });

                let outcome = prompt_orchestrator.run(&goal).await;

                // Let the pump drain rather than aborting it. The bus is
                // buffered, so a fast session can finish with notifications
                // still queued; aborting here would deliver the client a
                // stop reason and none of the work that led to it. The pump
                // exits on its own when it sees `SessionCompleted`.
                let drained = tokio::time::timeout(NOTIFICATION_DRAIN, pump).await;
                if drained.is_err() {
                    tracing::warn!("session notifications did not drain before the deadline");
                }

                let stop_reason = stop_reason_for(&outcome);

                if let Ok(result) = &outcome {
                    prompt_sessions.record(&session_id, &result.summary).await;
                }

                responder.respond(PromptResponse::new(stop_reason))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport)
        .await
        .map_err(|err| MetaAgentError::protocol_msg("acp", format!("ACP server failed: {err}")))
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
