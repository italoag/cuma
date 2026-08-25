//! Mock agents for testing the harness without spending a token.
//!
//! Every failure mode the resilience layer claims to handle needs a way to be
//! reproduced deterministically. [`MockAgent`] can be scripted to succeed, to
//! stall past its deadline, to return a rate limit, to crash mid-stream or to
//! run out of quota — and to change behaviour between attempts, which is what
//! makes retry and fallback testable at all.

use async_trait::async_trait;
use cuma_core::error::{MetaAgentError, Result};
use cuma_core::ports::{AgentAdapter, ExecutionRequest, ExecutionUpdate};
use cuma_core::{
    AgentDescriptor, AgentId, AgentProtocol, AttemptId, CapabilitySet, ErrorClass,
    ExecutionOutcome, TokenUsage,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

/// How a mock agent behaves on one attempt.
#[derive(Debug, Clone)]
pub enum Behaviour {
    /// Complete successfully.
    Succeed {
        /// The output to return.
        output: String,
        /// Files to report as changed.
        changed_files: Vec<String>,
    },
    /// Take `delay` before succeeding. Used to test timeouts and latency scoring.
    Slow {
        /// How long to take.
        delay: Duration,
        /// What to return afterwards.
        output: String,
    },
    /// Never return within the deadline.
    Timeout,
    /// Return a rate limit error.
    RateLimit {
        /// Server-suggested wait.
        retry_after_ms: Option<u64>,
    },
    /// Report the quota as exhausted.
    QuotaExceeded,
    /// Emit part of a stream, then die.
    PartialStream {
        /// Chunks to emit before dying.
        chunks: Vec<String>,
    },
    /// Die immediately.
    Crash {
        /// The death message.
        message: String,
    },
    /// Return something the harness cannot use.
    InvalidResponse,
    /// Reject the credentials.
    AuthFailure,
    /// Report the prompt as too long.
    ContextOverflow,
    /// Complete, but report the task itself as failed.
    TaskFailure {
        /// Why.
        reason: String,
    },
}

impl Behaviour {
    /// A plain success.
    pub fn ok(output: &str) -> Self {
        Self::Succeed {
            output: output.to_owned(),
            changed_files: Vec::new(),
        }
    }
}

/// A scriptable agent that speaks no protocol at all.
pub struct MockAgent {
    descriptor: AgentDescriptor,
    /// One behaviour per attempt. The last entry repeats once exhausted, so a
    /// mock scripted to always fail keeps failing rather than silently
    /// succeeding after the script runs out.
    script: Vec<Behaviour>,
    calls: Arc<AtomicUsize>,
    tokens_per_call: TokenUsage,
}

impl MockAgent {
    /// An agent that always behaves the same way.
    pub fn always(id: &str, behaviour: Behaviour) -> Self {
        Self::scripted(id, vec![behaviour])
    }

    /// An agent whose behaviour changes per attempt.
    pub fn scripted(id: &str, script: Vec<Behaviour>) -> Self {
        Self {
            descriptor: AgentDescriptor::new(id, id, AgentProtocol::Native),
            script: if script.is_empty() {
                vec![Behaviour::ok("done")]
            } else {
                script
            },
            calls: Arc::new(AtomicUsize::new(0)),
            tokens_per_call: TokenUsage::reported(100, 50),
        }
    }

    /// Advertise capabilities.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.descriptor.capabilities = capabilities;
        self
    }

    /// Replace the descriptor wholesale.
    #[must_use]
    pub fn with_descriptor(mut self, descriptor: AgentDescriptor) -> Self {
        self.descriptor = descriptor;
        self
    }

    /// Set the tokens each call reports.
    #[must_use]
    pub fn with_tokens(mut self, tokens: TokenUsage) -> Self {
        self.tokens_per_call = tokens;
        self
    }

    /// The descriptor this agent advertises.
    pub fn descriptor(&self) -> &AgentDescriptor {
        &self.descriptor
    }

    /// How many times this agent has been called.
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// A handle to the call counter, for assertions after the agent is moved
    /// into an `Arc<dyn AgentAdapter>`.
    pub fn call_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.calls)
    }

    fn next_behaviour(&self) -> Behaviour {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let clamped = index.min(self.script.len() - 1);
        self.script[clamped].clone()
    }
}

#[async_trait]
impl AgentAdapter for MockAgent {
    fn agent_id(&self) -> &AgentId {
        &self.descriptor.id
    }

    async fn describe(&self) -> Result<AgentDescriptor> {
        Ok(self.descriptor.clone())
    }

    async fn execute(
        &self,
        request: ExecutionRequest,
        updates: mpsc::Sender<ExecutionUpdate>,
    ) -> Result<ExecutionOutcome> {
        let behaviour = self.next_behaviour();
        let started = std::time::Instant::now();

        let outcome = |success: bool,
                       output: String,
                       changed_files: Vec<String>,
                       class: Option<ErrorClass>,
                       reason: Option<String>| {
            ExecutionOutcome {
                attempt_id: AttemptId::generate(),
                agent_id: self.descriptor.id.clone(),
                model_id: request.model.clone(),
                success,
                output,
                changed_files,
                tokens: self.tokens_per_call,
                #[allow(clippy::cast_possible_truncation)]
                latency_ms: started.elapsed().as_millis() as u64,
                failure_class: class,
                failure_reason: reason,
            }
        };

        match behaviour {
            Behaviour::Succeed {
                output,
                changed_files,
            } => {
                let _ = updates
                    .send(ExecutionUpdate::Text {
                        content: output.clone(),
                    })
                    .await;
                Ok(outcome(true, output, changed_files, None, None))
            }

            Behaviour::Slow { delay, output } => {
                // Respect the caller's deadline rather than sleeping through
                // it: a mock that ignores cancellation cannot test cancellation.
                let deadline = Duration::from_millis(request.timeout_ms);
                if tokio::time::timeout(deadline, tokio::time::sleep(delay))
                    .await
                    .is_err()
                {
                    return Err(MetaAgentError::Timeout {
                        operation: format!("mock agent {}", self.descriptor.id),
                        elapsed_ms: request.timeout_ms,
                    });
                }
                Ok(outcome(true, output, Vec::new(), None, None))
            }

            Behaviour::Timeout => {
                tokio::time::sleep(Duration::from_millis(request.timeout_ms)).await;
                Err(MetaAgentError::Timeout {
                    operation: format!("mock agent {}", self.descriptor.id),
                    elapsed_ms: request.timeout_ms,
                })
            }

            Behaviour::RateLimit { retry_after_ms } => Err(MetaAgentError::RateLimit {
                agent: self.descriptor.id.clone(),
                retry_after_ms,
            }),

            Behaviour::QuotaExceeded => Err(MetaAgentError::agent(
                self.descriptor.id.clone(),
                "monthly quota exhausted",
                ErrorClass::QuotaExceeded,
            )),

            Behaviour::PartialStream { chunks } => {
                for chunk in chunks {
                    let _ = updates.send(ExecutionUpdate::Text { content: chunk }).await;
                }
                Err(MetaAgentError::agent(
                    self.descriptor.id.clone(),
                    "stream ended unexpectedly",
                    ErrorClass::AgentCrash,
                ))
            }

            Behaviour::Crash { message } => Err(MetaAgentError::agent(
                self.descriptor.id.clone(),
                message,
                ErrorClass::AgentCrash,
            )),

            Behaviour::InvalidResponse => Err(MetaAgentError::agent(
                self.descriptor.id.clone(),
                "response was not valid JSON",
                ErrorClass::InvalidResponse,
            )),

            Behaviour::AuthFailure => Err(MetaAgentError::Authentication {
                target: self.descriptor.id.to_string(),
                message: "credentials rejected".to_owned(),
            }),

            Behaviour::ContextOverflow => Err(MetaAgentError::agent(
                self.descriptor.id.clone(),
                "prompt exceeds the model's context window",
                ErrorClass::ContextOverflow,
            )),

            Behaviour::TaskFailure { reason } => Ok(outcome(
                false,
                String::new(),
                Vec::new(),
                Some(ErrorClass::TaskFailure),
                Some(reason),
            )),
        }
    }

    async fn health_check(&self) -> Result<()> {
        Ok(())
    }
}

/// Build an [`ExecutionRequest`] for a task, for tests.
pub fn request_for(task: &cuma_core::Task) -> ExecutionRequest {
    ExecutionRequest {
        task: task.clone(),
        model: None,
        prompt: task.spec.description.clone(),
        workspace: std::path::PathBuf::from("."),
        handoff: None,
        timeout_ms: 5_000,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{Task, TaskSpec, TaskType};

    fn task() -> Task {
        Task::new(TaskSpec::new("do the thing", TaskType::General))
    }

    async fn run(agent: &MockAgent, task: &Task) -> Result<ExecutionOutcome> {
        let (tx, mut rx) = mpsc::channel(64);
        let handle = agent.execute(request_for(task), tx);
        let drain = async move {
            let mut chunks = Vec::new();
            while let Some(update) = rx.recv().await {
                chunks.push(update);
            }
            chunks
        };
        let (result, _) = tokio::join!(handle, drain);
        result
    }

    #[tokio::test]
    async fn a_successful_mock_reports_success_and_tokens() {
        let agent = MockAgent::always("m", Behaviour::ok("all done"));
        let outcome = run(&agent, &task()).await.unwrap();

        assert!(outcome.success);
        assert_eq!(outcome.output, "all done");
        assert_eq!(outcome.tokens.total(), 150);
        assert!(outcome.tokens.reported);
    }

    #[tokio::test]
    async fn each_failure_mode_produces_the_class_the_policy_expects() {
        let cases = [
            (
                Behaviour::RateLimit {
                    retry_after_ms: Some(1000),
                },
                ErrorClass::RateLimit,
            ),
            (Behaviour::QuotaExceeded, ErrorClass::QuotaExceeded),
            (
                Behaviour::Crash {
                    message: "boom".into(),
                },
                ErrorClass::AgentCrash,
            ),
            (Behaviour::InvalidResponse, ErrorClass::InvalidResponse),
            (Behaviour::AuthFailure, ErrorClass::AuthenticationFailure),
            (Behaviour::ContextOverflow, ErrorClass::ContextOverflow),
        ];

        for (behaviour, expected) in cases {
            let agent = MockAgent::always("m", behaviour.clone());
            let err = run(&agent, &task()).await.unwrap_err();
            assert_eq!(err.class(), expected, "wrong class for {behaviour:?}");
        }
    }

    #[tokio::test]
    async fn a_timeout_is_reported_as_a_timeout() {
        let agent = MockAgent::always("m", Behaviour::Timeout);
        let mut request = request_for(&task());
        request.timeout_ms = 20;

        let (tx, _rx) = mpsc::channel(8);
        let err = agent.execute(request, tx).await.unwrap_err();
        assert_eq!(err.class(), ErrorClass::Timeout);
    }

    #[tokio::test]
    async fn a_slow_agent_that_finishes_inside_its_deadline_succeeds() {
        let agent = MockAgent::always(
            "m",
            Behaviour::Slow {
                delay: Duration::from_millis(10),
                output: "eventually".into(),
            },
        );
        assert!(run(&agent, &task()).await.unwrap().success);
    }

    #[tokio::test]
    async fn a_slow_agent_that_overruns_its_deadline_times_out() {
        let agent = MockAgent::always(
            "m",
            Behaviour::Slow {
                delay: Duration::from_secs(30),
                output: "never".into(),
            },
        );
        let mut request = request_for(&task());
        request.timeout_ms = 20;

        let (tx, _rx) = mpsc::channel(8);
        let err = agent.execute(request, tx).await.unwrap_err();
        assert_eq!(err.class(), ErrorClass::Timeout);
    }

    #[tokio::test]
    async fn a_partial_stream_delivers_its_chunks_before_dying() {
        let agent = MockAgent::always(
            "m",
            Behaviour::PartialStream {
                chunks: vec!["first".into(), "second".into()],
            },
        );

        let (tx, mut rx) = mpsc::channel(64);
        let t = task();
        let (result, chunks) = tokio::join!(agent.execute(request_for(&t), tx), async {
            let mut seen = Vec::new();
            while let Some(ExecutionUpdate::Text { content }) = rx.recv().await {
                seen.push(content);
            }
            seen
        });

        assert_eq!(chunks, vec!["first", "second"]);
        assert_eq!(result.unwrap_err().class(), ErrorClass::AgentCrash);
    }

    #[tokio::test]
    async fn a_script_lets_an_agent_fail_then_recover() {
        let agent = MockAgent::scripted(
            "flaky",
            vec![
                Behaviour::RateLimit {
                    retry_after_ms: None,
                },
                Behaviour::ok("succeeded on the retry"),
            ],
        );
        let t = task();

        assert!(run(&agent, &t).await.is_err());
        let recovered = run(&agent, &t).await.unwrap();
        assert!(recovered.success);
        assert_eq!(agent.call_count(), 2);
    }

    #[tokio::test]
    async fn a_script_that_runs_out_repeats_its_last_behaviour() {
        let agent = MockAgent::always(
            "broken",
            Behaviour::Crash {
                message: "boom".into(),
            },
        );
        let t = task();

        for _ in 0..5 {
            assert!(
                run(&agent, &t).await.is_err(),
                "an always-failing mock must not start succeeding"
            );
        }
        assert_eq!(agent.call_count(), 5);
    }

    #[tokio::test]
    async fn a_completed_but_failed_task_is_a_success_at_the_protocol_level() {
        let agent = MockAgent::always(
            "m",
            Behaviour::TaskFailure {
                reason: "the tests still fail".into(),
            },
        );
        // The agent responded correctly; the *work* failed. That distinction
        // matters: the transport is healthy, so the breaker must not trip.
        let outcome = run(&agent, &task()).await.unwrap();
        assert!(!outcome.success);
        assert_eq!(outcome.failure_class, Some(ErrorClass::TaskFailure));
    }

    #[tokio::test]
    async fn a_mock_is_usable_behind_the_adapter_trait() {
        let agent: Arc<dyn AgentAdapter> = Arc::new(MockAgent::always("m", Behaviour::ok("fine")));
        assert_eq!(agent.agent_id(), &AgentId::new("m"));
        assert!(agent.health_check().await.is_ok());
        assert!(agent.describe().await.is_ok());
    }
}
