//! A reverse index from capability to the agents that provide it.
//!
//! Scoring every registered agent for every task is fine at ten agents and
//! wasteful at a few hundred. This index narrows the candidate set before
//! scoring begins, without changing which candidate wins.

use cuma_core::{AgentId, Capability, CapabilitySet};
use std::collections::{BTreeMap, BTreeSet};

use crate::RegistrySnapshot;

/// Capability to providing agents.
#[derive(Debug, Clone, Default)]
pub struct CapabilityIndex {
    by_capability: BTreeMap<Capability, BTreeSet<AgentId>>,
}

impl CapabilityIndex {
    /// Build the index from a registry snapshot.
    ///
    /// Only routable agents are indexed: an unhealthy agent is not a candidate,
    /// and indexing it would just force the router to filter it out again.
    pub fn build(snapshot: &RegistrySnapshot) -> Self {
        let mut by_capability: BTreeMap<Capability, BTreeSet<AgentId>> = BTreeMap::new();

        for agent in snapshot.routable() {
            let mut caps = agent.capabilities.clone();
            for model in &agent.models {
                for capability in model.capabilities.iter() {
                    caps.insert(capability.clone());
                }
            }

            for capability in caps.iter() {
                by_capability
                    .entry(capability.clone())
                    .or_default()
                    .insert(agent.id.clone());
            }
        }

        Self { by_capability }
    }

    /// Agents advertising `capability`.
    pub fn providers(&self, capability: &Capability) -> Vec<AgentId> {
        self.by_capability
            .get(capability)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Agents advertising *every* capability in `required`.
    ///
    /// An empty requirement matches nothing here by design: the caller falls
    /// back to the full routable set, which is the correct behaviour for a
    /// task that requires nothing in particular.
    pub fn providers_of_all(&self, required: &CapabilitySet) -> Vec<AgentId> {
        let mut iter = required.iter();
        let Some(first) = iter.next() else {
            return Vec::new();
        };

        let mut candidates: BTreeSet<AgentId> =
            self.by_capability.get(first).cloned().unwrap_or_default();

        for capability in iter {
            let Some(providers) = self.by_capability.get(capability) else {
                return Vec::new();
            };
            candidates.retain(|id| providers.contains(id));
            if candidates.is_empty() {
                break;
            }
        }

        candidates.into_iter().collect()
    }

    /// Capabilities in `required` that no agent provides.
    ///
    /// This is what tells the skill manager a capability gap exists and drives
    /// the search-install-execute flow.
    pub fn missing_from(&self, required: &CapabilitySet) -> Vec<Capability> {
        required
            .iter()
            .filter(|c| !self.by_capability.contains_key(c))
            .cloned()
            .collect()
    }

    /// Every capability any agent provides.
    pub fn all_capabilities(&self) -> Vec<Capability> {
        self.by_capability.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{AgentDescriptor, AgentProtocol, HealthState};

    fn snapshot() -> RegistrySnapshot {
        let coder = AgentDescriptor::new("coder", "coder", AgentProtocol::Acp).with_capabilities(
            CapabilitySet::new()
                .with(Capability::CodeEditing)
                .with(Capability::Debugging),
        );
        let researcher = AgentDescriptor::new("researcher", "researcher", AgentProtocol::A2A)
            .with_capabilities(
                CapabilitySet::new()
                    .with(Capability::Research)
                    .with(Capability::Debugging),
            );

        RegistrySnapshot::new(vec![coder, researcher])
    }

    #[test]
    fn a_capability_maps_to_every_agent_that_provides_it() {
        let index = CapabilityIndex::build(&snapshot());
        assert_eq!(index.providers(&Capability::Debugging).len(), 2);
        assert_eq!(index.providers(&Capability::Research), vec![AgentId::new("researcher")]);
    }

    #[test]
    fn requiring_several_capabilities_intersects_the_providers() {
        let index = CapabilityIndex::build(&snapshot());
        let required = CapabilitySet::new()
            .with(Capability::CodeEditing)
            .with(Capability::Debugging);

        assert_eq!(index.providers_of_all(&required), vec![AgentId::new("coder")]);
    }

    #[test]
    fn an_unsatisfiable_combination_yields_no_candidates() {
        let index = CapabilityIndex::build(&snapshot());
        let required = CapabilitySet::new()
            .with(Capability::CodeEditing)
            .with(Capability::Research);

        assert!(index.providers_of_all(&required).is_empty());
    }

    #[test]
    fn a_capability_no_one_has_is_reported_as_a_gap() {
        let index = CapabilityIndex::build(&snapshot());
        let required = CapabilitySet::new()
            .with(Capability::Debugging)
            .with(Capability::Vision);

        assert_eq!(index.missing_from(&required), vec![Capability::Vision]);
    }

    #[test]
    fn unhealthy_agents_are_not_indexed() {
        let mut snapshot = snapshot();
        let mut agents = snapshot.all().to_vec();
        agents[0].health.state = HealthState::Unavailable;
        snapshot = RegistrySnapshot::new(agents);

        let index = CapabilityIndex::build(&snapshot);
        assert!(index.providers(&Capability::CodeEditing).is_empty());
        assert_eq!(index.providers(&Capability::Debugging).len(), 1);
    }

    #[test]
    fn an_unknown_capability_from_discovery_is_indexed_like_any_other() {
        let agent = AgentDescriptor::new("exotic", "exotic", AgentProtocol::A2A)
            .with_capabilities(
                CapabilitySet::new().with(Capability::parse("formal-verification")),
            );
        let index = CapabilityIndex::build(&RegistrySnapshot::new(vec![agent]));

        assert_eq!(
            index.providers(&Capability::Custom("formal_verification".into())).len(),
            1
        );
    }
}
