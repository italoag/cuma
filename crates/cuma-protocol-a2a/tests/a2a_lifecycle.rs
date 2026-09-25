//! A2A over real HTTP: CUMA's client against CUMA's server, and against
//! scripted peers that reproduce the behaviours a remote agent can exhibit —
//! the 0.3 dialect, pausing for input, answering slowly, and never finishing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::Json;
use axum::routing::post;
use cuma_core::ports::{AgentAdapter, ExecutionUpdate};
use cuma_core::{
    AgentDescriptor, AgentProtocol, Capability, CapabilitySet, ErrorClass, Known, ModelDescriptor,
    Task, TaskSpec, TaskType,
};
use cuma_orchestrator::Orchestrator;
use cuma_planner::HeuristicPlanner;
use cuma_protocol_a2a::wire::Dialect;
use cuma_protocol_a2a::{A2aAdapter, A2aServer};
use cuma_testkit::{Behaviour, MockAgent};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn every_capability() -> CapabilitySet {
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

async fn cuma_with(behaviour: Behaviour) -> Arc<Orchestrator> {
    let mut descriptor = AgentDescriptor::new("worker", "worker", AgentProtocol::Native)
        .with_capabilities(every_capability());
    let mut model = ModelDescriptor::minimal(descriptor.id.clone(), "worker-model", "worker");
    model.context_window = Known::Reported(200_000);
    descriptor.models.push(model);

    let mut config = cuma_config::Config::default();
    config.limits.task_timeout_secs = 30;
    config.limits.max_retries = 0;

    let mut orchestrator = Orchestrator::new(
        config,
        Arc::new(HeuristicPlanner::new()),
        std::env::temp_dir(),
    );
    orchestrator
        .add_agent(Arc::new(
            MockAgent::always("worker", behaviour).with_descriptor(descriptor),
        ))
        .await
        .unwrap();
    Arc::new(orchestrator)
}

/// Serve `router` on an ephemeral local port and return its base URL.
async fn listen(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{address}/")
}

async fn serve_cuma(behaviour: Behaviour) -> String {
    let orchestrator = cuma_with(behaviour).await;
    // The card must name the address it is served on, which is only known
    // once bound; bind first, then build the server around it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let server = Arc::new(A2aServer::new(orchestrator, &base).await);
    let router = server.router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    base
}

async fn rpc(base: &str, method: &str, params: Value) -> Value {
    reqwest::Client::new()
        .post(base)
        .json(&json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn request(timeout_ms: u64) -> cuma_core::ports::ExecutionRequest {
    let task = Task::new(TaskSpec::new("say hello", TaskType::Research));
    cuma_core::ports::ExecutionRequest {
        task,
        model: None,
        prompt: "say hello".to_owned(),
        workspace: std::env::temp_dir(),
        handoff: None,
        timeout_ms,
    }
}

fn v1_message(text: &str) -> Value {
    json!({ "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "text": text }] } })
}

// ---------------------------------------------------------------------------
// CUMA to CUMA
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cuma_delegates_to_cuma_over_a2a_1_0_with_streaming() {
    let base = serve_cuma(Behaviour::ok("remote says hello")).await;

    let adapter = A2aAdapter::new("remote", base.clone()).unwrap();
    let descriptor = adapter.refresh_from_card().await.unwrap();
    assert_eq!(descriptor.name, "CUMA");
    assert_eq!(
        descriptor.metadata.get("a2a_streaming").map(String::as_str),
        Some("true")
    );
    assert_eq!(adapter.dialect().await, Dialect::V1);

    let (tx, mut rx) = mpsc::channel(64);
    let outcome = adapter.execute(request(20_000), tx).await.unwrap();

    assert!(outcome.success, "{:?}", outcome.failure_reason);
    assert!(
        outcome.output.contains("remote says hello"),
        "{}",
        outcome.output
    );
    assert!(
        !outcome.tokens.reported,
        "A2A reports no tokens; ours are estimates"
    );

    let mut streamed = String::new();
    while let Ok(ExecutionUpdate::Text { content }) = rx.try_recv() {
        streamed.push_str(&content);
    }
    assert!(
        streamed.contains("remote says hello"),
        "output should arrive as stream events, got {streamed:?}"
    );
}

#[tokio::test]
async fn a_task_started_without_waiting_can_be_polled_to_completion() {
    let base = serve_cuma(Behaviour::Slow {
        delay: Duration::from_millis(300),
        output: "slow result".into(),
    })
    .await;

    let mut params = v1_message("say hello");
    params["configuration"] = json!({ "returnImmediately": true });
    let started = rpc(&base, "SendMessage", params).await;

    let task = &started["result"]["task"];
    let id = task["id"].as_str().unwrap().to_owned();
    let state = task["status"]["state"].as_str().unwrap();
    assert!(
        state == "TASK_STATE_SUBMITTED" || state == "TASK_STATE_WORKING",
        "returned before settling, got {state}"
    );

    let mut last = Value::Null;
    for _ in 0..100 {
        last = rpc(&base, "GetTask", json!({ "id": id })).await;
        if last["result"]["status"]["state"] == "TASK_STATE_COMPLETED" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(last["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    assert!(
        last["result"]["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("slow result")
    );
}

#[tokio::test]
async fn a_running_task_can_be_cancelled_exactly_once() {
    let base = serve_cuma(Behaviour::Slow {
        delay: Duration::from_secs(20),
        output: "never".into(),
    })
    .await;

    let mut params = v1_message("say hello");
    params["configuration"] = json!({ "returnImmediately": true });
    let started = rpc(&base, "SendMessage", params).await;
    let id = started["result"]["task"]["id"].as_str().unwrap().to_owned();

    let cancelled = rpc(&base, "CancelTask", json!({ "id": id })).await;
    assert_eq!(
        cancelled["result"]["status"]["state"],
        "TASK_STATE_CANCELED"
    );

    let fetched = rpc(&base, "GetTask", json!({ "id": id })).await;
    assert_eq!(fetched["result"]["status"]["state"], "TASK_STATE_CANCELED");

    let again = rpc(&base, "CancelTask", json!({ "id": id })).await;
    assert_eq!(
        again["error"]["code"], -32002,
        "a settled task is not cancelable"
    );
}

#[tokio::test]
async fn tasks_can_be_listed_by_context() {
    let base = serve_cuma(Behaviour::ok("ok")).await;

    for context in ["alpha", "alpha", "beta"] {
        let mut params = v1_message("say hello");
        params["message"]["contextId"] = json!(context);
        rpc(&base, "SendMessage", params).await;
    }

    let listed = rpc(&base, "ListTasks", json!({ "contextId": "alpha" })).await;
    assert_eq!(listed["result"]["totalSize"], 2);
    assert_eq!(listed["result"]["tasks"].as_array().unwrap().len(), 2);

    let paged = rpc(&base, "ListTasks", json!({ "pageSize": 1 })).await;
    assert_eq!(paged["result"]["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(paged["result"]["nextPageToken"], "1");
}

#[tokio::test]
async fn a_0_3_caller_is_answered_in_the_0_3_dialect() {
    let base = serve_cuma(Behaviour::ok("legacy ok")).await;

    let response = rpc(
        &base,
        "message/send",
        json!({ "message": { "kind": "message", "messageId": "m", "role": "user",
                             "parts": [{ "kind": "text", "text": "say hello" }] } }),
    )
    .await;

    assert_eq!(response["result"]["kind"], "task");
    assert_eq!(response["result"]["status"]["state"], "completed");
}

#[tokio::test]
async fn the_published_card_is_a_1_0_card() {
    let base = serve_cuma(Behaviour::ok("ok")).await;
    let card: Value = reqwest::get(format!("{base}.well-known/agent-card.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(card["supportedInterfaces"][0]["protocolBinding"], "JSONRPC");
    assert_eq!(card["supportedInterfaces"][0]["protocolVersion"], "1.0");
    assert_eq!(card["capabilities"]["streaming"], true);
    assert_eq!(card["capabilities"]["pushNotifications"], false);
}

// ---------------------------------------------------------------------------
// Scripted peers
// ---------------------------------------------------------------------------

/// A peer that speaks only 0.3.
#[tokio::test]
async fn a_peer_that_only_speaks_0_3_is_reached_by_falling_back() {
    let router = axum::Router::new().route(
        "/",
        post(|Json(body): Json<Value>| async move {
            let id = body["id"].clone();
            Json(match body["method"].as_str() {
                Some("message/send") => json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "kind": "task", "id": "t1", "contextId": "c1",
                    "status": { "state": "completed" },
                    "artifacts": [{ "artifactId": "a", "parts": [{ "kind": "text", "text": "old but fine" }] }],
                }}),
                _ => json!({ "jsonrpc": "2.0", "id": id,
                             "error": { "code": -32601, "message": "Method not found" } }),
            })
        }),
    );
    let base = listen(router).await;

    let adapter = A2aAdapter::new("old", base).unwrap();
    let (tx, _rx) = mpsc::channel(8);
    let outcome = adapter.execute(request(5_000), tx).await.unwrap();

    assert!(outcome.success, "{:?}", outcome.failure_reason);
    assert!(outcome.output.contains("old but fine"));
    assert_eq!(
        adapter.dialect().await,
        Dialect::Legacy,
        "the fallback is remembered"
    );
}

/// A peer that stops to ask a question.
#[tokio::test]
async fn a_peer_asking_for_input_is_a_failure_not_a_success() {
    let router = axum::Router::new().route(
        "/",
        post(|Json(body): Json<Value>| async move {
            Json(json!({ "jsonrpc": "2.0", "id": body["id"], "result": { "task": {
                "id": "t1", "contextId": "c1",
                "status": { "state": "TASK_STATE_INPUT_REQUIRED",
                            "message": { "role": "ROLE_AGENT", "parts": [{ "text": "which database?" }] } },
            }}}))
        }),
    );
    let base = listen(router).await;

    let adapter = A2aAdapter::new("asker", base).unwrap();
    let (tx, _rx) = mpsc::channel(8);
    let outcome = adapter.execute(request(5_000), tx).await.unwrap();

    assert!(!outcome.success);
    assert_eq!(outcome.failure_class, Some(ErrorClass::TaskFailure));
    assert!(outcome.failure_reason.unwrap().contains("which database?"));
}

/// A peer whose task finishes only after being polled.
#[tokio::test]
async fn a_task_still_working_is_polled_until_it_completes() {
    let polls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&polls);

    let router = axum::Router::new().route(
        "/",
        post(move |Json(body): Json<Value>| {
            let counter = Arc::clone(&counter);
            async move {
                let id = body["id"].clone();
                let state = match body["method"].as_str() {
                    Some("SendMessage") => "TASK_STATE_WORKING",
                    Some("GetTask") if counter.fetch_add(1, Ordering::SeqCst) >= 1 => {
                        "TASK_STATE_COMPLETED"
                    }
                    _ => "TASK_STATE_WORKING",
                };
                let task = json!({
                    "id": "t1", "contextId": "c1", "status": { "state": state },
                    "artifacts": [{ "artifactId": "a", "parts": [{ "text": "eventually" }] }],
                });
                Json(if body["method"] == "SendMessage" {
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "task": task } })
                } else {
                    json!({ "jsonrpc": "2.0", "id": id, "result": task })
                })
            }
        }),
    );
    let base = listen(router).await;

    let adapter = A2aAdapter::new("slow", base).unwrap();
    let (tx, _rx) = mpsc::channel(8);
    let outcome = adapter.execute(request(10_000), tx).await.unwrap();

    assert!(outcome.success, "{:?}", outcome.failure_reason);
    assert!(polls.load(Ordering::SeqCst) >= 2, "GetTask was polled");
}

/// A peer whose task never finishes.
#[tokio::test]
async fn an_abandoned_remote_task_is_cancelled() {
    let cancels = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&cancels);

    let router = axum::Router::new().route(
        "/",
        post(move |Json(body): Json<Value>| {
            let counter = Arc::clone(&counter);
            async move {
                if body["method"] == "CancelTask" {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                let task = json!({ "id": "t1", "contextId": "c1",
                                   "status": { "state": "TASK_STATE_WORKING" } });
                Json(if body["method"] == "SendMessage" {
                    json!({ "jsonrpc": "2.0", "id": body["id"], "result": { "task": task } })
                } else {
                    json!({ "jsonrpc": "2.0", "id": body["id"], "result": task })
                })
            }
        }),
    );
    let base = listen(router).await;

    let adapter = A2aAdapter::new("stuck", base).unwrap();
    let (tx, _rx) = mpsc::channel(8);
    let err = adapter.execute(request(600), tx).await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Timeout);

    for _ in 0..50 {
        if cancels.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        cancels.load(Ordering::SeqCst),
        1,
        "the remote task was cancelled"
    );
}

#[tokio::test]
async fn cuma_streams_a_task_as_server_sent_events() {
    let base = serve_cuma(Behaviour::ok("streamed body")).await;

    let response = reqwest::Client::new()
        .post(&base)
        .header("accept", "text/event-stream")
        .json(
            &json!({ "jsonrpc": "2.0", "id": 9, "method": "SendStreamingMessage",
                       "params": v1_message("say hello") }),
        )
        .send()
        .await
        .unwrap();

    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );

    let body = response.text().await.unwrap();
    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| serde_json::from_str(data.trim()).unwrap())
        .collect();

    assert!(events.iter().all(|e| e["id"] == 9 && e["jsonrpc"] == "2.0"));
    assert!(
        events[0]["result"]["task"].is_object(),
        "the first event is the task"
    );
    assert!(
        events.iter().any(
            |e| e["result"]["artifactUpdate"]["artifact"]["parts"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("streamed body"))
        ),
        "agent output is streamed as artifact updates: {body}"
    );
    let last = events.last().unwrap();
    assert_eq!(
        last["result"]["statusUpdate"]["status"]["state"], "TASK_STATE_COMPLETED",
        "the stream ends on the terminal status"
    );
}

/// A peer that streams, and whose card says so.
#[tokio::test]
async fn a_streaming_peer_is_followed_over_sse() {
    let sends = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&sends);

    let router = axum::Router::new()
        .route(
            "/.well-known/agent-card.json",
            axum::routing::get(|| async {
                Json(json!({
                    "name": "streamer",
                    "supportedInterfaces": [{ "url": "", "protocolBinding": "JSONRPC", "protocolVersion": "1.0" }],
                    "capabilities": { "streaming": true },
                }))
            }),
        )
        .route(
            "/",
            post(move |Json(body): Json<Value>| {
                let counter = Arc::clone(&counter);
                async move {
                    use axum::response::IntoResponse;
                    if body["method"] != "SendStreamingMessage" {
                        counter.fetch_add(1, Ordering::SeqCst);
                        return Json(json!({ "jsonrpc": "2.0", "id": body["id"],
                            "error": { "code": -32600, "message": "stream, please" } }))
                        .into_response();
                    }
                    let id = body["id"].clone();
                    let events = [
                        json!({ "task": { "id": "t1", "contextId": "c1", "status": { "state": "TASK_STATE_WORKING" } } }),
                        json!({ "artifactUpdate": { "taskId": "t1", "contextId": "c1", "append": true,
                                "artifact": { "artifactId": "a", "parts": [{ "text": "part one" }] } } }),
                        json!({ "artifactUpdate": { "taskId": "t1", "contextId": "c1", "append": true,
                                "artifact": { "artifactId": "a", "parts": [{ "text": "part two" }] } } }),
                        json!({ "statusUpdate": { "taskId": "t1", "contextId": "c1",
                                "status": { "state": "TASK_STATE_COMPLETED" } } }),
                    ];
                    let body: String = events
                        .iter()
                        .map(|result| {
                            format!("data: {}\r\n\r\n", json!({ "jsonrpc": "2.0", "id": id, "result": result }))
                        })
                        .collect();
                    ([("content-type", "text/event-stream")], body).into_response()
                }
            }),
        );
    let base = listen(router).await;

    let adapter = A2aAdapter::new("streamer", base).unwrap();
    // The card names an empty URL; the configured endpoint must survive that.
    adapter.refresh_from_card().await.unwrap();

    let (tx, mut rx) = mpsc::channel(8);
    let outcome = adapter.execute(request(5_000), tx).await.unwrap();

    assert!(outcome.success, "{:?}", outcome.failure_reason);
    assert_eq!(outcome.output, "part one\npart two");
    assert_eq!(sends.load(Ordering::SeqCst), 0, "no blocking call was made");

    let mut chunks = Vec::new();
    while let Ok(ExecutionUpdate::Text { content }) = rx.try_recv() {
        chunks.push(content);
    }
    assert_eq!(
        chunks,
        vec!["part one", "part two"],
        "chunks arrive as they stream"
    );
}

// ---------------------------------------------------------------------------
// Restarts
// ---------------------------------------------------------------------------

/// A task store kept in memory, standing in for the runtime database.
#[derive(Default)]
struct MemoryStore(std::sync::Mutex<Vec<(String, String)>>);

impl cuma_protocol_a2a::TaskStore for MemoryStore {
    fn save(&self, id: &str, _context: &str, _state: &str, body: &str) -> cuma_core::Result<()> {
        let mut tasks = self.0.lock().unwrap();
        match tasks.iter_mut().find(|(existing, _)| existing == id) {
            Some(entry) => entry.1 = body.to_owned(),
            None => tasks.push((id.to_owned(), body.to_owned())),
        }
        Ok(())
    }
    fn load(&self, limit: usize) -> cuma_core::Result<Vec<String>> {
        let tasks = self.0.lock().unwrap();
        let skip = tasks.len().saturating_sub(limit);
        Ok(tasks
            .iter()
            .skip(skip)
            .map(|(_, body)| body.clone())
            .collect())
    }
    fn remove(&self, id: &str) -> cuma_core::Result<()> {
        self.0
            .lock()
            .unwrap()
            .retain(|(existing, _)| existing != id);
        Ok(())
    }
}

async fn call(server: &A2aServer, method: &str, params: Value) -> Value {
    server
        .handle_json(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string(),
        )
        .await
}

#[tokio::test]
async fn tasks_outlive_a_restart_and_an_interrupted_one_is_reported_not_rerun() {
    let store = Arc::new(MemoryStore::default());

    // The first process finishes one task and is still running another when
    // it stops.
    let first = A2aServer::new(
        cuma_with(Behaviour::ok("done before")).await,
        "http://localhost/",
    )
    .await
    .with_store(store.clone());
    let finished = call(&first, "SendMessage", v1_message("say hello")).await;
    let finished_id = finished["result"]["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        finished["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );

    let slow = A2aServer::new(
        cuma_with(Behaviour::Slow {
            delay: Duration::from_secs(30),
            output: "never".into(),
        })
        .await,
        "http://localhost/",
    )
    .await
    .with_store(store.clone());
    let mut params = v1_message("say hello");
    params["configuration"] = json!({ "returnImmediately": true });
    let running = call(&slow, "SendMessage", params).await;
    let running_id = running["result"]["task"]["id"].as_str().unwrap().to_owned();
    drop(slow);
    drop(first);

    // A new process on the same store.
    let second = A2aServer::new(cuma_with(Behaviour::ok("x")).await, "http://localhost/")
        .await
        .with_store(store.clone());

    let restored = call(&second, "GetTask", json!({ "id": finished_id })).await;
    assert_eq!(
        restored["result"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert!(
        restored["result"]["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("done before"),
        "a finished task keeps its result: {restored}"
    );

    let interrupted = call(&second, "GetTask", json!({ "id": running_id })).await;
    assert_eq!(
        interrupted["result"]["status"]["state"],
        "TASK_STATE_FAILED"
    );
    assert!(
        interrupted["result"]["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("restarted"),
        "the caller is told why: {interrupted}"
    );

    let listed = call(&second, "ListTasks", json!({})).await;
    assert_eq!(listed["result"]["totalSize"], 2);

    // The interrupted state was written back, so a third process agrees.
    let third = A2aServer::new(cuma_with(Behaviour::ok("x")).await, "http://localhost/")
        .await
        .with_store(store);
    let again = call(&third, "GetTask", json!({ "id": running_id })).await;
    assert_eq!(again["result"]["status"]["state"], "TASK_STATE_FAILED");
    let cancel = call(&third, "CancelTask", json!({ "id": running_id })).await;
    assert_eq!(
        cancel["error"]["code"], -32002,
        "a settled task cannot be cancelled"
    );
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// A variable cargo always sets for a test binary, standing in for a token:
/// the client resolves its handle from the environment, and tests may not
/// modify the environment.
const TOKEN_HANDLE: &str = "CARGO_MANIFEST_DIR";

async fn serve_cuma_with_token(behaviour: Behaviour, token: &str) -> String {
    let orchestrator = cuma_with(behaviour).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let server = Arc::new(
        A2aServer::new(orchestrator, &base)
            .await
            .with_bearer_tokens(vec!["an-older-token-still-valid".into(), token.to_owned()]),
    );
    let router = server.router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    base
}

#[tokio::test]
async fn an_authenticated_server_refuses_callers_without_its_token() {
    let token = std::env::var(TOKEN_HANDLE).unwrap();
    let base = serve_cuma_with_token(Behaviour::ok("hello"), &token).await;
    let http = reqwest::Client::new();
    let call = json!({ "jsonrpc": "2.0", "id": 1, "method": "GetTask", "params": { "id": "x" } });

    for header in [
        None,
        Some("Bearer wrong".to_owned()),
        Some(format!("Basic {token}")),
    ] {
        let mut request = http.post(&base).json(&call);
        if let Some(header) = &header {
            request = request.header("authorization", header);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 401, "{header:?} must be refused");
        assert_eq!(
            response.headers()["www-authenticate"].to_str().unwrap(),
            "Bearer realm=\"cuma\""
        );
    }

    // Either configured token works, and the scheme name is case-insensitive.
    for header in [
        format!("Bearer {token}"),
        format!("bearer {token}"),
        "Bearer an-older-token-still-valid".to_owned(),
    ] {
        let response = http
            .post(&base)
            .header("authorization", &header)
            .json(&call)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["error"]["code"], -32001,
            "authenticated, and the task is simply unknown"
        );
    }

    // The card stays public and says what to send.
    let card: Value = http
        .get(format!("{base}.well-known/agent-card.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        card["securitySchemes"]["bearer"]["httpAuthSecurityScheme"]["scheme"],
        "Bearer"
    );
    assert_eq!(
        card["securityRequirements"][0]["schemes"]["bearer"]["list"],
        json!([])
    );
}

#[tokio::test]
async fn cuma_reaches_an_authenticated_cuma_with_a_token_from_a_handle() {
    let token = std::env::var(TOKEN_HANDLE).unwrap();
    let base = serve_cuma_with_token(Behaviour::ok("authenticated hello"), &token).await;

    let (updates, _rx) = mpsc::channel(64);
    let with_token = A2aAdapter::new("remote", base.clone())
        .unwrap()
        .with_auth_handle(TOKEN_HANDLE);
    let outcome = with_token.execute(request(10_000), updates).await.unwrap();
    assert!(outcome.success, "{:?}", outcome.failure_reason);
    assert!(outcome.output.contains("authenticated hello"));

    let (updates, _rx) = mpsc::channel(64);
    let without = A2aAdapter::new("remote", base).unwrap();
    let refused = without.execute(request(10_000), updates).await;
    let class = match refused {
        Ok(outcome) => outcome.failure_class,
        Err(err) => Some(err.class()),
    };
    assert_eq!(
        class,
        Some(ErrorClass::AuthenticationFailure),
        "a refusal is an auth failure, not a retry"
    );
}

#[test]
fn an_unauthenticated_server_publishes_no_security_requirement() {
    let card = cuma_protocol_a2a::AgentCard::new("cuma", "http://localhost/");
    let json = serde_json::to_value(&card).unwrap();
    assert!(json.get("securitySchemes").is_none());
    assert!(json.get("securityRequirements").is_none());
}
