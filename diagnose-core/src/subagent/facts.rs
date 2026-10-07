//! Bounded, normalized completion facts. Hosts validate their native ledgers.

use std::collections::BTreeSet;

use super::{
    state::{CompletionFacts, RequiredReceipt, TaskDependency},
    valid_id,
};
use crate::{
    action_result::{ActionResultOrigin, ActionResultReceipt},
    chat::ChatRole,
    session::PersistedAgentSession,
};
use desk_agent_protocol::{
    AgentOutcome, OperationOutput,
    computer_use::{ComputerActionCompleted, ComputerActionResultClass},
    data_lineage::DataEnvelope,
};
use serde_json::json;

pub const MAX_ACTION_FACTS: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct RuntimeFacts {
    pub completion: CompletionFacts,
    pub dependencies: Vec<TaskDependency>,
    pub result_envelopes: Vec<DataEnvelope>,
    native_result_call_ids: BTreeSet<String>,
}

impl CompletionFacts {
    pub fn validate(&self) -> Result<(), &'static str> {
        fn ids(values: &[String]) -> bool {
            values.len() <= MAX_ACTION_FACTS
                && values.iter().all(|id| valid_id(id))
                && values.iter().collect::<BTreeSet<_>>().len() == values.len()
        }
        if !ids(&self.accepted_receipt_ids)
            || !ids(&self.available_evidence_ids)
            || !ids(&self.incomplete_action_ids)
            || self.required_receipts.len() > MAX_ACTION_FACTS
            || self
                .required_receipts
                .iter()
                .any(|receipt| !self.accepted_receipt_ids.contains(&receipt.receipt_id))
            || self
                .required_receipts
                .iter()
                .map(|receipt| &receipt.receipt_id)
                .collect::<BTreeSet<_>>()
                .len()
                != self.required_receipts.len()
        {
            return Err("invalid durable task completion facts");
        }
        Ok(())
    }
}

/// A late fact still belongs to its original child after pause or adjustment.
/// It cannot be borrowed from a sibling or a later input/control/source epoch.
pub fn validate_origin(
    session: &PersistedAgentSession,
    origin: &ActionResultOrigin,
) -> Result<(), &'static str> {
    origin
        .validate()
        .map_err(|_| "invalid delegated action origin")?;
    let binding = session
        .agent_role
        .binding()
        .ok_or("action does not belong to a child")?;
    let fence = &origin.turn_fence;
    let delegation = fence
        .delegation
        .as_ref()
        .ok_or("action has no delegation origin")?;
    if fence.conversation_id != session.conversation_id
        || fence.actor_id != session.actor_id
        || fence.device_id != session.device_id
        || fence.input_revision > binding.input_revision
        || fence.control_revision > binding.control_revision
        || delegation.source_epoch > binding.source_epoch
        || delegation.root_conversation_id != binding.root_conversation_id
        || delegation.group_id != binding.group_id
        || delegation.task_id != binding.task_id
    {
        return Err("action origin belongs to another delegated task");
    }
    Ok(())
}

pub fn command_succeeded(outcome: &AgentOutcome) -> bool {
    matches!(outcome, AgentOutcome::Ok(OperationOutput::Exec(output)) if output.started
        && output.exit_code == Some(0) && output.termination_signal.is_none() && output.failure.is_none())
}

pub fn computer_verified(completion: &ComputerActionCompleted) -> bool {
    completion.result == ComputerActionResultClass::Verified
        && completion
            .facts
            .iter()
            .all(|fact| !fact.changed || fact.verified)
}

impl RuntimeFacts {
    pub fn dependency(&mut self, dependency: TaskDependency) -> Result<(), &'static str> {
        if !self.dependencies.contains(&dependency) {
            self.dependencies.push(dependency);
        }
        if self.dependencies.len() > 64 {
            return Err("too many delegated dependencies");
        }
        Ok(())
    }

    pub fn incomplete(&mut self, action_id: String) -> Result<(), &'static str> {
        if !valid_id(&action_id) {
            return Err("invalid incomplete action identity");
        }
        if !self.completion.incomplete_action_ids.contains(&action_id) {
            self.completion.incomplete_action_ids.push(action_id);
        }
        self.completion.validate()
    }

    /// Called only with a receipt verified against the immutable host result.
    /// An action result is evidence of its outcome, including failures; tool_ok
    /// and the model's description are deliberately absent from this input.
    pub fn receipt(
        &mut self,
        receipt: &ActionResultReceipt,
        required: bool,
        succeeded: bool,
        verification_complete: bool,
    ) -> Result<(), &'static str> {
        receipt
            .envelope
            .validate()
            .map_err(|_| "invalid action result label")?;
        let id = receipt.envelope.envelope_id.clone();
        if self.completion.accepted_receipt_ids.contains(&id) {
            return Err("duplicate physical action receipt");
        }
        self.completion.accepted_receipt_ids.push(id.clone());
        if required {
            self.completion.required_receipts.push(RequiredReceipt {
                receipt_id: id,
                succeeded,
                verification_complete,
            });
        }
        self.result_envelopes.push(receipt.envelope.clone());
        self.completion.validate()
    }

    /// Hosts supply the original call only after validating the immutable native
    /// result. A late receipt supersedes unavailable-result recovery metadata.
    pub fn receipt_for_call(
        &mut self,
        call_id: &str,
        receipt: &ActionResultReceipt,
        required: bool,
        succeeded: bool,
        verification_complete: bool,
    ) -> Result<(), &'static str> {
        if !valid_id(call_id) {
            return Err("invalid original receipt call identity");
        }
        self.receipt(receipt, required, succeeded, verification_complete)?;
        self.native_result_call_ids.insert(call_id.into());
        Ok(())
    }

    pub fn retained_evidence(
        &mut self,
        session: &PersistedAgentSession,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        for message in &session.conversation {
            if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
                continue;
            }
            if let Some(envelope) = &message.data_envelope
                && matches!(
                    envelope.provenance.source_tool_name.as_str(),
                    "recover_tool_call" | "child_mutation_result_unavailable"
                )
                && let Some(call_id) = &message.tool_call_id
                && !self.native_result_call_ids.contains(call_id)
            {
                let registry = crate::ai_assistant::ai_assistant_provider_registry();
                if session
                    .conversation
                    .iter()
                    .flat_map(|item| &item.tool_calls)
                    .filter(|call| &call.id == call_id)
                    .any(|call| {
                        registry
                            .capability_for_tool(&call.name)
                            .is_some_and(|capability| capability.wire.effect.is_side_effecting())
                    })
                {
                    self.incomplete(format!("unavailable-call:{call_id}"))?;
                }
            }
            let Some(envelope) = &message.data_envelope else {
                continue;
            };
            envelope
                .validate()
                .map_err(|_| "invalid retained evidence label")?;
            if crate::model_egress::envelope_expires_by(envelope, now_ms)
                || !valid_id(&envelope.envelope_id)
            {
                continue;
            }
            if !self
                .completion
                .available_evidence_ids
                .contains(&envelope.envelope_id)
            {
                self.completion
                    .available_evidence_ids
                    .push(envelope.envelope_id.clone());
            }
        }
        self.completion.validate()
    }

    pub fn projection(&self) -> serde_json::Value {
        json!({"accepted_receipt_ids": self.completion.accepted_receipt_ids,
            "available_evidence_ids": self.completion.available_evidence_ids,
            "incomplete_action_ids": self.completion.incomplete_action_ids,
            "required_receipts": self.completion.required_receipts.iter().map(|receipt| json!({
                "receipt_id": receipt.receipt_id, "succeeded": receipt.succeeded,
                "verification_complete": receipt.verification_complete })).collect::<Vec<_>>(),
            "dependencies": self.dependencies,
            "rule": "These are validated runtime facts, not additional authority. Cite exact identifiers. A returned action may have failed or remain unverified. A turn becoming idle does not complete this task."})
    }
}
