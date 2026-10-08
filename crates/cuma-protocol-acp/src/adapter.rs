//! The [`AgentAdapter`] implementation for ACP agents.

use crate::capabilities::capabilities_from_initialize;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, McpServer, McpServerStdio, NewSessionRequest, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, StopReason, TextContent, Usage,
    UsageUpdate,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo};
use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{
    AgentAdapter, ExecutionRequest, ExecutionUpdate, LaunchPurpose, SandboxLaunch,
};
use cuma_core::{
    AgentDescriptor, AgentId, AgentProtocol, AttemptId, ErrorClass, ExecutionOutcome, Risk,
    TokenUsage,
};
use std::path::{Path, PathBuf};
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
    mcp_servers: Vec<SharedMcpServer>,
    /// Starts the agent inside a sandbox, e.g. `ai-jail --exec … --`.
    launcher: Option<Arc<dyn AgentLauncher>>,
    /// Environment variables the agent needs beyond a sandbox's baseline.
    kept_env: Vec<String>,
}

/// Starts agents confined to the directory they work in.
///
/// Asked afresh for every launch: an agent working in a task's worktree must
/// be able to write that worktree, not the directory CUMA started in.
#[async_trait]
pub trait AgentLauncher: Send + Sync {
    /// The command-line prefix for an agent working in `workspace`, keeping
    /// the environment variables named in `keep_env` and able to read the
    /// paths its own command names in `readable`.
    ///
    /// For a sandbox with a lifecycle this is only indicative — its first
    /// word is the program a launch needs — and [`open`](Self::open) is what
    /// prepares a real one.
    fn prefix(&self, workspace: &Path, keep_env: &[String], readable: &[PathBuf]) -> Vec<String>;

    /// Whether agents run on another system — a container, a virtual
    /// machine, a remote service — whose programs this machine's `PATH` says
    /// nothing about. Their own `initialize` is then the only test of their
    /// command.
    fn runs_elsewhere(&self) -> bool {
        false
    }

    /// Prepare one launch of an agent working in `workspace`.
    ///
    /// A launcher that is a prefix needs nothing more. A sandbox with a
    /// lifecycle of its own — a microVM, a pod, a remote service — is created
    /// here, and torn down when the returned launch is finished or dropped.
    async fn open(
        &self,
        workspace: &Path,
        purpose: LaunchPurpose,
        keep_env: &[String],
        readable: &[PathBuf],
    ) -> Result<SandboxLaunch> {
        let _ = purpose;
        Ok(SandboxLaunch::new(
            self.prefix(workspace, keep_env, readable),
        ))
    }
}

/// A fixed prefix, whatever the workspace.
struct FixedPrefix(Vec<String>);

impl AgentLauncher for FixedPrefix {
    fn prefix(
        &self,
        _workspace: &Path,
        _keep_env: &[String],
        _readable: &[PathBuf],
    ) -> Vec<String> {
        self.0.clone()
    }
}

/// An MCP server to hand an agent in `session/new`.
///
/// Plain data, so whoever builds it needs no ACP types. Every ACP agent must
/// accept stdio servers, so that is the only transport offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedMcpServer {
    /// The name the agent will know it by.
    pub name: String,
    /// The program to launch.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
}

impl SharedMcpServer {
    fn to_acp(&self) -> McpServer {
        McpServer::Stdio(
            McpServerStdio::new(self.name.clone(), self.command.clone()).args(self.args.clone()),
        )
    }
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
            mcp_servers: Vec::new(),
            launcher: None,
            kept_env: Vec::new(),
        }
    }

    /// Launch the agent under a fixed `prefix`.
    #[must_use]
    pub fn with_launch_prefix(self, prefix: Vec<String>) -> Self {
        if prefix.is_empty() {
            return self;
        }
        self.with_launcher(Arc::new(FixedPrefix(prefix)))
    }

    /// Launch the agent through `launcher` — a sandbox, typically.
    #[must_use]
    pub fn with_launcher(mut self, launcher: Arc<dyn AgentLauncher>) -> Self {
        self.launcher = Some(launcher);
        self
    }

    /// Keep these environment variables when the agent is sandboxed — the
    /// secrets MCP servers it launches will look for, say.
    #[must_use]
    pub fn with_kept_env(mut self, names: Vec<String>) -> Self {
        self.kept_env = names;
        self
    }

    /// The full command an agent working in `workspace` is launched with,
    /// sandbox included.
    ///
    /// Leading `NAME=value` assignments in the configured command set the
    /// agent's environment. They are moved ahead of the sandbox — which would
    /// otherwise try to run `NAME=value` as a program — and their names kept.
    pub fn launch_command(&self, workspace: &Path) -> String {
        let (Some(launcher), Some(parts)) = (&self.launcher, self.command_parts()) else {
            return self.command.clone();
        };
        let prefix = launcher.prefix(workspace, &parts.keep, &parts.readable);
        parts.under(&prefix).unwrap_or_else(|| self.command.clone())
    }

    /// Prepare a launch in `workspace`: the sandbox, if any, and the command
    /// to run in it. The launch must be finished once the agent has exited;
    /// dropping it instead tears the sandbox down.
    async fn open_launch(
        &self,
        workspace: &Path,
        purpose: LaunchPurpose,
    ) -> Result<(String, Option<SandboxLaunch>)> {
        let (Some(launcher), Some(parts)) = (&self.launcher, self.command_parts()) else {
            return Ok((self.command.clone(), None));
        };
        let launch = launcher
            .open(workspace, purpose, &parts.keep, &parts.readable)
            .await?;
        let command = parts
            .under(launch.prefix())
            .unwrap_or_else(|| self.command.clone());
        Ok((command, Some(launch)))
    }

    /// The configured command, taken apart for a launcher.
    fn command_parts(&self) -> Option<CommandParts> {
        let words = shell_words::split(&self.command).ok()?;
        let assignments: Vec<String> = words
            .iter()
            .take_while(|w| is_assignment(w))
            .cloned()
            .collect();
        let program = words[assignments.len()..].to_vec();
        let mut keep = self.kept_env.clone();
        keep.extend(
            assignments
                .iter()
                .filter_map(|a| a.split_once('=').map(|(name, _)| name.to_owned())),
        );

        // The agent's program and any path its command names — a script in
        // a project folder, a binary under $HOME — were chosen by whoever
        // configured it, and must stay visible inside the sandbox. Where the
        // agent runs on another system, its program is that system's, not
        // whatever this machine's PATH finds under the same name.
        let elsewhere = self
            .launcher
            .as_ref()
            .is_some_and(|launcher| launcher.runs_elsewhere());
        let readable: Vec<PathBuf> = program
            .iter()
            .enumerate()
            .filter_map(|(index, word)| {
                let path = Path::new(word);
                if path.is_absolute() {
                    Some(path.to_path_buf())
                } else if index == 0 && !elsewhere {
                    which::which(word).ok()
                } else {
                    None
                }
            })
            .filter(|path| path.exists())
            .collect();

        Some(CommandParts {
            assignments,
            program,
            keep,
            readable,
        })
    }

    /// MCP servers to offer the agent in every session.
    #[must_use]
    pub fn with_mcp_servers(mut self, servers: Vec<SharedMcpServer>) -> Self {
        self.mcp_servers = servers;
        self
    }

    /// The MCP servers this adapter offers its agent.
    pub fn mcp_servers(&self) -> &[SharedMcpServer] {
        &self.mcp_servers
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
    /// reported as missing rather than silently failing at routing time. An
    /// agent that runs elsewhere — in a container, a virtual machine — is
    /// checked only for its sandbox's program: its own command lives there.
    pub fn is_launchable(&self) -> bool {
        let Ok(parts) = shell_words::split(&self.command) else {
            return false;
        };
        let elsewhere = self
            .launcher
            .as_ref()
            .is_some_and(|launcher| launcher.runs_elsewhere());
        let agent = elsewhere
            || parts
                .iter()
                .find(|word| !is_assignment(word))
                .is_some_and(|binary| which::which(binary).is_ok());
        let here = std::env::current_dir().unwrap_or_default();
        let launcher = self.launcher.as_ref().is_none_or(|launcher| {
            launcher
                .prefix(&here, &[], &[])
                .first()
                .is_none_or(|binary| which::which(binary).is_ok())
        });
        agent && launcher
    }

    /// Build the SDK's agent handle for `command`.
    fn spawn_handle(&self, command: &str) -> Result<AcpAgent> {
        AcpAgent::from_str(command).map_err(|err| {
            MetaAgentError::Configuration(format!(
                "agent {}: cannot parse command {:?}: {err}",
                self.id, self.command
            ))
        })
    }
}

/// A configured command, taken apart: what a launcher needs to know about it,
/// and how to put it back together under a sandbox's prefix.
struct CommandParts {
    /// Leading `NAME=value` words.
    assignments: Vec<String>,
    /// The program and its arguments.
    program: Vec<String>,
    /// Variables the agent keeps.
    keep: Vec<String>,
    /// Paths the command names.
    readable: Vec<PathBuf>,
}

impl CommandParts {
    /// The command under `prefix`, or `None` when there is no prefix.
    fn under(&self, prefix: &[String]) -> Option<String> {
        if prefix.is_empty() {
            return None;
        }
        let words = self
            .assignments
            .iter()
            .chain(prefix)
            .chain(&self.program)
            .map(String::as_str);
        Some(shell_words::join(words))
    }
}

/// Whether a command word is a `NAME=value` environment assignment.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
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

/// Work out an attempt's token usage from what the agent reported.
///
/// In order of preference:
///
/// 1. the turn's own usage (`PromptResponse.usage`) — exact, and marked
///    reported;
/// 2. the last `UsageUpdate`: its `used` is the context the agent held, which
///    for a fresh session is the input it consumed, but output is not
///    reported, so it is estimated from the transcript and the whole figure
///    is marked estimated;
/// 3. an estimate from the text exchanged.
///
/// Each attempt runs in a fresh ACP session, so session-cumulative figures
/// are this attempt's figures.
fn account_tokens(
    turn: Option<&Usage>,
    context: Option<&UsageUpdate>,
    prompt: &str,
    output: &str,
) -> TokenUsage {
    if let Some(turn) = turn {
        let mut tokens = TokenUsage::reported(turn.input_tokens, turn.output_tokens);
        tokens.cached = turn.cached_read_tokens.unwrap_or(0);
        return tokens;
    }

    let estimate = TokenUsage::estimate_from_text(prompt, output);
    match context {
        Some(report) if report.used > 0 => TokenUsage::estimated(report.used, estimate.output),
        _ => estimate,
    }
}

/// The cost an agent reported, when it reported one in dollars.
///
/// Other currencies are not converted: an exchange rate CUMA made up would be
/// exactly the kind of estimate presented as a measurement it refuses to make.
fn reported_cost(report: &UsageUpdate) -> Option<f64> {
    let cost = report.cost.as_ref()?;
    (cost.currency.eq_ignore_ascii_case("USD") && cost.amount.is_finite() && cost.amount >= 0.0)
        .then_some(cost.amount)
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
        // Dropped on any early return below, which tears its sandbox down.
        let (command, launch) = self
            .open_launch(&request.workspace, LaunchPurpose::Execute)
            .await?;
        let agent = self.spawn_handle(&command)?;

        let risk = request.task.spec.risk;
        let policy = self.permission_policy;
        let prompt_text = request.prompt.clone();
        let workspace = request.workspace.clone();
        let agent_id = self.id.clone();
        let mcp_servers: Vec<McpServer> = self
            .mcp_servers
            .iter()
            .map(SharedMcpServer::to_acp)
            .collect();

        // Collected assistant text and the latest usage report, shared with
        // the notification handler.
        let transcript = Arc::new(Mutex::new(String::new()));
        let transcript_for_handler = Arc::clone(&transcript);
        let usage = Arc::new(Mutex::new(None::<UsageUpdate>));
        let usage_for_handler = Arc::clone(&usage);
        let updates_for_handler = updates.clone();

        let turn = agent_client_protocol::Client
            .builder()
            .name("cuma")
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    if let SessionUpdate::UsageUpdate(report) = &notification.update {
                        *usage_for_handler.lock().await = Some(report.clone());
                    }
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
                    .send_request(NewSessionRequest::new(workspace).mcp_servers(mcp_servers))
                    .block_task()
                    .await?;

                let response = connection
                    .send_request(PromptRequest::new(
                        session.session_id,
                        vec![ContentBlock::Text(TextContent::new(prompt_text))],
                    ))
                    .block_task()
                    .await?;

                Ok((response.stop_reason, response.usage))
            })
            .await
            .map_err(|err| {
                // An ACP-level error is a transport or protocol failure. The
                // orchestrator re-classifies from the message when this comes
                // back as a generic protocol error.
                MetaAgentError::protocol_msg("acp", format!("agent {agent_id} failed: {err}"))
            });

        // The sandbox must not outlive the turn, and the agent's work comes
        // back whatever the turn's outcome — as it would have stayed in a
        // mounted workspace.
        let collected = match launch {
            Some(launch) => launch.finish().await,
            None => Ok(()),
        };
        let (stop_reason, turn_usage) = match turn {
            Ok(turn) => turn,
            Err(err) => {
                if let Err(collect) = collected {
                    tracing::warn!(agent = %self.id, error = %collect, "closing the sandbox failed too");
                }
                return Err(err);
            }
        };

        let output = transcript.lock().await.clone();
        let context_report = usage.lock().await.clone();
        let tokens = account_tokens(
            turn_usage.as_ref(),
            context_report.as_ref(),
            &request.prompt,
            &output,
        );
        let reported_cost_usd = context_report.as_ref().and_then(reported_cost);
        #[allow(clippy::cast_possible_truncation)]
        let latency_ms = started.elapsed().as_millis() as u64;

        let (success, failure_class, failure_reason) =
            match (interpret_stop_reason(stop_reason), collected) {
                (Ok(()), Ok(())) => (true, None, None),
                (Err((class, reason)), _) => (false, Some(class), Some(reason)),
                // The agent finished, but its work did not reach the
                // workspace: the task did not happen.
                (Ok(()), Err(err)) => (
                    false,
                    Some(ErrorClass::TaskFailure),
                    Some(format!(
                        "the agent's work could not be brought back from its sandbox: {err}"
                    )),
                ),
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
            tokens,
            latency_ms,
            failure_class,
            failure_reason,
            reported_cost_usd,
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
        // No task yet, so no workspace of its own: negotiation only
        // exchanges capabilities, from where CUMA runs.
        let here = std::env::current_dir().unwrap_or_default();
        let (command, launch) = self.open_launch(&here, LaunchPurpose::Negotiate).await?;
        let agent = self.spawn_handle(&command)?;
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
            .await;
        if let Some(launch) = launch
            && let Err(err) = launch.finish().await
        {
            tracing::warn!(agent = %self.id, error = %err, "closing the negotiation sandbox failed");
        }
        let response = response.map_err(|err| {
            MetaAgentError::protocol_msg(
                "acp",
                format!("agent {agent_id}: initialize failed: {err}"),
            )
        })?;

        let mut descriptor = self.descriptor.lock().await;
        // Added to what was configured, never in place of it: negotiation
        // yields a coding baseline, and the operator may know more.
        descriptor
            .capabilities
            .extend(capabilities_from_initialize(&response).iter().cloned());

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
    use agent_client_protocol::schema::v1::Cost;

    #[test]
    fn a_sandboxed_agent_is_launched_under_its_prefix_with_quoting_intact() {
        let adapter = AcpAdapter::new(
            "claude",
            "npx -y @agentclientprotocol/claude-agent-acp@latest",
        )
        .with_launch_prefix(vec![
            "ai-jail".into(),
            "--exec".into(),
            "--rw-map".into(),
            "/work/my project".into(),
            "--".into(),
        ]);
        let command = adapter.launch_command(Path::new("/work"));
        assert_eq!(
            shell_words::split(&command).unwrap(),
            vec![
                "ai-jail",
                "--exec",
                "--rw-map",
                "/work/my project",
                "--",
                "npx",
                "-y",
                "@agentclientprotocol/claude-agent-acp@latest"
            ]
        );
    }

    #[test]
    fn leading_assignments_go_ahead_of_the_sandbox_and_are_kept() {
        struct Recording(std::sync::Mutex<Vec<String>>);
        impl AgentLauncher for Recording {
            fn prefix(&self, workspace: &Path, keep_env: &[String], _: &[PathBuf]) -> Vec<String> {
                *self.0.lock().unwrap() = keep_env.to_vec();
                vec![
                    "bwrap".into(),
                    "--chdir".into(),
                    workspace.display().to_string(),
                    "--".into(),
                ]
            }
        }
        let launcher = Arc::new(Recording(std::sync::Mutex::default()));
        let adapter = AcpAdapter::new("codex", "RUST_LOG=debug codex-acp --flag")
            .with_kept_env(vec!["GH_TOKEN".into()])
            .with_launcher(launcher.clone());

        let command = adapter.launch_command(Path::new("/work/task 1"));
        assert_eq!(
            shell_words::split(&command).unwrap(),
            vec![
                "RUST_LOG=debug",
                "bwrap",
                "--chdir",
                "/work/task 1",
                "--",
                "codex-acp",
                "--flag"
            ]
        );
        assert_eq!(*launcher.0.lock().unwrap(), vec!["GH_TOKEN", "RUST_LOG"]);
        // The SDK reads leading assignments as the process environment.
        assert!(AcpAgent::from_str(&command).is_ok());
    }

    #[test]
    fn an_agent_whose_sandbox_is_missing_is_not_launchable() {
        let adapter = AcpAdapter::new("echo", "echo")
            .with_launch_prefix(vec!["definitely-not-a-sandbox-5b1c".into(), "--".into()]);
        assert!(!adapter.is_launchable());
    }

    #[test]
    fn a_shared_mcp_server_is_declared_as_a_stdio_server() {
        let shared = SharedMcpServer {
            name: "git".into(),
            command: "/usr/bin/cuma".into(),
            args: vec!["mcp".into(), "proxy".into(), "git".into()],
        };
        let json = serde_json::to_value(shared.to_acp()).unwrap();

        assert_eq!(json["name"], "git");
        assert_eq!(json["command"], "/usr/bin/cuma");
        assert_eq!(json["args"][2], "git");
        assert!(
            json.get("type").is_none(),
            "stdio servers carry no type tag"
        );
        assert_eq!(
            json["env"],
            serde_json::json!([]),
            "no secrets travel in the request"
        );
    }

    #[test]
    fn discovered_agents_are_offered_the_shared_servers() {
        let config = cuma_config::Config::from_toml(
            "[agents.echo]\nprotocol = \"acp\"\ncommand = \"echo\"\n",
        )
        .unwrap();
        let shared = vec![SharedMcpServer {
            name: "git".into(),
            command: "cuma".into(),
            args: Vec::new(),
        }];

        let adapters = crate::AcpConfigDiscovery::new(config)
            .with_mcp_servers(shared.clone())
            .adapters();
        assert_eq!(adapters[0].mcp_servers(), shared.as_slice());
    }

    #[test]
    fn a_turns_own_usage_is_taken_as_reported() {
        let turn = Usage::new(1_500, 1_200, 300);
        let tokens = account_tokens(Some(&turn), None, "prompt", "output");
        assert!(tokens.reported);
        assert_eq!((tokens.input, tokens.output), (1_200, 300));
    }

    #[test]
    fn a_context_report_supplies_input_but_the_total_stays_an_estimate() {
        let context = UsageUpdate::new(40_000, 200_000);
        let tokens = account_tokens(None, Some(&context), "prompt", &"x".repeat(400));
        assert_eq!(tokens.input, 40_000);
        assert_eq!(tokens.output, 100);
        assert!(
            !tokens.reported,
            "output was estimated, so the whole figure is"
        );
    }

    #[test]
    fn with_no_report_tokens_are_estimated_from_the_text_and_never_zero() {
        let tokens = account_tokens(None, None, &"p".repeat(80), &"o".repeat(40));
        assert!(!tokens.reported);
        assert_eq!((tokens.input, tokens.output), (20, 10));
    }

    #[test]
    fn only_a_dollar_cost_is_taken_at_face_value() {
        let usd = UsageUpdate::new(1, 1).cost(Cost::new(0.045, "USD"));
        assert_eq!(reported_cost(&usd), Some(0.045));

        let eur = UsageUpdate::new(1, 1).cost(Cost::new(0.045, "EUR"));
        assert_eq!(reported_cost(&eur), None, "no invented exchange rate");

        let nonsense = UsageUpdate::new(1, 1).cost(Cost::new(-3.0, "USD"));
        assert_eq!(reported_cost(&nonsense), None);

        assert_eq!(reported_cost(&UsageUpdate::new(1, 1)), None);
    }

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
        let err = adapter
            .spawn_handle(&adapter.launch_command(Path::new("/w")))
            .unwrap_err();
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

    // --- sandbox launches -------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    /// How the launches a [`Recording`] launcher opened ended.
    #[derive(Default)]
    struct Lifecycle {
        opened: std::sync::Mutex<Vec<LaunchPurpose>>,
        finished: AtomicUsize,
        aborted: AtomicUsize,
    }

    struct Ended(Arc<Lifecycle>);

    #[async_trait]
    impl cuma_core::ports::SandboxSession for Ended {
        async fn finish(&self) -> Result<()> {
            self.0.finished.fetch_add(1, SeqCst);
            Ok(())
        }

        async fn abort(&self) {
            self.0.aborted.fetch_add(1, SeqCst);
        }
    }

    /// A launcher whose every launch has a session, run under `env`.
    struct Recording(Arc<Lifecycle>);

    #[async_trait]
    impl AgentLauncher for Recording {
        fn prefix(&self, _: &Path, _: &[String], _: &[PathBuf]) -> Vec<String> {
            vec!["env".into()]
        }

        async fn open(
            &self,
            _: &Path,
            purpose: LaunchPurpose,
            _: &[String],
            _: &[PathBuf],
        ) -> Result<SandboxLaunch> {
            self.0.opened.lock().unwrap().push(purpose);
            Ok(SandboxLaunch::with_session(
                vec!["env".into()],
                Arc::new(Ended(Arc::clone(&self.0))),
            ))
        }
    }

    /// The minimal ACP agent fixture, or `None` without python3.
    fn minimal_agent() -> Option<String> {
        which::which("python3").ok()?;
        let script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/minimal_agent.py"
        );
        Some(shell_words::join(["python3", script]))
    }

    fn request() -> ExecutionRequest {
        ExecutionRequest {
            task: cuma_core::Task::new(cuma_core::TaskSpec::new(
                "say hello",
                cuma_core::TaskType::Research,
            )),
            model: None,
            prompt: "say hello".to_owned(),
            workspace: std::env::temp_dir(),
            handoff: None,
            timeout_ms: 20_000,
        }
    }

    async fn settle(count: &AtomicUsize, expected: usize) {
        for _ in 0..200 {
            if count.load(SeqCst) == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_launch_is_finished_after_the_turn() {
        let Some(command) = minimal_agent() else {
            return;
        };
        let lifecycle = Arc::new(Lifecycle::default());
        let adapter = AcpAdapter::new("minimal", command)
            .with_launcher(Arc::new(Recording(Arc::clone(&lifecycle))));

        let (tx, _rx) = mpsc::channel(16);
        let outcome = adapter.execute(request(), tx).await.unwrap();

        assert!(outcome.success, "{:?}", outcome.failure_reason);
        assert_eq!(outcome.output, "done");
        assert_eq!(
            *lifecycle.opened.lock().unwrap(),
            vec![LaunchPurpose::Execute]
        );
        assert_eq!(lifecycle.finished.load(SeqCst), 1);
        tokio::task::yield_now().await;
        assert_eq!(lifecycle.aborted.load(SeqCst), 0, "finished, not aborted");
    }

    #[tokio::test]
    async fn negotiation_opens_a_launch_that_needs_no_workspace() {
        let Some(command) = minimal_agent() else {
            return;
        };
        let lifecycle = Arc::new(Lifecycle::default());
        let adapter = AcpAdapter::new("minimal", command)
            .with_launcher(Arc::new(Recording(Arc::clone(&lifecycle))));

        adapter.refresh_capabilities().await.unwrap();

        assert_eq!(
            *lifecycle.opened.lock().unwrap(),
            vec![LaunchPurpose::Negotiate]
        );
        assert_eq!(lifecycle.finished.load(SeqCst), 1);
    }

    /// A launcher for agents on another system, recording what it was told
    /// the agent's command names.
    struct Elsewhere(std::sync::Mutex<Vec<PathBuf>>);

    impl AgentLauncher for Elsewhere {
        fn prefix(&self, _: &Path, _: &[String], readable: &[PathBuf]) -> Vec<String> {
            *self.0.lock().unwrap() = readable.to_vec();
            vec!["env".into()]
        }

        fn runs_elsewhere(&self) -> bool {
            true
        }
    }

    #[test]
    fn an_agent_on_another_system_is_not_resolved_on_this_machines_path() {
        let launcher = Arc::new(Elsewhere(std::sync::Mutex::new(Vec::new())));
        let script = tempfile::NamedTempFile::new().unwrap();
        let command = shell_words::join(["sh", &script.path().display().to_string()]);
        let adapter = AcpAdapter::new("guest", command).with_launcher(launcher.clone());

        adapter.launch_command(Path::new("/w"));

        // The script it names travels; this machine's `sh` does not.
        assert_eq!(*launcher.0.lock().unwrap(), [script.path().to_path_buf()]);
        assert!(
            adapter.is_launchable(),
            "its program is checked where it runs"
        );
    }

    #[tokio::test]
    async fn a_launch_is_aborted_when_the_turn_is_abandoned() {
        let lifecycle = Arc::new(Lifecycle::default());
        // An agent that never answers `initialize`.
        let adapter = AcpAdapter::new("silent", "sleep 30")
            .with_launcher(Arc::new(Recording(Arc::clone(&lifecycle))));

        let (tx, _rx) = mpsc::channel(16);
        let abandoned = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            adapter.execute(request(), tx),
        )
        .await;

        assert!(
            abandoned.is_err(),
            "the turn should still have been running"
        );
        settle(&lifecycle.aborted, 1).await;
        assert_eq!(lifecycle.aborted.load(SeqCst), 1);
        assert_eq!(
            lifecycle.finished.load(SeqCst),
            0,
            "nothing is brought back"
        );
    }
}
