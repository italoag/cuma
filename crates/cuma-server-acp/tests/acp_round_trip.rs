//! The architectural goal, proven end to end.
//!
//! A real ACP **client** — the same SDK an editor uses — connects to CUMA
//! serving the ACP **agent** role, sends a prompt, and CUMA plans it, routes it
//! to a downstream agent, and streams the result back as session notifications.
//!
//! From the client's perspective there is exactly one agent.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification,
    StopReason, TextContent,
};
use agent_client_protocol::{Agent, ConnectionTo};
use cuma_config::{Config, LimitsConfig};
use cuma_core::{
    AgentDescriptor, AgentProtocol, Capability, CapabilitySet, CostProfile, Known, ModelDescriptor,
};
use cuma_orchestrator::Orchestrator;
use cuma_planner::HeuristicPlanner;
use cuma_testkit::{Behaviour, MockAgent};
use std::sync::{Arc, Mutex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// The SDK's transports speak `futures::io`; tokio's duplex speaks `tokio::io`.
/// `tokio-util`'s compat shims bridge the two.
fn byte_streams(
    stream: tokio::io::DuplexStream,
) -> agent_client_protocol::ByteStreams<
    impl futures::AsyncWrite + Send + 'static,
    impl futures::AsyncRead + Send + 'static,
> {
    let (read, write) = tokio::io::split(stream);
    agent_client_protocol::ByteStreams::new(write.compat_write(), read.compat())
}

/// Every capability the heuristic planner can ask for.
fn all_capabilities() -> CapabilitySet {
    [
        Capability::CodeComprehension,
        Capability::CodeGeneration,
        Capability::CodeEditing,
        Capability::Debugging,
        Capability::Refactoring,
        Capability::Testing,
        Capability::ShellExecution,
        Capability::FileSystem,
        Capability::VersionControl,
        Capability::Research,
        Capability::Documentation,
        Capability::Architecture,
        Capability::CodeReview,
        Capability::Planning,
        Capability::ToolUse,
    ]
    .into_iter()
    .collect()
}

fn descriptor(id: &str) -> AgentDescriptor {
    let mut agent =
        AgentDescriptor::new(id, id, AgentProtocol::Native).with_capabilities(all_capabilities());

    let mut model = ModelDescriptor::minimal(agent.id.clone(), format!("{id}-model"), id);
    model.context_window = Known::Reported(200_000);
    model.cost = CostProfile {
        input_per_mtok: Known::Reported(3.0),
        output_per_mtok: Known::Reported(15.0),
        cache_read_per_mtok: Known::Unknown,
    };
    agent.models.push(model);
    agent
}

/// Build an orchestrator backed by the given mock agents.
async fn orchestrator_with(agents: Vec<MockAgent>) -> Orchestrator {
    let config = Config {
        limits: LimitsConfig {
            max_parallel_tasks: 2,
            max_retries: 2,
            task_timeout_secs: 5,
            ..LimitsConfig::default()
        },
        ..Config::default()
    };

    let mut orchestrator = Orchestrator::new(
        config,
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );

    for agent in agents {
        orchestrator
            .add_agent(Arc::new(agent))
            .await
            .expect("add agent");
    }

    orchestrator
}

/// What one client turn observed.
struct TurnOutcome {
    stop_reason: StopReason,
    transcript: String,
}

/// Drive CUMA-as-an-ACP-agent with a real ACP client over an in-process pipe.
async fn drive(orchestrator: Orchestrator, goal: &str) -> TurnOutcome {
    // A duplex pipe stands in for the stdio an editor would use. Each side
    // reads what the other writes.
    let (client_side, server_side) = tokio::io::duplex(64 * 1024);

    let server = tokio::spawn(async move {
        let _ = cuma_server_acp::serve(orchestrator, byte_streams(server_side)).await;
    });

    let transcript = Arc::new(Mutex::new(String::new()));
    let transcript_for_handler = Arc::clone(&transcript);

    let stop_reason = agent_client_protocol::Client
        .builder()
        .name("test-editor")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if let agent_client_protocol::schema::v1::SessionUpdate::AgentMessageChunk(chunk) =
                    &notification.update
                    && let ContentBlock::Text(text) = &chunk.content
                    && let Ok(mut guard) = transcript_for_handler.lock()
                {
                    guard.push_str(&text.text);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            byte_streams(client_side),
            |connection: ConnectionTo<Agent>| {
                let goal = goal.to_owned();
                async move {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;

                    let session = connection
                        .send_request(NewSessionRequest::new(std::env::temp_dir()))
                        .block_task()
                        .await?;

                    let response = connection
                        .send_request(PromptRequest::new(
                            session.session_id,
                            vec![ContentBlock::Text(TextContent::new(goal))],
                        ))
                        .block_task()
                        .await?;

                    Ok(response.stop_reason)
                }
            },
        )
        .await
        .expect("the client turn should complete");

    server.abort();

    let transcript = transcript.lock().map(|g| g.clone()).unwrap_or_default();
    TurnOutcome {
        stop_reason,
        transcript,
    }
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_acp_client_sees_one_agent_while_cuma_routes_behind_it() {
    let worker = MockAgent::always("worker", Behaviour::ok("wrote the endpoint"))
        .with_descriptor(descriptor("worker"));
    let worker_calls = worker.call_counter();

    let outcome = drive(
        orchestrator_with(vec![worker]).await,
        "add a health endpoint",
    )
    .await;

    assert_eq!(outcome.stop_reason, StopReason::EndTurn);

    // The client saw a single conversation.
    assert!(
        outcome.transcript.contains("Planned"),
        "transcript was: {}",
        outcome.transcript
    );
    assert!(outcome.transcript.contains("wrote the endpoint"));

    // Behind it, CUMA decomposed the goal and ran several tasks.
    assert!(
        worker_calls.load(std::sync::atomic::Ordering::SeqCst) > 1,
        "the goal should have been decomposed into multiple delegated tasks"
    );
}

#[tokio::test]
async fn the_client_is_told_which_agent_each_task_was_delegated_to() {
    let outcome = drive(
        orchestrator_with(vec![
            MockAgent::always("codex", Behaviour::ok("done")).with_descriptor(descriptor("codex")),
        ])
        .await,
        "add a health endpoint",
    )
    .await;

    assert!(
        outcome.transcript.contains("delegating to codex"),
        "routing should be visible to the user: {}",
        outcome.transcript
    );
}

#[tokio::test]
async fn a_fallback_behind_the_scenes_is_reported_but_the_turn_still_succeeds() {
    // The cheap agent crashes; CUMA reroutes. The editor sees one coherent
    // conversation that mentions the recovery and still completes.
    let mut broken_descriptor = descriptor("broken");
    broken_descriptor.models[0].cost = CostProfile {
        input_per_mtok: Known::Reported(0.1),
        output_per_mtok: Known::Reported(0.5),
        cache_read_per_mtok: Known::Unknown,
    };

    let broken = MockAgent::always(
        "broken",
        Behaviour::Crash {
            message: "agent process exited with code 139".into(),
        },
    )
    .with_descriptor(broken_descriptor);

    let working = MockAgent::always("working", Behaviour::ok("recovered"))
        .with_descriptor(descriptor("working"));

    let outcome = drive(
        orchestrator_with(vec![broken, working]).await,
        "add a health endpoint",
    )
    .await;

    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert!(outcome.transcript.contains("broken failed"));
    assert!(outcome.transcript.contains("falling back"));
    assert!(
        outcome.transcript.contains("recovered"),
        "the work still got done: {}",
        outcome.transcript
    );
}

#[tokio::test]
async fn an_empty_prompt_ends_the_turn_without_planning_anything() {
    let worker =
        MockAgent::always("worker", Behaviour::ok("done")).with_descriptor(descriptor("worker"));
    let calls = worker.call_counter();

    let outcome = drive(orchestrator_with(vec![worker]).await, "   ").await;

    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing should have been spent on an empty prompt"
    );
}

#[tokio::test]
async fn a_session_that_fails_still_ends_the_turn_rather_than_refusing() {
    // An auth failure fails every task. The editor must still get a clean
    // turn ending with the failure explained, not a protocol-level refusal.
    let locked =
        MockAgent::always("locked", Behaviour::AuthFailure).with_descriptor(descriptor("locked"));

    let outcome = drive(orchestrator_with(vec![locked]).await, "add an endpoint").await;

    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert!(
        outcome.transcript.contains("locked failed"),
        "the failure should be in the transcript: {}",
        outcome.transcript
    );
}

#[tokio::test]
async fn internal_bookkeeping_does_not_leak_into_the_client_transcript() {
    let outcome = drive(
        orchestrator_with(vec![
            MockAgent::always("worker", Behaviour::ok("done"))
                .with_descriptor(descriptor("worker")),
        ])
        .await,
        "add a health endpoint",
    )
    .await;

    // The user in an editor should not see the routing scoring table, breaker
    // transitions or usage records.
    assert!(!outcome.transcript.contains("capability & quality"));
    assert!(!outcome.transcript.contains("circuit breaker"));
    assert!(!outcome.transcript.contains("Rejected:"));
}
