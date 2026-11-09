//! Independent provenance metadata for accepted main-assistant model outputs.
//! This contains no answer/tool body and grants no source access or native I/O.

use super::native_export::{AssistantDataExportAuthorization, AssistantNativeSourceSet};
use crate::model_egress::{NativeModelOutputSources, sink_metadata_sha256};
use desk_agent_protocol::data_lineage::DataEnvelope;
use serde::{Deserialize, Serialize};

mod visible;
pub use visible::VisibleAssistantMessage;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivedAssistantOutput {
    pub source_set: AssistantNativeSourceSet,
    pub input_revision: u64,
    pub actor_input_seq: u64,
    pub input_recorded_at_unix_ms: u64,
    pub statement_timezone: String,
    pub sources: NativeModelOutputSources,
    pub input_policies: super::source_registry::NativeInputPolicyRegistration,
    pub input_policy_graph: super::source_registry::NativeInputPolicyGraph,
    pub output: DataEnvelope,
    pub retained_policy: Option<desk_agent_protocol::ai_assistant::source::SourceUsePolicy>,
    pub retained_roots: Vec<super::seam::AssistantReadRoot>,
}

impl ArchivedAssistantOutput {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.source_set.validate()?;
        if let Some(policy) = &self.retained_policy {
            super::source::validate_policy(policy)?;
        }
        self.sources.validate(&self.output)?;
        if self.input_policies.source_contract_revision
            != self.source_set.context.source_contract_revision
            || self.input_policies.policy_revision != self.source_set.context.policy_revision
        {
            return Err("archived native policy changed its original contract");
        }
        if self.input_policy_graph.actor_id != self.source_set.actor_id
            || self.input_policy_graph.conversation_id != self.source_set.consumer_conversation_id
        {
            return Err("archived native policy belongs to another source context");
        }
        self.input_policy_graph
            .validate(&self.sources.input_snapshot, &self.input_policies)?;
        if self.input_policy_graph.parents.iter().any(|parent| {
            parent.physical_call_id == self.source_set.physical_call_id
                || parent.output_envelope_id == self.output.envelope_id
                || parent.actor_input_seq > self.actor_input_seq
        }) {
            return Err("archived native output has an invalid causal parent");
        }
        self.source_set
            .validate_input_lineage(&self.sources.inputs)?;
        let evidence = &self.sources.evidence;
        if evidence.physical_call_id != self.source_set.physical_call_id
            || evidence.context_conversation_id != self.source_set.consumer_conversation_id
            || evidence.model_export_authorization_id
                != self.source_set.model_export_authorization_id
            || evidence.destination != self.source_set.destination
            || (self.actor_input_seq == 0
                && self.source_set.context.task_id.as_deref()
                    != Some(self.source_set.consumer_conversation_id.as_str()))
            || self.actor_input_seq > i64::MAX as u64
            || self.input_revision == 0
            || self.input_revision > i64::MAX as u64
            || self.input_recorded_at_unix_ms == 0
            || self.input_recorded_at_unix_ms > i64::MAX as u64
            || self.input_revision != self.source_set.context.input_revision
            || self.actor_input_seq != self.source_set.context.actor_input_seq
            || self.input_recorded_at_unix_ms != self.source_set.context.input_recorded_at_unix_ms
            || self.statement_timezone != self.source_set.context.statement_timezone
            || evidence.provider_started_at_unix_ms != self.source_set.context.authorized_at_unix_ms
            || !super::source::valid_statement_timezone(&self.statement_timezone)
        {
            return Err("invalid archived assistant output bindings");
        }
        sink_metadata_sha256(self)?;
        Ok(())
    }

    /// Bind grants to an archive already checked at its native/storage boundary.
    pub fn validate_grants(
        &self,
        grants: &[AssistantDataExportAuthorization],
    ) -> Result<(), &'static str> {
        self.source_set.validate_grants(grants)?;
        if grants.iter().any(|grant| {
            grant.input_revision != self.input_revision
                || grant.actor_input_seq != self.actor_input_seq
        }) {
            return Err("archived assistant output changed its original owner input");
        }
        Ok(())
    }

    /// Hash an already validated archive without repeating its structural checks.
    pub fn fingerprint(&self) -> Result<String, &'static str> {
        sink_metadata_sha256(self)
    }

    pub fn decode(raw: &str) -> Result<Self, &'static str> {
        let value: Self =
            serde_json::from_str(raw).map_err(|_| "invalid archived assistant output encoding")?;
        value.validate()?;
        Ok(value)
    }
}
