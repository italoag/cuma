//! Explainable routing decisions.
//!
//! Every number that went into a choice survives into the output. This is not
//! a debugging nicety: routing weights are operator-tunable, and an operator
//! cannot tune weights they cannot see the effect of.

use cuma_core::{AgentId, ModelId};
use serde::{Deserialize, Serialize};

/// The five weighted dimensions, before and after weighting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    /// Capability coverage and model quality, in `[0.0, 1.0]`.
    pub quality: f64,
    /// Cost desirability: 1.0 is free, 0.0 is the most expensive candidate.
    pub cost: f64,
    /// Latency desirability: 1.0 is instant.
    pub latency: f64,
    /// Historical success and current health, in `[0.0, 1.0]`.
    pub reliability: f64,
    /// How comfortably the task fits the model's context window.
    pub context: f64,
    /// The weighted sum. This is what candidates are ranked by.
    pub total: f64,
}

impl ScoreBreakdown {
    /// Render as aligned lines, one dimension per line.
    ///
    /// Contributions, not raw scores, are shown alongside: a raw 0.9 on a
    /// dimension weighted at 0.05 is nearly irrelevant, and showing only the
    /// raw value hides that.
    pub fn render(&self, weights: &cuma_config::RouterWeights) -> String {
        let w = weights.normalized();
        let rows = [
            ("capability & quality", self.quality, w.quality),
            ("cost", self.cost, w.cost),
            ("latency", self.latency, w.latency),
            ("reliability", self.reliability, w.reliability),
            ("context fit", self.context, w.context),
        ];

        let mut out = String::new();
        for (name, raw, weight) in rows {
            out.push_str(&format!(
                "  {name:<22} {raw:>6.2}  x{weight:<5.2} = {:>6.3}\n",
                raw * weight
            ));
        }
        out.push_str(&format!("  {:<22} {:>21.3}\n", "total", self.total));
        out
    }
}

/// One scored agent+model pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// The agent.
    pub agent_id: AgentId,
    /// The model, when the agent exposes any.
    pub model_id: Option<ModelId>,
    /// Its score breakdown.
    pub breakdown: ScoreBreakdown,
    /// Notes worth surfacing (missing capabilities, unknown pricing).
    pub notes: Vec<String>,
}

impl Candidate {
    /// A one-line summary for the runners-up list.
    pub fn summary(&self) -> String {
        match &self.model_id {
            Some(model) => format!("{}/{}  {:.3}", self.agent_id, model, self.breakdown.total),
            None => format!("{}  {:.3}", self.agent_id, self.breakdown.total),
        }
    }
}

/// Why a candidate was removed before scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rejection {
    /// The agent that was rejected.
    pub agent_id: AgentId,
    /// The model, when the rejection was model-specific.
    pub model_id: Option<ModelId>,
    /// Why.
    pub reason: String,
}

/// The complete result of a routing decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    /// The winner.
    pub selected: Candidate,
    /// Runners-up, best first, capped so an explanation stays readable.
    pub alternatives: Vec<Candidate>,
    /// Candidates removed by a hard constraint, and why.
    pub rejected: Vec<Rejection>,
    /// The weights in force.
    pub weights: cuma_config::RouterWeights,
    /// The strategy in force.
    pub strategy: cuma_config::RoutingStrategy,
}

impl RoutingDecision {
    /// Whether any other candidate could have taken this task.
    ///
    /// The retry policy needs this: proposing a reroute with nowhere to go
    /// wastes an attempt.
    pub fn has_alternatives(&self) -> bool {
        !self.alternatives.is_empty()
    }

    /// Render the full explanation.
    pub fn explain(&self) -> String {
        let mut out = String::new();

        out.push_str("Selected:\n");
        out.push_str(&format!("  Agent: {}\n", self.selected.agent_id));
        if let Some(model) = &self.selected.model_id {
            out.push_str(&format!("  Model: {model}\n"));
        }

        out.push_str(&format!("Strategy: {:?}\n", self.strategy));
        out.push_str("Reasons:\n");
        out.push_str(&self.selected.breakdown.render(&self.weights));

        if !self.selected.notes.is_empty() {
            out.push_str("Notes:\n");
            for note in &self.selected.notes {
                out.push_str(&format!("  - {note}\n"));
            }
        }

        if !self.alternatives.is_empty() {
            out.push_str("Alternatives:\n");
            for alt in &self.alternatives {
                out.push_str(&format!("  {}\n", alt.summary()));
            }
        }

        if !self.rejected.is_empty() {
            out.push_str("Rejected:\n");
            for rejection in &self.rejected {
                let target = match &rejection.model_id {
                    Some(m) => format!("{}/{}", rejection.agent_id, m),
                    None => rejection.agent_id.to_string(),
                };
                out.push_str(&format!("  {target}: {}\n", rejection.reason));
            }
        }

        out
    }

    /// A compact one-line form, for logs and events.
    pub fn summary(&self) -> String {
        format!(
            "{} (score {:.3}, {} alternatives, {} rejected)",
            self.selected.summary(),
            self.selected.breakdown.total,
            self.alternatives.len(),
            self.rejected.len()
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_config::{RouterWeights, RoutingStrategy};

    fn candidate(agent: &str, total: f64) -> Candidate {
        Candidate {
            agent_id: AgentId::new(agent),
            model_id: Some(ModelId::new("m")),
            breakdown: ScoreBreakdown {
                quality: 0.9,
                cost: 0.5,
                latency: 0.7,
                reliability: 0.95,
                context: 1.0,
                total,
            },
            notes: vec![],
        }
    }

    fn decision() -> RoutingDecision {
        RoutingDecision {
            selected: candidate("claude", 0.87),
            alternatives: vec![candidate("codex", 0.82), candidate("gemini", 0.71)],
            rejected: vec![Rejection {
                agent_id: AgentId::new("broken"),
                model_id: None,
                reason: "circuit breaker open".into(),
            }],
            weights: RouterWeights::default(),
            strategy: RoutingStrategy::Balanced,
        }
    }

    #[test]
    fn an_explanation_names_the_winner_and_every_dimension() {
        let text = decision().explain();
        assert!(text.contains("Agent: claude"));
        assert!(text.contains("capability & quality"));
        assert!(text.contains("cost"));
        assert!(text.contains("latency"));
        assert!(text.contains("reliability"));
        assert!(text.contains("context fit"));
        assert!(text.contains("total"));
    }

    #[test]
    fn an_explanation_lists_the_runners_up_with_their_scores() {
        let text = decision().explain();
        assert!(text.contains("Alternatives:"));
        assert!(text.contains("codex"));
        assert!(text.contains("0.820"));
    }

    #[test]
    fn an_explanation_says_why_a_candidate_was_excluded() {
        let text = decision().explain();
        assert!(text.contains("Rejected:"));
        assert!(text.contains("broken: circuit breaker open"));
    }

    #[test]
    fn the_breakdown_shows_weighted_contributions_not_just_raw_scores() {
        let weights = RouterWeights {
            quality: 0.5,
            cost: 0.5,
            latency: 0.0,
            reliability: 0.0,
            context: 0.0,
        };
        let breakdown = ScoreBreakdown {
            quality: 1.0,
            cost: 0.0,
            latency: 0.0,
            reliability: 0.0,
            context: 0.0,
            total: 0.5,
        };

        let text = breakdown.render(&weights);
        assert!(text.contains("x0.50"), "the weight itself must be visible");
        assert!(text.contains("0.500"), "and so must the contribution");
    }

    #[test]
    fn a_decision_with_no_runners_up_reports_no_alternatives() {
        let mut d = decision();
        d.alternatives.clear();
        assert!(!d.has_alternatives());
        assert!(!d.explain().contains("Alternatives:"));
    }
}
