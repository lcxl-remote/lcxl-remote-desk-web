//! Immutable direct-parent references. Input lists are stored once per conversation
//! as shared items, never recursively copied into every later archive.

use super::super::{
    output_archive::ArchivedAssistantOutput,
    source::{derive_visible_text_policy, validate_policy},
};
use super::{NativeInputPolicyRegistration, SOURCE_CONTRACT_REVISION};
use crate::model_egress::{NativeModelInputSnapshot, sink_metadata_sha256, sink_metadata_size};
use desk_agent_protocol::{
    ai_assistant::source::{SourceUsePolicy, validate_source_id},
    data_lineage::{DataEnvelope, MAX_SOURCE_LINEAGE_BYTES},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDerivedInputPolicy {
    pub actor_id: String,
    pub conversation_id: String,
    pub physical_call_id: String,
    pub output_envelope_id: String,
    pub output_digest_sha256: String,
    pub output_label_sha256: String,
    pub actor_input_seq: u64,
    pub input_revision: u64,
    pub source_contract_revision: u64,
    pub policy_revision: u64,
    /// Historical policy summary only. Hosts verify it against the actual
    /// referenced immutable archive before using it for any new purpose.
    pub text_policy: Option<SourceUsePolicy>,
}

impl NativeDerivedInputPolicy {
    pub fn from_archive(parent: &ArchivedAssistantOutput) -> Result<Self, &'static str> {
        let evidence = &parent.sources.evidence;
        let value = Self {
            actor_id: parent.source_set.actor_id.clone(),
            conversation_id: evidence.context_conversation_id.clone(),
            physical_call_id: evidence.physical_call_id.clone(),
            output_envelope_id: parent.output.envelope_id.clone(),
            output_digest_sha256: parent
                .output
                .content_sha256()
                .unwrap_or_default()
                .to_owned(),
            output_label_sha256: evidence.output_label_sha256.clone(),
            actor_input_seq: parent.actor_input_seq,
            input_revision: parent.input_revision,
            source_contract_revision: parent.source_set.context.source_contract_revision,
            policy_revision: parent.source_set.context.policy_revision,
            text_policy: parent.retained_policy.clone(),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        validate_source_id(&self.actor_id)?;
        validate_source_id(&self.conversation_id)?;
        validate_source_id(&self.physical_call_id)?;
        validate_source_id(&self.output_envelope_id)?;
        if [&self.output_digest_sha256, &self.output_label_sha256]
            .iter()
            .any(|hash| hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()))
            || (self.actor_input_seq == 0 && !self.conversation_id.starts_with("schedule-run-"))
            || self.actor_input_seq > i64::MAX as u64
            || self.input_revision == 0
            || self.input_revision > i64::MAX as u64
            || self.source_contract_revision == 0
            || self.policy_revision == 0
        {
            return Err("invalid immutable native parent reference");
        }
        if let Some(policy) = &self.text_policy {
            validate_policy(policy)?;
            if policy.policy_revision != self.policy_revision
                || self.source_contract_revision != SOURCE_CONTRACT_REVISION
            {
                return Err("native parent policy changed its original contract");
            }
        }
        sink_metadata_sha256(self)?;
        Ok(())
    }

    pub fn validate_archive(&self, actual: &ArchivedAssistantOutput) -> Result<(), &'static str> {
        if self != &Self::from_archive(actual)? {
            return Err("native parent reference changed its immutable archive");
        }
        Ok(())
    }

    fn matches_label(&self, source: &DataEnvelope) -> Result<bool, &'static str> {
        Ok(is_native_output_policy_source(source)
            && self.output_envelope_id == source.envelope_id
            && source.content_sha256() == Some(self.output_digest_sha256.as_str())
            && self.output_label_sha256 == sink_metadata_sha256(source)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeInputPolicyGraph {
    pub actor_id: String,
    pub conversation_id: String,
    pub input_snapshot_sha256: String,
    /// Direct parents only. Native ancestors are followed through shared,
    /// immutable archive rows under current owner/source controls.
    pub parents: Vec<NativeDerivedInputPolicy>,
}

/// Collects direct parents with one size total, so the metadata bound stays
/// linear in the number of parents.
pub struct NativeInputPolicyGraphBuilder {
    graph: NativeInputPolicyGraph,
    size: usize,
    seen: BTreeMap<String, usize>,
}

impl NativeInputPolicyGraphBuilder {
    pub fn new(graph: NativeInputPolicyGraph) -> Result<Self, &'static str> {
        Ok(Self {
            size: sink_metadata_size(&graph)?,
            seen: graph
                .parents
                .iter()
                .enumerate()
                .map(|(index, parent)| (parent.output_envelope_id.clone(), index))
                .collect(),
            graph,
        })
    }

    /// The host authenticates this direct parent. Its ancestors are not
    /// cloned or inferred from this summary.
    pub fn include(&mut self, reference: NativeDerivedInputPolicy) -> Result<(), &'static str> {
        if reference.actor_id != self.graph.actor_id
            || reference.conversation_id != self.graph.conversation_id
        {
            return Err("native parent belongs to another source context");
        }
        if let Some(index) = self.seen.get(&reference.output_envelope_id) {
            if self.graph.parents[*index] != reference {
                return Err("conflicting native parent reference");
            }
            return Ok(());
        }
        self.size = self
            .size
            .checked_add(sink_metadata_size(&reference)?)
            .and_then(|size| size.checked_add(usize::from(!self.graph.parents.is_empty())))
            .filter(|size| *size <= MAX_SOURCE_LINEAGE_BYTES)
            .ok_or("native parent references exceed their metadata bound")?;
        self.seen.insert(
            reference.output_envelope_id.clone(),
            self.graph.parents.len(),
        );
        self.graph.parents.push(reference);
        Ok(())
    }

    pub fn finish(self) -> NativeInputPolicyGraph {
        self.graph
    }
}

impl NativeInputPolicyGraph {
    #[cfg(test)]
    pub fn from_archives(
        snapshot: &NativeModelInputSnapshot,
        registration: &NativeInputPolicyRegistration,
        actor: &str,
        conversation: &str,
        parents: &[&ArchivedAssistantOutput],
    ) -> Result<Self, &'static str> {
        let mut value =
            NativeInputPolicyGraphBuilder::new(Self::empty(snapshot, actor, conversation)?)?;
        for parent in parents {
            value.include(NativeDerivedInputPolicy::from_archive(parent)?)?;
        }
        let value = value.finish();
        value.validate(snapshot, registration)?;
        Ok(value)
    }

    pub fn empty(
        snapshot: &NativeModelInputSnapshot,
        actor: &str,
        conversation: &str,
    ) -> Result<Self, &'static str> {
        if [actor, conversation]
            .iter()
            .any(|id| id.trim().is_empty() || id.len() > 256)
        {
            return Err("invalid native policy subject");
        }
        Ok(Self {
            actor_id: actor.into(),
            conversation_id: conversation.into(),
            input_snapshot_sha256: snapshot.fingerprint()?,
            parents: Vec::new(),
        })
    }

    pub fn validate(
        &self,
        snapshot: &NativeModelInputSnapshot,
        registration: &NativeInputPolicyRegistration,
    ) -> Result<(), &'static str> {
        self.complete_text_policy(snapshot, registration)
            .map(|_| ())
    }

    /// Complete adjacency only. Hosts match every parent against its archive
    /// head and original receipt columns; full proofs are verified when read.
    pub fn validate_native_closure(
        &self,
        snapshot: &NativeModelInputSnapshot,
    ) -> Result<(), &'static str> {
        if self.input_snapshot_sha256 != snapshot.fingerprint()? {
            return Err("native adjacency changed its input snapshot");
        }
        let parents = self.parent_index()?;
        for source in snapshot.policy_sources() {
            if is_native_output_policy_source(source)
                && !parents
                    .get(source.envelope_id.as_str())
                    .map(|parent| parent.matches_label(source))
                    .transpose()?
                    .unwrap_or(false)
            {
                return Err("retained output omitted an actual direct native parent");
            }
        }
        Ok(())
    }

    /// This summary is historical policy evidence. It cannot authorize source
    /// bodies, ignore revocation, promote facts, or re-arm native I/O.
    pub fn complete_text_policy(
        &self,
        snapshot: &NativeModelInputSnapshot,
        registration: &NativeInputPolicyRegistration,
    ) -> Result<Option<SourceUsePolicy>, &'static str> {
        if [&self.actor_id, &self.conversation_id]
            .iter()
            .any(|id| id.trim().is_empty() || id.len() > 256)
            || self.input_snapshot_sha256 != snapshot.fingerprint()?
        {
            return Err("native policy changed its root snapshot");
        }
        sink_metadata_sha256(self)?;
        let mut complete = registration.source_contract_revision == SOURCE_CONTRACT_REVISION;
        if complete {
            registration.validate_registered_leaves(snapshot)?;
        } else {
            registration.validate_metadata(
                snapshot,
                registration.source_contract_revision,
                registration.policy_revision,
            )?;
        }
        let parents = self.parent_index()?;
        let mut used = BTreeSet::new();
        let mut policies = std::collections::BTreeSet::new();
        for (source, entry) in snapshot
            .policy_sources()
            .into_iter()
            .zip(&registration.leaves)
        {
            let parent = parents.get(source.envelope_id.as_str());
            if let Some(parent) = parent {
                if !parent.matches_label(source)? {
                    return Err("native parent changed the original full label");
                }
                used.insert(parent.output_envelope_id.as_str());
            }
            let policy = match entry {
                Some(entry) => registration.policy(entry),
                None => parent.and_then(|parent| parent.text_policy.as_ref()),
            };
            if let Some(policy) = policy {
                policies.insert(policy.clone());
            } else {
                complete = false;
            }
        }
        if used.len() != parents.len() {
            return Err("native policy contains unrelated parent references");
        }
        if !complete {
            return Ok(None);
        }
        derive_visible_text_policy(
            &policies.into_iter().collect::<Vec<_>>(),
            registration.policy_revision,
        )
        .map(Some)
    }

    fn parent_index(&self) -> Result<BTreeMap<&str, &NativeDerivedInputPolicy>, &'static str> {
        let mut parents = BTreeMap::new();
        let mut calls = BTreeSet::new();
        for parent in &self.parents {
            parent.validate()?;
            if parent.actor_id != self.actor_id
                || parent.conversation_id != self.conversation_id
                || !calls.insert(&parent.physical_call_id)
                || parents
                    .insert(parent.output_envelope_id.as_str(), parent)
                    .is_some()
            {
                return Err("duplicate immutable native parent identity");
            }
        }
        Ok(parents)
    }
}

/// Summary, child, review and assistant-data transformations have separate
/// native contracts and cannot be guessed to be ordinary model outputs.
pub fn is_native_output_policy_source(source: &DataEnvelope) -> bool {
    source.provenance.source_provider_id == "external-model"
        && source.provenance.source_tool_name == "model-response"
        && !source.provenance.source_envelope_ids.is_empty()
}
