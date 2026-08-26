//! The [`AgentAdapter`] implementation for ACP agents.

use crate::capabilities::capabilities_from_initialize;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo};
use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{AgentAdapter, ExecutionRequest, ExecutionUpdate};
use cuma_core::{
    AgentDescriptor, AgentId, AgentProtocol, AttemptId, ErrorClass, ExecutionOutcome, Risk,
    TokenUsage,
};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// How the adapter answers an agent's permission request.
///
/// The harness answers from policy rather than forwarding to a human, because
/// an unattended run must not block on a prompt nobody will see. What policy
/// says is decided by the task's [`Risk`], not by the agent asking nicely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Approve everything. Only appropriate inside a sandbox.
    AlwaysAllow,
    /// Approve only for tasks the planner marked read-only or low risk.
    AllowLowRisk,
    /// Refuse everything.
    AlwaysDeny,
}

impl PermissionPolicy {
    /// Whether a task at `risk` may proceed under this policy.
    fn permits(self, risk: Risk) -> bool {
        match self {
            Self::AlwaysAllow => true,
            Self::AllowLowRisk => matches!(risk, Risk::ReadOnly | Risk::Low),
            Self::AlwaysDeny => false,
        }
    }
}

/// Drives one ACP agent.
///
/// A fresh process is spawned per execution rather than held open across
/// tasks. That costs a spawn per task and buys three things worth more: a
/// crashed agent cannot poison later tasks, `Send`-ness stays simple, and
/// there is no long-lived child to leak when a session is abandoned. Session
/// reuse is the obvious future optimization (see `docs/PROTOCOLS.md`).
pub struct AcpAdapter {
    descriptor: Arc<Mutex<AgentDescriptor>>,
    id: AgentId,
    command: String,
    permission_policy: PermissionPolicy,
}

impl AcpAdapter {
    /// An adapter that launches `command`, which must speak ACP over stdio.
    pub fn new(id: impl Into<AgentId>, command: impl Into<String>) -> Self {
        let id = id.into();
        let descriptor = AgentDescriptor::new(id.clone(), id.to_string(), AgentProtocol::Acp);

        Self {
            descriptor: Arc::new(Mutex::new(descriptor)),
            id,
            command: command.into(),
            permission_policy: PermissionPolicy::AllowLowRisk,
        }
    }

    /// Seed the adapter with a configured descriptor.
    #[must_use]
    pub fn with_descriptor(self, descriptor: AgentDescriptor) -> Self {
        if let Ok(mut guard) = self.descriptor.try_lock() {
            *guard = descriptor;
        }
        self
    }

    /// Set how permission requests are answered.
    #[must_use]
    pub fn with_permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = policy;
        self
    }

    /// Whether the agent's command is actually on `PATH`.
    ///
    /// Used by `cuma doctor` and by discovery so a misconfigured agent is
    /// reported as missing rather than silently failing at routing time.
    pub fn is_launchable(&self) -> bool {
        let Ok(parts) = shell_words::split(&self.command) else {
            return false;
        };
        parts
            .first()
            .is_some_and(|binary| which::which(binary).is_ok())
    }

    /// Build the SDK's agent handle from the configured command.
    fn spawn_handle(&self) -> Result<AcpAgent> {
        AcpAgent::from_str(&self.command).map_err(|err| {
            MetaAgentError::Configuration(format!(
                "agent {}: cannot parse command {:?}: {err}",
                self.id, self.command
            ))
        })
    }
}

/// Translate an ACP session notification into a protocol-agnostic update.
///
/// Variants the harness has no use for map to `None` rather than being
/// forwarded as opaque text: an event stream full of noise is as unhelpful as
/// no event stream at all.
fn translate_update(notification: &SessionNotification) -> Option<ExecutionUpdate> {
    match &notification.update {
        SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            ContentBlock::Text(text) => Some(ExecutionUpdate::Text {
                content: text.text.clone(),
            }),
            _ => None,
        },
        SessionUpdate::ToolCall(call) => Some(ExecutionUpdate::ToolCall {
            name: call.title.clone(),
            status: format!("{:?}", call.status),
        }),
        SessionUpdate::ToolCallUpdate(update) => {
            update
                .fields
                .status
                .map(|status| ExecutionUpdate::ToolCall {
                    name: update.tool_call_id.to_string(),
                    status: format!("{status:?}"),
                })
        }
        SessionUpdate::Plan(plan) => Some(ExecutionUpdate::Plan {
            entries: plan
                .entries
                .iter()
                .map(|entry| format!("[{:?}] {}", entry.status, entry.content))
                .collect(),
        }),
        // Thoughts, user echoes, mode changes and command lists are not the
        // orchestrator's business.
        _ => None,
    }
}

/// Map an ACP stop reason onto success or a classified failure.
fn interpret_stop_reason(reason: StopReason) -> std::result::Result<(), (ErrorClass, String)> {
    match reason {
        StopReason::EndTurn => Ok(()),
        StopReason::MaxTokens => Err((
            ErrorClass::ContextOverflow,
            "the agent hit its token limit before finishing".to_owned(),
        )),
        StopReason::MaxTurnRequests => Err((
            ErrorClass::TaskFailure,
            "the agent hit its turn limit before finishing".to_owned(),
        )),
        StopReason::Refusal => Err((
            ErrorClass::TaskFailure,
            "the agent refused to continue".to_owned(),
        )),
        StopReason::Cancelled => Err((ErrorClass::Cancelled, "the turn was cancelled".to_owned())),
        // `StopReason` is `#[non_exhaustive]`: a newer agent may return a
        // reason this build has never heard of. Treating an unrecognized
        // reason as success would silently mark unfinished work as done.
        other => Err((
            ErrorClass::TaskFailure,
            format!("the agent stopped for an unrecognized reason: {other:?}"),
        )),
    }
}

#[async_trait]
impl AgentAdapter for AcpAdapter {
    fn agent_id(&self) -> &AgentId {
        &self.id
    }

    async fn describe(&self) -> Result<AgentDescriptor> {
        // Return what is already known without launching a process. Live
        // interrogation happens in `refresh_capabilities`, which the registry
        // calls explicitly — `describe` is called often enough that spawning
        // an agent here would make discovery pathologically slow.
        Ok(self.descriptor.lock().await.clone())
    }

    async fn execute(
        &self,
        request: ExecutionRequest,
        updates: mpsc::Sender<ExecutionUpdate>,
    ) -> Result<ExecutionOutcome> {
        let started = std::time::Instant::now();
        let agent = self.spawn_handle()?;

        let risk = request.task.spec.risk;
        let policy = self.permission_policy;
        let prompt_text = request.prompt.clone();
        let workspace = request.workspace.clone();
        let agent_id = self.id.clone();

        // Collected assistant text, shared with the notification handler.
        let transcript = Arc::new(Mutex::new(String::new()));
        let transcript_for_handler = Arc::clone(&transcript);
        let updates_for_handler = updates.clone();

        let stop_reason = agent_client_protocol::Client
            .builder()
            .name("cuma")
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    if let Some(update) = translate_update(&notification) {
                        if let ExecutionUpdate::Text { content } = &update {
                            transcript_for_handler.lock().await.push_str(content);
                        }
                        // A full channel means the consumer is not keeping up.
                        // Dropping an update is strictly better than stalling
                        // the agent that produced it.
                        let _ = updates_for_handler.try_send(update);
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |permission: RequestPermissionRequest, responder, _cx| {
                    // Policy decides, not the agent's phrasing of the request.
                    if policy.permits(risk) {
                        match permission.options.first().map(|o| o.option_id.clone()) {
                            Some(option_id) => responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                    option_id,
                                )),
                            )),
                            None => responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Cancelled,
                            )),
                        }
                    } else {
                        tracing::info!(
                            risk = ?risk,
                            "refusing an agent permission request under the configured policy"
                        );
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let session = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;

                let response = connection
                    .send_request(PromptRequest::new(
                        session.session_id,
                        vec![ContentBlock::Text(TextContent::new(prompt_text))],
                    ))
                    .block_task()
                    .await?;

                Ok(response.stop_reason)
            })
            .await
            .map_err(|err| {
                // An ACP-level error is a transport or protocol failure. The
                // orchestrator re-classifies from the message when this comes
                // back as a generic protocol error.
                MetaAgentError::protocol_msg("acp", format!("agent {agent_id} failed: {err}"))
            })?;

        let output = transcript.lock().await.clone();
        #[allow(clippy::cast_possible_truncation)]
        let latency_ms = started.elapsed().as_millis() as u64;

        let (success, failure_class, failure_reason) = match interpret_stop_reason(stop_reason) {
            Ok(()) => (true, None, None),
            Err((class, reason)) => (false, Some(class), Some(reason)),
        };

        Ok(ExecutionOutcome {
            attempt_id: AttemptId::generate(),
            agent_id: self.id.clone(),
            model_id: request.model,
            success,
            output,
            // ACP does not report changed files as part of a prompt turn.
            // Claiming otherwise would put fabricated paths in the handoff.
            changed_files: Vec::new(),
            // Token reporting is behind an unstable ACP feature. Rather than
            // guess, usage is marked estimated and zero, so the ledger shows
            // "unknown" instead of a made-up number.
            tokens: TokenUsage::estimated(0, 0),
            latency_ms,
            failure_class,
            failure_reason,
        })
    }

    async fn health_check(&self) -> Result<()> {
        if !self.is_launchable() {
            return Err(MetaAgentError::Configuration(format!(
                "agent {}: command {:?} is not on PATH",
                self.id, self.command
            )));
        }
        Ok(())
    }
}

impl AcpAdapter {
    /// Launch the agent, negotiate capabilities and update the descriptor.
    ///
    /// This is the expensive counterpart to [`AgentAdapter::describe`], called
    /// by discovery rather than on every routing decision.
    pub async fn refresh_capabilities(&self) -> Result<AgentDescriptor> {
        let agent = self.spawn_handle()?;
        let agent_id = self.id.clone();

        let response = agent_client_protocol::Client
            .builder()
            .name("cuma")
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await
            })
            .await
            .map_err(|err| {
                MetaAgentError::protocol_msg(
                    "acp",
                    format!("agent {agent_id}: initialize failed: {err}"),
                )
            })?;

        let mut descriptor = self.descriptor.lock().await;
        descriptor.capabilities = capabilities_from_initialize(&response);

        if let Some(info) = &response.agent_info {
            descriptor.name = info.name.clone();
            descriptor
                .metadata
                .insert("version".to_owned(), info.version.clone());
        }

        // An agent that advertises auth methods manages its own credentials;
        // that is the mode CUMA prefers, because it means no secret ever
        // reaches the harness.
        if !response.auth_methods.is_empty() {
            descriptor.auth = cuma_core::AgentAuth::AgentManaged;
        }

        descriptor
            .metadata
            .insert("acp_command".to_owned(), self.command.clone());

        Ok(descriptor.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_read_only_task_is_permitted_under_the_default_policy() {
        assert!(PermissionPolicy::AllowLowRisk.permits(Risk::ReadOnly));
        assert!(PermissionPolicy::AllowLowRisk.permits(Risk::Low));
    }

    #[test]
    fn the_default_policy_refuses_high_risk_work() {
        assert!(!PermissionPolicy::AllowLowRisk.permits(Risk::Medium));
        assert!(
            !PermissionPolicy::AllowLowRisk.permits(Risk::High),
            "destructive work must not be auto-approved"
        );
    }

    #[test]
    fn always_deny_refuses_even_read_only_work() {
        assert!(!PermissionPolicy::AlwaysDeny.permits(Risk::ReadOnly));
    }

    #[test]
    fn always_allow_permits_everything_including_destructive_work() {
        // Only safe inside a sandbox, which is why it is not the default.
        assert!(PermissionPolicy::AlwaysAllow.permits(Risk::High));
    }

    #[test]
    fn an_agent_whose_command_is_missing_fails_its_health_check() {
        let adapter = AcpAdapter::new("ghost", "definitely-not-a-real-binary-93f2a --acp");
        assert!(!adapter.is_launchable());
    }

    #[tokio::test]
    async fn a_missing_command_is_reported_as_configuration_not_as_a_crash() {
        let adapter = AcpAdapter::new("ghost", "definitely-not-a-real-binary-93f2a");
        let err = adapter.health_check().await.unwrap_err();
        assert_eq!(err.class(), ErrorClass::Configuration);
        assert!(err.to_string().contains("PATH"));
    }

    #[tokio::test]
    async fn describe_returns_the_configured_descriptor_without_spawning() {
        let adapter = AcpAdapter::new("codex", "echo hello");
        let descriptor = adapter.describe().await.unwrap();
        assert_eq!(descriptor.id, AgentId::new("codex"));
        assert_eq!(descriptor.protocol, AgentProtocol::Acp);
    }

    #[test]
    fn an_unparseable_command_is_a_configuration_error() {
        let adapter = AcpAdapter::new("broken", "unterminated 'quote");
        let err = adapter.spawn_handle().unwrap_err();
        assert_eq!(err.class(), ErrorClass::Configuration);
    }

    #[test]
    fn end_turn_is_the_only_stop_reason_that_counts_as_success() {
        assert!(interpret_stop_reason(StopReason::EndTurn).is_ok());

        for reason in [
            StopReason::MaxTokens,
            StopReason::MaxTurnRequests,
            StopReason::Refusal,
            StopReason::Cancelled,
        ] {
            assert!(interpret_stop_reason(reason).is_err(), "{reason:?}");
        }
    }

    #[test]
    fn hitting_the_token_limit_is_classified_as_a_context_overflow() {
        // This matters: ContextOverflow triggers a replan, whereas a generic
        // failure would trigger a pointless retry with the same oversized prompt.
        let (class, _) = interpret_stop_reason(StopReason::MaxTokens).unwrap_err();
        assert_eq!(class, ErrorClass::ContextOverflow);
        assert!(class.requires_replan());
    }

    #[test]
    fn cancellation_is_classified_as_cancellation_not_as_a_failure() {
        let (class, _) = interpret_stop_reason(StopReason::Cancelled).unwrap_err();
        assert_eq!(class, ErrorClass::Cancelled);
        assert!(!class.counts_against_health());
    }

    #[test]
    fn the_adapter_is_usable_behind_the_port_trait() {
        let adapter: Arc<dyn AgentAdapter> = Arc::new(AcpAdapter::new("codex", "echo"));
        assert_eq!(adapter.agent_id(), &AgentId::new("codex"));
    }
}
