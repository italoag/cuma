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

// ---------------------------------------------------------------------------
// Several sessions, cancellation, and reloading
// ---------------------------------------------------------------------------

/// Transcripts per ACP session id, as a client saw them.
type Transcripts = Arc<Mutex<std::collections::HashMap<String, String>>>;

/// Start CUMA on a pipe and return the client's end.
fn serve_in_background(
    orchestrator: Orchestrator,
    sessions: cuma_server_acp::SessionRegistry,
) -> tokio::io::DuplexStream {
    let (client_side, server_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ =
            cuma_server_acp::serve_with(orchestrator, sessions, byte_streams(server_side)).await;
    });
    client_side
}

/// Run `body` as an ACP client over `stream`, recording every text chunk by
/// session and marking which side of the conversation it came from.
async fn with_client<R>(
    stream: tokio::io::DuplexStream,
    transcripts: &Transcripts,
    body: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<R, agent_client_protocol::Error>,
) -> R {
    let transcripts = Arc::clone(transcripts);
    agent_client_protocol::Client
        .builder()
        .name("editor")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                use agent_client_protocol::schema::v1::SessionUpdate;
                let (prefix, chunk) = match &notification.update {
                    SessionUpdate::AgentMessageChunk(chunk) => ("", chunk),
                    SessionUpdate::UserMessageChunk(chunk) => ("USER: ", chunk),
                    _ => return Ok(()),
                };
                if let ContentBlock::Text(text) = &chunk.content
                    && let Ok(mut all) = transcripts.lock()
                {
                    let entry = all.entry(notification.session_id.to_string()).or_default();
                    entry.push_str(prefix);
                    entry.push_str(&text.text);
                    entry.push('\n');
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(byte_streams(stream), body)
        .await
        .unwrap()
}

fn text(goal: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(goal.to_owned()))]
}

#[tokio::test]
async fn concurrent_sessions_see_only_their_own_work() {
    let worker = MockAgent::always(
        "worker",
        Behaviour::Slow {
            delay: std::time::Duration::from_millis(200),
            output: "slow work".into(),
        },
    )
    .with_descriptor(descriptor("worker"));

    let client_side = serve_in_background(
        orchestrator_with(vec![worker]).await,
        cuma_server_acp::SessionRegistry::new(),
    );
    let transcripts = Transcripts::default();

    let (first, second) = with_client(client_side, &transcripts, async |connection| {
        connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        let a = connection
            .send_request(NewSessionRequest::new(std::env::temp_dir()))
            .block_task()
            .await?
            .session_id;
        let b = connection
            .send_request(NewSessionRequest::new(std::env::temp_dir()))
            .block_task()
            .await?
            .session_id;

        // Both prompts are in flight at once.
        let first = connection.send_request(PromptRequest::new(a.clone(), text("write docs")));
        let second = connection.send_request(PromptRequest::new(b.clone(), text("write docs")));
        first.block_task().await?;
        second.block_task().await?;
        Ok((a.to_string(), b.to_string()))
    })
    .await;

    let all = transcripts.lock().unwrap().clone();
    for session in [&first, &second] {
        let transcript = all.get(session).cloned().unwrap_or_default();
        assert_eq!(
            transcript.matches("Planned").count(),
            1,
            "session {session} saw another session's plan:\n{transcript}"
        );
    }
}

#[tokio::test]
async fn a_client_can_cancel_a_running_prompt() {
    let worker = MockAgent::always(
        "worker",
        Behaviour::Slow {
            delay: std::time::Duration::from_secs(30),
            output: "never".into(),
        },
    )
    .with_descriptor(descriptor("worker"));

    let client_side = serve_in_background(
        orchestrator_with(vec![worker]).await,
        cuma_server_acp::SessionRegistry::new(),
    );
    let transcripts = Transcripts::default();

    let started = std::time::Instant::now();
    let stop = with_client(client_side, &transcripts, async |connection| {
        connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        let session = connection
            .send_request(NewSessionRequest::new(std::env::temp_dir()))
            .block_task()
            .await?
            .session_id;

        let pending =
            connection.send_request(PromptRequest::new(session.clone(), text("write docs")));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        connection.send_notification(
            agent_client_protocol::schema::v1::CancelNotification::new(session),
        )?;
        Ok(pending.block_task().await?.stop_reason)
    })
    .await;

    assert_eq!(stop, StopReason::Cancelled);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "cancelling must not wait for the agent to finish"
    );
}

#[tokio::test]
async fn a_persisted_session_can_be_loaded_by_a_new_process() {
    let directory = tempfile::tempdir().unwrap();

    // First "process": hold a conversation.
    let client_side = serve_in_background(
        orchestrator_with(vec![
            MockAgent::always("worker", Behaviour::ok("added the endpoint"))
                .with_descriptor(descriptor("worker")),
        ])
        .await,
        cuma_server_acp::SessionRegistry::persistent(directory.path()),
    );
    let transcripts = Transcripts::default();
    let session = with_client(client_side, &transcripts, async |connection| {
        let init = connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        assert!(
            init.agent_capabilities.load_session,
            "a persistent registry can load"
        );
        let session = connection
            .send_request(NewSessionRequest::new(std::env::temp_dir()))
            .block_task()
            .await?
            .session_id;
        connection
            .send_request(PromptRequest::new(
                session.clone(),
                text("add a health endpoint"),
            ))
            .block_task()
            .await?;
        Ok(session)
    })
    .await;

    // Second "process": nothing in memory, only what was written to disk.
    let client_side = serve_in_background(
        orchestrator_with(vec![]).await,
        cuma_server_acp::SessionRegistry::persistent(directory.path()),
    );
    let replayed = Transcripts::default();
    let loaded_id = session.clone();
    with_client(client_side, &replayed, async |connection| {
        connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        connection
            .send_request(agent_client_protocol::schema::v1::LoadSessionRequest::new(
                loaded_id,
                std::env::temp_dir(),
            ))
            .block_task()
            .await?;
        Ok(())
    })
    .await;

    // Notifications may trail the response by a moment.
    let mut transcript = String::new();
    for _ in 0..50 {
        transcript = replayed
            .lock()
            .unwrap()
            .get(&session.to_string())
            .cloned()
            .unwrap_or_default();
        if transcript.contains("added the endpoint") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        transcript.contains("USER: add a health endpoint"),
        "{transcript}"
    );
    assert!(transcript.contains("added the endpoint"), "{transcript}");
}

#[tokio::test]
async fn loading_an_unknown_session_is_an_error() {
    let directory = tempfile::tempdir().unwrap();
    let client_side = serve_in_background(
        orchestrator_with(vec![]).await,
        cuma_server_acp::SessionRegistry::persistent(directory.path()),
    );

    let result = agent_client_protocol::Client
        .builder()
        .connect_with(
            byte_streams(client_side),
            |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                Ok(connection
                    .send_request(agent_client_protocol::schema::v1::LoadSessionRequest::new(
                        agent_client_protocol::schema::v1::SessionId::new("cuma-missing"),
                        std::env::temp_dir(),
                    ))
                    .block_task()
                    .await
                    .is_err())
            },
        )
        .await
        .unwrap();

    assert!(result, "an unknown session must not load");
}

// ---------------------------------------------------------------------------
// Working directories
// ---------------------------------------------------------------------------

/// An agent that answers with the workspace it was asked to work in.
struct WhereAmI(AgentDescriptor);

#[async_trait::async_trait]
impl cuma_core::ports::AgentAdapter for WhereAmI {
    fn agent_id(&self) -> &cuma_core::AgentId {
        &self.0.id
    }
    async fn describe(&self) -> cuma_core::Result<AgentDescriptor> {
        Ok(self.0.clone())
    }
    async fn execute(
        &self,
        request: cuma_core::ports::ExecutionRequest,
        updates: tokio::sync::mpsc::Sender<cuma_core::ports::ExecutionUpdate>,
    ) -> cuma_core::Result<cuma_core::ExecutionOutcome> {
        let output = format!("WORKING IN {}", request.workspace.display());
        let _ = updates
            .send(cuma_core::ports::ExecutionUpdate::Text {
                content: output.clone(),
            })
            .await;
        Ok(cuma_core::ExecutionOutcome {
            attempt_id: cuma_core::AttemptId::generate(),
            agent_id: self.0.id.clone(),
            model_id: request.model.clone(),
            success: true,
            output,
            changed_files: Vec::new(),
            tokens: cuma_core::TokenUsage::estimated(10, 10),
            latency_ms: 1,
            failure_class: None,
            failure_reason: None,
            reported_cost_usd: None,
        })
    }
}

async fn located_orchestrator(root: std::path::PathBuf) -> Orchestrator {
    let mut orchestrator =
        Orchestrator::new(Config::default(), Arc::new(HeuristicPlanner::new()), root);
    orchestrator
        .add_agent(Arc::new(WhereAmI(descriptor("where"))))
        .await
        .unwrap();
    orchestrator
}

#[tokio::test]
async fn each_session_works_in_the_directory_its_client_named() {
    let started_in = tempfile::tempdir().unwrap();
    let project_a = tempfile::tempdir().unwrap();
    let project_b = tempfile::tempdir().unwrap();

    let build: cuma_server_acp::Builder =
        Arc::new(|path| Box::pin(async move { Ok(located_orchestrator(path).await) }));
    let workspaces = cuma_server_acp::Workspaces::per_directory(
        started_in.path().to_path_buf(),
        located_orchestrator(std::fs::canonicalize(started_in.path()).unwrap()).await,
        build,
    );

    let (client_side, server_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = cuma_server_acp::serve_workspaces(
            workspaces,
            cuma_server_acp::SessionRegistry::new(),
            byte_streams(server_side),
        )
        .await;
    });
    let transcripts = Transcripts::default();

    let (a_dir, b_dir) = (
        project_a.path().to_path_buf(),
        project_b.path().to_path_buf(),
    );
    let (a, b, refused) = with_client(client_side, &transcripts, async |connection| {
        connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        let a = connection
            .send_request(NewSessionRequest::new(a_dir.clone()))
            .block_task()
            .await?
            .session_id;
        let b = connection
            .send_request(NewSessionRequest::new(b_dir.clone()))
            .block_task()
            .await?
            .session_id;
        let refused = connection
            .send_request(NewSessionRequest::new("relative/path"))
            .block_task()
            .await
            .err()
            .map(|err| err.message);

        let first =
            connection.send_request(PromptRequest::new(a.clone(), text("explain the code")));
        let second =
            connection.send_request(PromptRequest::new(b.clone(), text("explain the code")));
        first.block_task().await?;
        second.block_task().await?;
        Ok((a.to_string(), b.to_string(), refused))
    })
    .await;

    let all = transcripts.lock().unwrap().clone();
    let canonical_a = std::fs::canonicalize(project_a.path()).unwrap();
    let canonical_b = std::fs::canonicalize(project_b.path()).unwrap();
    let transcript_a = all.get(&a).cloned().unwrap_or_default();
    let transcript_b = all.get(&b).cloned().unwrap_or_default();

    assert!(
        transcript_a.contains(&format!("WORKING IN {}", canonical_a.display())),
        "session A worked elsewhere:\n{transcript_a}"
    );
    assert!(
        transcript_b.contains(&format!("WORKING IN {}", canonical_b.display())),
        "session B worked elsewhere:\n{transcript_b}"
    );
    let refused = refused.expect("a relative directory is refused");
    assert!(refused.contains("absolute"), "{refused}");
}
