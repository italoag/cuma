//! The A2A wire vocabulary.
//!
//! Written against the A2A 1.0 protocol definition (`a2a.proto`) and the
//! JSON-RPC binding used by the official `a2a-rs` SDK, both read from source
//! rather than recalled — see `DEPENDENCY_ANALYSIS.md`.
//!
//! ## Two dialects
//!
//! A2A 1.0 renamed every method (`message/send` became `SendMessage`) and
//! changed the JSON shape: enums are proto names (`TASK_STATE_COMPLETED`,
//! `ROLE_USER`), parts are tag-free (`{"text": "..."}`), and responses are
//! field-presence unions (`{"task": {...}}`). Much of the deployed ecosystem
//! still speaks the 0.3 dialect. CUMA speaks 1.0 first and falls back to 0.3
//! when a peer answers "method not found", and its server accepts both — so a
//! version bump on either side degrades to a slower handshake rather than to
//! a broken integration.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// JSON-RPC method names.
pub mod methods {
    /// Send a message; returns a task or a message.
    pub const SEND_MESSAGE: &str = "SendMessage";
    /// Send a message and stream updates over SSE.
    pub const SEND_STREAMING_MESSAGE: &str = "SendStreamingMessage";
    /// Fetch a task.
    pub const GET_TASK: &str = "GetTask";
    /// List tasks.
    pub const LIST_TASKS: &str = "ListTasks";
    /// Cancel a task.
    pub const CANCEL_TASK: &str = "CancelTask";
    /// Re-attach to a running task's stream.
    pub const SUBSCRIBE_TO_TASK: &str = "SubscribeToTask";
    /// Fetch the authenticated extended card.
    pub const GET_EXTENDED_AGENT_CARD: &str = "GetExtendedAgentCard";

    /// The 0.3 name for a 1.0 method, if it had one.
    pub fn legacy_name(method: &str) -> Option<&'static str> {
        Some(match method {
            SEND_MESSAGE => "message/send",
            SEND_STREAMING_MESSAGE => "message/stream",
            GET_TASK => "tasks/get",
            CANCEL_TASK => "tasks/cancel",
            SUBSCRIBE_TO_TASK => "tasks/resubscribe",
            _ => return None,
        })
    }

    /// The 1.0 name for a method, whichever dialect it was written in.
    pub fn canonical(method: &str) -> &str {
        match method {
            "message/send" => SEND_MESSAGE,
            "message/stream" => SEND_STREAMING_MESSAGE,
            "tasks/get" => GET_TASK,
            "tasks/cancel" => CANCEL_TASK,
            "tasks/resubscribe" => SUBSCRIBE_TO_TASK,
            other => other,
        }
    }

    /// Whether a method was written in the 0.3 dialect.
    pub fn is_legacy(method: &str) -> bool {
        method.contains('/')
    }
}

/// JSON-RPC and A2A error codes.
pub mod error_code {
    /// Malformed JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// Not a JSON-RPC request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// Unknown method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Bad parameters.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Server-side failure.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// No such task.
    pub const TASK_NOT_FOUND: i64 = -32001;
    /// The task has already finished.
    pub const TASK_NOT_CANCELABLE: i64 = -32002;
    /// Push notifications are not offered.
    pub const PUSH_NOTIFICATION_NOT_SUPPORTED: i64 = -32003;
    /// The operation is not offered.
    pub const UNSUPPORTED_OPERATION: i64 = -32004;
    /// No extended card is configured.
    pub const EXTENDED_CARD_NOT_CONFIGURED: i64 = -32007;
}

/// Which protocol dialect a peer speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Dialect {
    /// A2A 1.0: PascalCase methods, proto-JSON shapes.
    #[default]
    V1,
    /// A2A 0.3: slash methods, `kind`-tagged shapes.
    Legacy,
}

/// A task's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskState {
    /// Accepted, not started.
    Submitted,
    /// In progress.
    Working,
    /// Finished successfully.
    Completed,
    /// Finished unsuccessfully.
    Failed,
    /// Cancelled by a caller.
    Canceled,
    /// Waiting for the caller to supply input.
    InputRequired,
    /// Refused by the agent.
    Rejected,
    /// Waiting for the caller to authenticate.
    AuthRequired,
    /// Unrecognised. Treated as non-terminal so a caller keeps polling rather
    /// than mistaking an unknown state for success.
    Unknown,
}

impl TaskState {
    /// Parse either dialect's spelling.
    pub fn parse(raw: &str) -> Self {
        match raw
            .trim()
            .trim_start_matches("TASK_STATE_")
            .to_ascii_lowercase()
            .replace('_', "-")
            .as_str()
        {
            "submitted" => Self::Submitted,
            "working" => Self::Working,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "canceled" | "cancelled" => Self::Canceled,
            "input-required" => Self::InputRequired,
            "rejected" => Self::Rejected,
            "auth-required" => Self::AuthRequired,
            _ => Self::Unknown,
        }
    }

    /// The spelling for `dialect`.
    pub fn render(self, dialect: Dialect) -> &'static str {
        match (self, dialect) {
            (Self::Submitted, Dialect::V1) => "TASK_STATE_SUBMITTED",
            (Self::Working, Dialect::V1) => "TASK_STATE_WORKING",
            (Self::Completed, Dialect::V1) => "TASK_STATE_COMPLETED",
            (Self::Failed, Dialect::V1) => "TASK_STATE_FAILED",
            (Self::Canceled, Dialect::V1) => "TASK_STATE_CANCELED",
            (Self::InputRequired, Dialect::V1) => "TASK_STATE_INPUT_REQUIRED",
            (Self::Rejected, Dialect::V1) => "TASK_STATE_REJECTED",
            (Self::AuthRequired, Dialect::V1) => "TASK_STATE_AUTH_REQUIRED",
            (Self::Unknown, Dialect::V1) => "TASK_STATE_UNSPECIFIED",
            (Self::Submitted, Dialect::Legacy) => "submitted",
            (Self::Working, Dialect::Legacy) => "working",
            (Self::Completed, Dialect::Legacy) => "completed",
            (Self::Failed, Dialect::Legacy) => "failed",
            (Self::Canceled, Dialect::Legacy) => "canceled",
            (Self::InputRequired, Dialect::Legacy) => "input-required",
            (Self::Rejected, Dialect::Legacy) => "rejected",
            (Self::AuthRequired, Dialect::Legacy) => "auth-required",
            (Self::Unknown, Dialect::Legacy) => "unknown",
        }
    }

    /// Whether the task will never change state again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }

    /// Whether the agent has stopped and is waiting on the caller.
    ///
    /// Not terminal in the spec, but a caller polling for a result should stop
    /// here too: nothing will happen until someone supplies what was asked for.
    pub fn is_interrupted(self) -> bool {
        matches!(self, Self::InputRequired | Self::AuthRequired)
    }
}

/// A task, as CUMA holds it internally.
#[derive(Debug, Clone, PartialEq)]
pub struct WireTask {
    /// Task id.
    pub id: String,
    /// Conversation id.
    pub context_id: String,
    /// Current state.
    pub state: TaskState,
    /// The latest status message, as text.
    pub status_text: Option<String>,
    /// When the status last changed (RFC 3339).
    pub timestamp: Option<String>,
    /// Text of every artifact, in order.
    pub artifacts: Vec<(String, String)>,
}

impl WireTask {
    /// All the text a task produced: artifacts first, then its status message.
    pub fn text(&self) -> String {
        let mut parts: Vec<&str> = self
            .artifacts
            .iter()
            .map(|(_, text)| text.as_str())
            .collect();
        if let Some(status) = &self.status_text {
            parts.push(status);
        }
        parts
            .into_iter()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Render for `dialect`.
    pub fn to_json(&self, dialect: Dialect) -> Value {
        let status_message = self
            .status_text
            .as_ref()
            .map(|text| message_json(text, "agent", dialect));

        let artifacts: Vec<Value> = self
            .artifacts
            .iter()
            .map(|(id, text)| match dialect {
                Dialect::V1 => {
                    json!({ "artifactId": id, "name": "result", "parts": [{ "text": text }] })
                }
                Dialect::Legacy => json!({
                    "artifactId": id,
                    "name": "result",
                    "parts": [{ "kind": "text", "text": text }],
                }),
            })
            .collect();

        let mut status = json!({ "state": self.state.render(dialect) });
        if let Some(message) = status_message {
            status["message"] = message;
        }
        if let Some(timestamp) = &self.timestamp {
            status["timestamp"] = json!(timestamp);
        }

        let mut task = json!({
            "id": self.id,
            "contextId": self.context_id,
            "status": status,
            "artifacts": artifacts,
        });
        if dialect == Dialect::Legacy {
            task["kind"] = json!("task");
        }
        task
    }
}

/// Build a message in `dialect`.
pub fn message_json(text: &str, role: &str, dialect: Dialect) -> Value {
    let id = uuid::Uuid::new_v4().to_string();
    match dialect {
        Dialect::V1 => json!({
            "messageId": id,
            "role": if role == "user" { "ROLE_USER" } else { "ROLE_AGENT" },
            "parts": [{ "text": text }],
        }),
        Dialect::Legacy => json!({
            "kind": "message",
            "messageId": id,
            "role": role,
            "parts": [{ "kind": "text", "text": text }],
        }),
    }
}

/// Collect every `text` part in a `parts` array.
///
/// Both dialects keep text under `text`; 1.0 simply drops the `kind` tag.
/// Non-text parts (raw bytes, URLs, data) contribute nothing: a caller's file
/// is not a goal, and a peer's binary artifact is not a transcript.
pub fn parts_text(parts: Option<&Value>) -> String {
    parts
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Parse a task from either dialect.
pub fn parse_task(value: &Value) -> Option<WireTask> {
    let id = value.get("id")?.as_str()?.to_owned();
    let status = value.get("status");

    let state = status
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .map_or(TaskState::Unknown, TaskState::parse);

    let status_text = status
        .and_then(|s| s.get("message"))
        .map(|m| parts_text(m.get("parts")))
        .filter(|t| !t.is_empty());

    let artifacts = value
        .get("artifacts")
        .and_then(Value::as_array)
        .map(|artifacts| {
            artifacts
                .iter()
                .map(|artifact| {
                    let id = artifact
                        .get("artifactId")
                        .and_then(Value::as_str)
                        .unwrap_or("artifact")
                        .to_owned();
                    (id, parts_text(artifact.get("parts")))
                })
                .filter(|(_, text)| !text.is_empty())
                .collect()
        })
        .unwrap_or_default();

    Some(WireTask {
        id,
        context_id: value
            .get("contextId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        state,
        status_text,
        timestamp: status
            .and_then(|s| s.get("timestamp"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        artifacts,
    })
}

/// What a `SendMessage` returned.
#[derive(Debug, Clone, PartialEq)]
pub enum SendResult {
    /// A task, possibly still running.
    Task(WireTask),
    /// A direct reply: the peer answered without creating a task.
    Message(String),
}

/// Parse a `SendMessage` result in either dialect.
///
/// 1.0 wraps the payload in a field-presence union (`{"task": ...}` or
/// `{"message": ...}`); 0.3 returns the object bare, tagged with `kind`.
pub fn parse_send_result(result: &Value) -> Option<SendResult> {
    if let Some(task) = result.get("task") {
        return parse_task(task).map(SendResult::Task);
    }
    if let Some(message) = result.get("message") {
        return Some(SendResult::Message(parts_text(message.get("parts"))));
    }

    match result.get("kind").and_then(Value::as_str) {
        Some("message") => Some(SendResult::Message(parts_text(result.get("parts")))),
        Some("task") => parse_task(result).map(SendResult::Task),
        // No tag: a bare task is identifiable by its status.
        _ if result.get("status").is_some() => parse_task(result).map(SendResult::Task),
        _ if result.get("parts").is_some() => {
            Some(SendResult::Message(parts_text(result.get("parts"))))
        }
        _ => None,
    }
}

/// One event from a streaming call.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A full task snapshot.
    Task(WireTask),
    /// A direct reply.
    Message(String),
    /// The task changed state.
    Status {
        /// New state.
        state: TaskState,
        /// Status text, if any.
        text: Option<String>,
    },
    /// An artifact arrived or grew.
    Artifact {
        /// Its text.
        text: String,
    },
}

/// Parse one streamed `result` in either dialect.
pub fn parse_stream_event(result: &Value) -> Option<StreamEvent> {
    let status_event = |event: &Value| {
        let status = event.get("status")?;
        Some(StreamEvent::Status {
            state: status
                .get("state")
                .and_then(Value::as_str)
                .map_or(TaskState::Unknown, TaskState::parse),
            text: status
                .get("message")
                .map(|m| parts_text(m.get("parts")))
                .filter(|t| !t.is_empty()),
        })
    };
    let artifact_event = |event: &Value| {
        Some(StreamEvent::Artifact {
            text: parts_text(event.get("artifact")?.get("parts")),
        })
    };

    if let Some(event) = result.get("statusUpdate") {
        return status_event(event);
    }
    if let Some(event) = result.get("artifactUpdate") {
        return artifact_event(event);
    }

    match result.get("kind").and_then(Value::as_str) {
        Some("status-update") => status_event(result),
        Some("artifact-update") => artifact_event(result),
        _ => match parse_send_result(result)? {
            SendResult::Task(task) => Some(StreamEvent::Task(task)),
            SendResult::Message(text) => Some(StreamEvent::Message(text)),
        },
    }
}

/// Render a status-update stream event.
pub fn status_event_json(task: &WireTask, dialect: Dialect) -> Value {
    let full = task.to_json(dialect);
    match dialect {
        Dialect::V1 => json!({
            "statusUpdate": {
                "taskId": task.id,
                "contextId": task.context_id,
                "status": full["status"],
            }
        }),
        Dialect::Legacy => json!({
            "kind": "status-update",
            "taskId": task.id,
            "contextId": task.context_id,
            "status": full["status"],
            "final": task.state.is_terminal() || task.state.is_interrupted(),
        }),
    }
}

/// Render an artifact-update stream event carrying `text`.
pub fn artifact_event_json(
    task: &WireTask,
    artifact_id: &str,
    text: &str,
    dialect: Dialect,
) -> Value {
    match dialect {
        Dialect::V1 => json!({
            "artifactUpdate": {
                "taskId": task.id,
                "contextId": task.context_id,
                "artifact": { "artifactId": artifact_id, "parts": [{ "text": text }] },
                "append": true,
            }
        }),
        Dialect::Legacy => json!({
            "kind": "artifact-update",
            "taskId": task.id,
            "contextId": task.context_id,
            "artifact": { "artifactId": artifact_id, "parts": [{ "kind": "text", "text": text }] },
            "append": true,
        }),
    }
}

/// Wrap a task in the dialect's `SendMessage` response shape.
pub fn send_result_json(task: &WireTask, dialect: Dialect) -> Value {
    match dialect {
        Dialect::V1 => json!({ "task": task.to_json(dialect) }),
        Dialect::Legacy => task.to_json(dialect),
    }
}

/// Wrap a task in the dialect's stream-event shape.
pub fn task_event_json(task: &WireTask, dialect: Dialect) -> Value {
    send_result_json(task, dialect)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn task(state: TaskState) -> WireTask {
        WireTask {
            id: "t1".into(),
            context_id: "c1".into(),
            state,
            status_text: Some("done".into()),
            timestamp: Some("2026-09-24T00:00:00Z".into()),
            artifacts: vec![("a1".into(), "the result".into())],
        }
    }

    // --- methods ----------------------------------------------------------

    #[test]
    fn methods_are_pascal_case_in_1_0() {
        assert_eq!(methods::SEND_MESSAGE, "SendMessage");
        assert_eq!(methods::GET_TASK, "GetTask");
        assert_eq!(methods::CANCEL_TASK, "CancelTask");
    }

    #[test]
    fn legacy_and_canonical_names_round_trip() {
        for method in [
            methods::SEND_MESSAGE,
            methods::SEND_STREAMING_MESSAGE,
            methods::GET_TASK,
            methods::CANCEL_TASK,
            methods::SUBSCRIBE_TO_TASK,
        ] {
            let legacy = methods::legacy_name(method).unwrap();
            assert!(methods::is_legacy(legacy));
            assert_eq!(methods::canonical(legacy), method);
        }
    }

    #[test]
    fn a_method_without_a_legacy_name_has_none() {
        assert_eq!(methods::legacy_name(methods::LIST_TASKS), None);
    }

    // --- states -----------------------------------------------------------

    #[test]
    fn both_dialects_state_spellings_parse() {
        assert_eq!(
            TaskState::parse("TASK_STATE_COMPLETED"),
            TaskState::Completed
        );
        assert_eq!(TaskState::parse("completed"), TaskState::Completed);
        assert_eq!(
            TaskState::parse("TASK_STATE_INPUT_REQUIRED"),
            TaskState::InputRequired
        );
        assert_eq!(TaskState::parse("input-required"), TaskState::InputRequired);
        assert_eq!(TaskState::parse("cancelled"), TaskState::Canceled);
    }

    #[test]
    fn states_render_and_parse_back_in_each_dialect() {
        for state in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::InputRequired,
            TaskState::Rejected,
            TaskState::AuthRequired,
        ] {
            for dialect in [Dialect::V1, Dialect::Legacy] {
                assert_eq!(
                    TaskState::parse(state.render(dialect)),
                    state,
                    "{state:?} {dialect:?}"
                );
            }
        }
    }

    #[test]
    fn input_required_is_interrupted_not_complete() {
        // Treating a paused task as a success reports work as done when the
        // peer is actually waiting for an answer.
        assert!(!TaskState::InputRequired.is_terminal());
        assert!(TaskState::InputRequired.is_interrupted());
        assert!(TaskState::AuthRequired.is_interrupted());
    }

    #[test]
    fn an_unknown_state_is_neither_terminal_nor_interrupted() {
        let state = TaskState::parse("TASK_STATE_FROM_THE_FUTURE");
        assert_eq!(state, TaskState::Unknown);
        assert!(
            !state.is_terminal(),
            "an unknown state must not look like success"
        );
    }

    // --- shapes -----------------------------------------------------------

    #[test]
    fn a_1_0_message_uses_proto_names_and_tag_free_parts() {
        let message = message_json("hello", "user", Dialect::V1);
        assert_eq!(message["role"], "ROLE_USER");
        assert_eq!(message["parts"][0], json!({ "text": "hello" }));
        assert!(message.get("kind").is_none());
    }

    #[test]
    fn a_legacy_message_uses_kind_tags() {
        let message = message_json("hello", "user", Dialect::Legacy);
        assert_eq!(message["role"], "user");
        assert_eq!(message["parts"][0]["kind"], "text");
        assert_eq!(message["kind"], "message");
    }

    #[test]
    fn a_1_0_send_result_wraps_the_task_in_a_union() {
        let json = send_result_json(&task(TaskState::Completed), Dialect::V1);
        assert_eq!(json["task"]["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(json["task"]["contextId"], "c1");
    }

    #[test]
    fn a_legacy_send_result_is_the_bare_task() {
        let json = send_result_json(&task(TaskState::Completed), Dialect::Legacy);
        assert_eq!(json["kind"], "task");
        assert_eq!(json["status"]["state"], "completed");
    }

    #[test]
    fn a_task_round_trips_through_both_dialects() {
        for dialect in [Dialect::V1, Dialect::Legacy] {
            let original = task(TaskState::Working);
            let parsed = match parse_send_result(&send_result_json(&original, dialect)).unwrap() {
                SendResult::Task(t) => t,
                other => panic!("expected a task, got {other:?}"),
            };
            assert_eq!(parsed, original, "{dialect:?}");
        }
    }

    #[test]
    fn a_direct_message_reply_parses_in_both_dialects() {
        let v1 = json!({ "message": { "messageId": "m", "role": "ROLE_AGENT", "parts": [{ "text": "hi" }] } });
        assert_eq!(
            parse_send_result(&v1),
            Some(SendResult::Message("hi".into()))
        );

        let legacy = json!({ "kind": "message", "parts": [{ "kind": "text", "text": "hi" }] });
        assert_eq!(
            parse_send_result(&legacy),
            Some(SendResult::Message("hi".into()))
        );
    }

    #[test]
    fn non_text_parts_contribute_nothing() {
        let parts = json!([{ "raw": "AQID" }, { "url": "file:///etc/passwd" }, { "text": "ok" }]);
        assert_eq!(parts_text(Some(&parts)), "ok");
    }

    #[test]
    fn stream_events_parse_in_both_dialects() {
        let t = task(TaskState::Working);
        for dialect in [Dialect::V1, Dialect::Legacy] {
            match parse_stream_event(&status_event_json(&t, dialect)).unwrap() {
                StreamEvent::Status { state, .. } => assert_eq!(state, TaskState::Working),
                other => panic!("{dialect:?}: {other:?}"),
            }
            match parse_stream_event(&artifact_event_json(&t, "a", "chunk", dialect)).unwrap() {
                StreamEvent::Artifact { text } => assert_eq!(text, "chunk"),
                other => panic!("{dialect:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn task_text_joins_artifacts_and_status() {
        assert_eq!(task(TaskState::Completed).text(), "the result\n\ndone");
    }
}
