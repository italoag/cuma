//! The internal event bus.
//!
//! Everything that happens in the harness is announced here. The TUI, the
//! logger, the persistence layer and the usage tracker are all *subscribers* —
//! none of them is called directly by the orchestrator. That is what keeps the
//! TUI from being wired into the runtime, and what lets a future web or IDE
//! front end attach without touching the core.

use crate::ids::{AgentId, AttemptId, ModelId, SessionId, SkillId, TaskId};
use crate::task::{TaskStatus, TokenUsage};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// What happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum EventKind {
    /// A user goal was accepted.
    SessionStarted {
        /// The user's original intent.
        goal: String,
    },
    /// The session finished.
    SessionCompleted {
        /// Whether every task succeeded.
        success: bool,
    },

    /// The planner produced a plan.
    TaskPlanned {
        /// How many tasks it contains.
        task_count: usize,
    },
    /// A task entered the graph.
    TaskCreated {
        /// What the task is.
        description: String,
    },
    /// A task changed state.
    TaskStatusChanged {
        /// The new status.
        status: TaskStatus,
    },
    /// A task finished successfully.
    TaskCompleted {
        /// Tokens it consumed across all attempts.
        tokens: TokenUsage,
    },
    /// A task exhausted its retries and fallbacks.
    TaskFailed {
        /// Why.
        reason: String,
    },
    /// A task was skipped because a dependency failed.
    TaskSkipped {
        /// The dependency that failed.
        blocked_by: TaskId,
    },

    /// The router picked a target.
    ///
    /// `explanation` is the rendered scoring breakdown — every selection must
    /// be explainable after the fact, so the reasoning travels with the event.
    AgentSelected {
        /// Chosen agent.
        agent: AgentId,
        /// Chosen model, when the agent exposes any.
        model: Option<ModelId>,
        /// Winning score.
        score: f64,
        /// Human-readable breakdown.
        explanation: String,
    },
    /// The router could not pick anything.
    RoutingFailed {
        /// Why every candidate was rejected.
        reason: String,
    },
    /// Execution began against an agent.
    AgentStarted {
        /// The agent.
        agent: AgentId,
    },
    /// A chunk of streamed output arrived.
    AgentOutputReceived {
        /// The chunk.
        chunk: String,
    },
    /// An attempt failed.
    AgentFailed {
        /// The agent.
        agent: AgentId,
        /// Classification driving the reaction.
        class: crate::error::ErrorClass,
        /// What went wrong.
        message: String,
    },

    /// A retry was scheduled against the same target.
    RetryScheduled {
        /// Which attempt number is next.
        attempt: u32,
        /// How long we will wait.
        delay_ms: u64,
        /// Why we are retrying.
        reason: String,
    },
    /// The router chose a different target after a failure.
    FallbackSelected {
        /// The target that failed.
        from: AgentId,
        /// The replacement.
        to: AgentId,
        /// Why.
        reason: String,
    },
    /// A circuit breaker changed state.
    CircuitBreakerChanged {
        /// The agent whose breaker moved.
        agent: AgentId,
        /// The new state.
        state: String,
    },
    /// Context was handed from one agent to another.
    HandoffPerformed {
        /// Outgoing agent.
        from: AgentId,
        /// Incoming agent.
        to: AgentId,
    },

    /// A skill was installed.
    SkillInstalled {
        /// The skill.
        skill: SkillId,
        /// Its trust level at install time.
        trust: String,
    },
    /// A skill install was refused by policy.
    SkillRejected {
        /// The skill.
        skill: SkillId,
        /// Why.
        reason: String,
    },

    /// Usage was recorded for an attempt.
    UsageRecorded {
        /// Tokens.
        tokens: TokenUsage,
        /// Estimated USD, when pricing is known.
        estimated_cost_usd: Option<f64>,
    },
}

/// An event plus the identifiers needed to correlate it.
///
/// Every field beyond `kind` exists so a log line can be joined back to a
/// session, a task and an attempt without parsing prose.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// When it happened.
    pub at: chrono::DateTime<chrono::Utc>,
    /// The session this belongs to.
    pub session_id: SessionId,
    /// The task, when the event is task-scoped.
    pub task_id: Option<TaskId>,
    /// The attempt, when the event is attempt-scoped.
    pub attempt_id: Option<AttemptId>,
    /// What happened.
    pub kind: EventKind,
}

impl Event {
    /// A session-scoped event.
    pub fn session(session_id: SessionId, kind: EventKind) -> Self {
        Self {
            at: chrono::Utc::now(),
            session_id,
            task_id: None,
            attempt_id: None,
            kind,
        }
    }

    /// A task-scoped event.
    pub fn task(session_id: SessionId, task_id: TaskId, kind: EventKind) -> Self {
        Self {
            at: chrono::Utc::now(),
            session_id,
            task_id: Some(task_id),
            attempt_id: None,
            kind,
        }
    }

    /// Attach an attempt id.
    #[must_use]
    pub fn with_attempt(mut self, attempt_id: AttemptId) -> Self {
        self.attempt_id = Some(attempt_id);
        self
    }
}

/// A broadcast bus.
///
/// Deliberately lossy: a slow subscriber (a TUI redrawing, say) must never
/// stall the orchestrator. Subscribers that lag past the buffer are told they
/// lagged via [`broadcast::error::RecvError::Lagged`] rather than silently
/// missing events.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

/// A handle for receiving events.
pub type EventSubscriber = broadcast::Receiver<Event>;

impl EventBus {
    /// A bus buffering up to `capacity` events per subscriber.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    /// Publish an event.
    ///
    /// Publishing with no subscribers is not an error — the harness runs fine
    /// headless with nothing attached.
    pub fn publish(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    /// Subscribe. Only events published after this call are delivered.
    pub fn subscribe(&self) -> EventSubscriber {
        self.sender.subscribe()
    }

    /// How many subscribers are currently attached.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[tokio::test]
    async fn subscribers_receive_published_events() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        let session = SessionId::generate();
        bus.publish(Event::session(
            session.clone(),
            EventKind::SessionStarted {
                goal: "do the thing".into(),
            },
        ));

        let received = rx.recv().await.unwrap();
        assert_eq!(received.session_id, session);
        assert!(matches!(received.kind, EventKind::SessionStarted { .. }));
    }

    #[tokio::test]
    async fn publishing_without_subscribers_is_not_an_error() {
        let bus = EventBus::new(4);
        assert_eq!(bus.subscriber_count(), 0);
        bus.publish(Event::session(
            SessionId::generate(),
            EventKind::SessionCompleted { success: true },
        ));
    }

    #[tokio::test]
    async fn a_lagging_subscriber_is_told_it_lagged_rather_than_silently_losing_events() {
        let bus = EventBus::new(2);
        let mut rx = bus.subscribe();

        for i in 0..10 {
            bus.publish(Event::session(
                SessionId::generate(),
                EventKind::AgentOutputReceived {
                    chunk: i.to_string(),
                },
            ));
        }

        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
    }

    #[tokio::test]
    async fn every_subscriber_sees_every_event() {
        let bus = EventBus::new(16);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);

        bus.publish(Event::session(
            SessionId::generate(),
            EventKind::SessionCompleted { success: true },
        ));

        assert!(a.recv().await.is_ok());
        assert!(b.recv().await.is_ok());
    }
}
