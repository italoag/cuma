#![no_main]

use arbitrary::Arbitrary;
use cuma_core::Capability;
use cuma_protocol_a2a::wire::{
    Dialect, SendResult, StreamEvent, TaskState, WireTask, parse_send_result, parse_stream_event,
    parse_task, send_result_json, status_event_json,
};
use cuma_protocol_a2a::{AgentCard, capabilities_from_card};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct A2aFuzzInput {
    raw_bytes: Vec<u8>,
    state_idx: u8,
    dialect_v1: bool,
    task_id: String,
    context_id: String,
    status_text: Option<String>,
    artifacts: Vec<(String, String)>,
}

const KNOWN_STATES: [TaskState; 8] = [
    TaskState::Submitted,
    TaskState::Working,
    TaskState::Completed,
    TaskState::Failed,
    TaskState::Canceled,
    TaskState::InputRequired,
    TaskState::Rejected,
    TaskState::AuthRequired,
];

fuzz_target!(|input: A2aFuzzInput| {
    if let Ok(card) = serde_json::from_slice::<AgentCard>(&input.raw_bytes) {
        let _ = card.jsonrpc_endpoint();
        let _ = card.protocol_version();
        let caps = capabilities_from_card(&card);
        assert!(
            !caps.is_empty(),
            "A2A card must always retain baseline Research capability"
        );
        for cap in caps.iter() {
            if let Capability::Custom(name) = cap {
                assert!(!name.is_empty());
                assert!(name.len() <= 64);
                assert!(!name.starts_with('.'));
                assert!(!name.contains(".."));
                assert!(!name.contains('/'));
                assert!(!name.chars().any(char::is_whitespace));
            }
        }
    }

    let raw_str = String::from_utf8_lossy(&input.raw_bytes);
    let _ = TaskState::parse(&raw_str);

    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&input.raw_bytes) {
        let _ = parse_task(&val);
        let _ = parse_send_result(&val);
        let _ = parse_stream_event(&val);
    }

    let state = KNOWN_STATES[usize::from(input.state_idx) % KNOWN_STATES.len()];
    let dialect = if input.dialect_v1 {
        Dialect::V1
    } else {
        Dialect::Legacy
    };

    assert_eq!(TaskState::parse(state.render(dialect)), state);

    let wt = WireTask {
        id: if input.task_id.is_empty() {
            "t1".to_owned()
        } else {
            input.task_id
        },
        context_id: input.context_id,
        state,
        status_text: input.status_text.filter(|s| !s.trim().is_empty()),
        timestamp: None,
        artifacts: input
            .artifacts
            .into_iter()
            .take(8)
            .filter(|(_, text)| !text.trim().is_empty())
            .map(|(id, text)| (if id.is_empty() { "art".to_owned() } else { id }, text))
            .collect(),
    };

    let json_val = send_result_json(&wt, dialect);
    match parse_send_result(&json_val) {
        Some(SendResult::Task(parsed)) => {
            assert_eq!(parsed.id, wt.id);
            assert_eq!(parsed.state, wt.state);
        }
        other => panic!("structured WireTask failed round-trip: {other:?}"),
    }

    let status_val = status_event_json(&wt, dialect);
    match parse_stream_event(&status_val) {
        Some(StreamEvent::Status { state: s, .. }) => assert_eq!(s, wt.state),
        other => panic!("structured status event failed round-trip: {other:?}"),
    }
});
