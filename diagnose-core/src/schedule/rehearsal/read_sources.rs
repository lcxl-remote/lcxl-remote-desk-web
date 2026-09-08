//! Project verified historical read receipts into source-graph evidence.
use crate::{
    chat::{ChatMessage, ChatRole, ToolCall},
    model_egress::{ModelInputLineage, message_payload_bytes},
    schedule::{
        rehearsal::ObservedRehearsalRead,
        source_graph::{TaskSourceAuthority, TaskSourceBinding},
    },
    seam::ToolRunOutput,
};

/// Selected by the original receipt producer, never inferred from a hash match.
#[derive(Debug, Clone, Copy)]
pub enum ReadSourceDigest {
    BoundToolOutput,
    ModelPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidReadSource {
    Conflict,
    Invalid,
}

pub struct RehearsalToolSource {
    pub lineage: ModelInputLineage,
    pub authority: TaskSourceBinding,
}

/// A status is a deterministic projection of the original proposal and execution
/// identity. The caller has already verified the successful original action.
pub fn verified_action_status_sources(
    messages: &[ChatMessage],
    authority: &crate::provider_preflight::ObservedCapabilityAuthority,
    origin: &crate::action_result::ActionResultOrigin,
    receipt: &crate::action_result::ActionResultReceipt,
) -> Result<Vec<RehearsalToolSource>, InvalidReadSource> {
    let proposals: Vec<_> = messages
        .iter()
        .filter(|message| {
            message.role == ChatRole::Assistant
                && message.turn_id.as_deref() == Some(origin.turn_fence.turn_id.as_str())
                && message
                    .tool_calls
                    .iter()
                    .any(|call| call.id == origin.tool_call_id && call.name == origin.tool_name)
        })
        .collect();
    if proposals.len() != 1 || authority.resources.is_empty() {
        return Err(InvalidReadSource::Conflict);
    }
    let parent = proposals[0]
        .data_envelope
        .as_ref()
        .ok_or(InvalidReadSource::Conflict)?;
    let statuses: Vec<_> = messages
        .iter()
        .filter(|message| {
            message.role == ChatRole::Tool
                && message.tool_call_id.as_deref() == Some(origin.tool_call_id.as_str())
                && message.data_envelope.as_ref() != Some(&receipt.envelope)
        })
        .collect();
    if statuses.len() > 1 {
        return Err(InvalidReadSource::Conflict);
    }
    statuses
        .into_iter()
        .map(|message| {
            if message.background_task_id.as_deref()
                != Some(receipt.action.action_request_id.as_str())
                || message.text
                    != crate::chat::background_task_running_result(
                        &receipt.action.action_request_id,
                    )
                || message.image_data_url.is_some()
                || !message.tool_calls.is_empty()
            {
                return Err(InvalidReadSource::Conflict);
            }
            let expected = crate::model_message_labels::internal_tool_result_envelope(
                Some(parent),
                &origin.tool_call_id,
                &message.text,
                "provider_execution_status",
            )
            .map_err(|_| InvalidReadSource::Invalid)?
            .ok_or(InvalidReadSource::Conflict)?;
            if message.data_envelope.as_ref() != Some(&expected) {
                return Err(InvalidReadSource::Conflict);
            }
            Ok(envelope_source(&expected, &authority.resources))
        })
        .collect()
}

/// The store verifies success and the original grant before projecting a native
/// result. The complete receipt label binds parents as well as output bytes.
pub fn verified_action_source(
    messages: &[ChatMessage],
    authority: &crate::provider_preflight::ObservedCapabilityAuthority,
    origin: &crate::action_result::ActionResultOrigin,
    receipt: &crate::action_result::ActionResultReceipt,
) -> Result<RehearsalToolSource, InvalidReadSource> {
    let results: Vec<_> = messages
        .iter()
        .filter(|message| {
            message
                .data_envelope
                .as_ref()
                .is_some_and(|envelope| envelope.envelope_id == receipt.envelope.envelope_id)
        })
        .collect();
    if results.len() != 1 {
        return Err(InvalidReadSource::Conflict);
    }
    let result = results[0];
    if result.data_envelope.as_ref() != Some(&receipt.envelope)
        || result.tool_call_id.as_deref() != Some(origin.tool_call_id.as_str())
        || !matches!(result.role, ChatRole::Tool | ChatRole::UntrustedOutput)
        || result
            .background_task_id
            .as_ref()
            .is_some_and(|id| id != &receipt.action.action_request_id)
        || (result.role == ChatRole::UntrustedOutput && result.background_task_id.is_none())
    {
        return Err(InvalidReadSource::Conflict);
    }
    let output = ToolRunOutput {
        content: results[0].text.clone(),
        image_data_url: results[0].image_data_url.clone(),
    };
    receipt
        .validate_for(origin, receipt.action.clone(), receipt.attempt, &output)
        .map_err(|_| InvalidReadSource::Conflict)?;
    project_tool_result_source(
        messages,
        authority,
        &origin.tool_call_id,
        result,
        &receipt.envelope.digest_sha256,
        ReadSourceDigest::ModelPayload,
    )
}

/// The caller must verify the original receipt, owner-bound frozen session and
/// authority before calling this; matching content alone establishes no grant.
pub fn verified_read_source(
    messages: &[ChatMessage],
    read: &ObservedRehearsalRead,
    digest_kind: ReadSourceDigest,
) -> Result<RehearsalToolSource, InvalidReadSource> {
    verified_tool_source(
        messages,
        &read.authority,
        &read.tool_call_id,
        &read.output_sha256,
        digest_kind,
    )
}

fn verified_tool_source(
    messages: &[ChatMessage],
    authority: &crate::provider_preflight::ObservedCapabilityAuthority,
    tool_call_id: &str,
    output_sha256: &str,
    digest_kind: ReadSourceDigest,
) -> Result<RehearsalToolSource, InvalidReadSource> {
    let results: Vec<_> = messages
        .iter()
        .filter(|message| {
            message.role == ChatRole::Tool && message.tool_call_id.as_deref() == Some(tool_call_id)
        })
        .collect();
    if results.len() != 1 || results[0].background_task_id.is_some() {
        return Err(InvalidReadSource::Conflict);
    }
    project_tool_result_source(
        messages,
        authority,
        tool_call_id,
        results[0],
        output_sha256,
        digest_kind,
    )
}

fn project_tool_result_source(
    messages: &[ChatMessage],
    authority: &crate::provider_preflight::ObservedCapabilityAuthority,
    tool_call_id: &str,
    result: &ChatMessage,
    output_sha256: &str,
    digest_kind: ReadSourceDigest,
) -> Result<RehearsalToolSource, InvalidReadSource> {
    let proposals: Vec<_> = messages
        .iter()
        .filter(|message| message.role == ChatRole::Assistant)
        .flat_map(|message| &message.tool_calls)
        .filter(|call| call.id == tool_call_id)
        .collect();
    if proposals.len() != 1 || authority.resources.is_empty() {
        return Err(InvalidReadSource::Conflict);
    }
    let call = ToolCall {
        id: proposals[0].id.clone(),
        name: proposals[0].name.clone(),
        arguments_json: proposals[0].arguments_json.clone(),
    };
    let output = ToolRunOutput {
        content: result.text.clone(),
        image_data_url: result.image_data_url.clone(),
    };
    let envelope = result
        .data_envelope
        .as_ref()
        .ok_or(InvalidReadSource::Conflict)?;
    envelope
        .validate()
        .map_err(|_| InvalidReadSource::Invalid)?;
    let bytes = message_payload_bytes(&result.text, result.image_data_url.as_deref())
        .map_err(|_| InvalidReadSource::Invalid)?;
    use sha2::{Digest, Sha256};
    let payload_digest = format!("{:x}", Sha256::digest(&bytes));
    let receipt_digest = match digest_kind {
        ReadSourceDigest::BoundToolOutput => {
            crate::provider_preflight::read::output_digest(&call, &output)
        }
        ReadSourceDigest::ModelPayload => payload_digest.clone(),
    };
    if !result.tool_calls.is_empty()
        || call.name != authority.tool_name
        || envelope.provenance.source_tool_name != call.name
        || envelope.provenance.source_provider_id != authority.provider_id
        || receipt_digest != output_sha256
        || payload_digest != envelope.digest_sha256
    {
        return Err(InvalidReadSource::Conflict);
    }
    Ok(envelope_source(envelope, &authority.resources))
}

fn envelope_source(
    envelope: &desk_agent_protocol::data_lineage::DataEnvelope,
    resources: &[String],
) -> RehearsalToolSource {
    RehearsalToolSource {
        lineage: ModelInputLineage {
            public_system_prompt: false,
            envelope_id: envelope.envelope_id.clone(),
            digest_sha256: envelope.digest_sha256.clone(),
            source_provider_id: envelope.provenance.source_provider_id.clone(),
            source_tool_name: envelope.provenance.source_tool_name.clone(),
            source_envelope_ids: envelope.provenance.source_envelope_ids.clone(),
        },
        authority: TaskSourceBinding {
            envelope_id: envelope.envelope_id.clone(),
            digest_sha256: envelope.digest_sha256.clone(),
            authority: TaskSourceAuthority::Scopes(resources.to_vec()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model_message_labels::{ReadResultLabel, read_result_envelope},
        provider_preflight::ObservedCapabilityAuthority,
    };
    use desk_agent_protocol::{
        capability_grant::{CapabilityGrantIssuer, CapabilityRiskTier},
        capability_provider::CapabilityEffect,
    };

    #[test]
    fn source_requires_original_receipt_encoding_and_unmodified_result() {
        let call = ToolCall {
            id: "call".into(),
            name: "read_system_info".into(),
            arguments_json: "{}".into(),
        };
        let output = ToolRunOutput {
            content: "original result".into(),
            image_data_url: None,
        };
        let registry = crate::device_assistant::device_assistant_provider_registry();
        let mut envelope = read_result_envelope(
            &registry,
            &call,
            &output,
            ReadResultLabel {
                envelope_id: "read".into(),
                observation_id: "observation".into(),
                source_object_id: None,
                observed_at_unix_ms: 100,
            },
        )
        .unwrap();
        envelope.provenance.source_envelope_ids = vec!["original-question".into()];
        let mut result = ChatMessage::tool_result("result", &call.id, &output.content);
        result.data_envelope = Some(envelope.clone());
        let messages = vec![
            ChatMessage::assistant_tool_calls("proposal", "", vec![call.to_ref()]),
            result,
        ];
        let mut read = ObservedRehearsalRead {
            call_id: "server-call".into(),
            tool_call_id: call.id.clone(),
            grant_id: "grant".into(),
            issued_by: CapabilityGrantIssuer::UserDecision,
            completed_at: 100,
            output_sha256: crate::provider_preflight::read::output_digest(&call, &output),
            authority: ObservedCapabilityAuthority {
                target_session_id: None,
                envelope_ids: vec![],
                content_digests_sha256: vec![],
                provider_id: envelope.provenance.source_provider_id.clone(),
                capability_id: "system-info".into(),
                tool_name: call.name.clone(),
                tool_schema_version: 1,
                effect: CapabilityEffect::ReadDevice,
                risk_tier: CapabilityRiskTier::R0,
                canonical_input_sha256: "a".repeat(64),
                resources: vec!["device:1".into()],
                operations: vec!["read".into()],
                export_destinations: vec![],
            },
        };
        let proof =
            verified_read_source(&messages, &read, ReadSourceDigest::BoundToolOutput).unwrap();
        assert_eq!(proof.lineage.source_envelope_ids, ["original-question"]);
        assert_eq!(
            proof.authority.authority,
            TaskSourceAuthority::Scopes(vec!["device:1".into()])
        );
        assert!(verified_read_source(&messages, &read, ReadSourceDigest::ModelPayload).is_err());
        read.output_sha256 = envelope.digest_sha256.clone();
        assert!(verified_read_source(&messages, &read, ReadSourceDigest::ModelPayload).is_ok());
        assert!(verified_read_source(&messages, &read, ReadSourceDigest::BoundToolOutput).is_err());
        let mut changed = messages.clone();
        changed[1].text = "changed result".into();
        assert!(verified_read_source(&changed, &read, ReadSourceDigest::ModelPayload).is_err());
        changed[1] = messages[1].clone();
        changed[1]
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_provider_id = "other-provider".into();
        assert!(verified_read_source(&changed, &read, ReadSourceDigest::ModelPayload).is_err());
        changed[1].data_envelope = None;
        assert!(verified_read_source(&changed, &read, ReadSourceDigest::ModelPayload).is_err());
        let mut duplicated = messages.clone();
        duplicated.push(messages[1].clone());
        assert!(verified_read_source(&duplicated, &read, ReadSourceDigest::ModelPayload).is_err());
    }
}

/// Preserve authenticated receipt nodes and every parent while replacing only
/// an approved artifact directory's ephemeral resource label with its contract label.
pub fn verified_task_action_sources(
    session: &crate::session::PersistedAgentSession,
    contract: &crate::schedule::contract::ValidatedTaskContract,
    observed: &crate::provider_preflight::ObservedCapabilityAuthority,
    origin: &crate::action_result::ActionResultOrigin,
    receipt: &crate::action_result::ActionResultReceipt,
    artifact: Option<&desk_agent_protocol::computer_use::CreatedFileArtifactOutput>,
    completed_at: u64,
) -> Result<Vec<RehearsalToolSource>, InvalidReadSource> {
    let mut sources = vec![verified_action_source(
        &session.conversation,
        observed,
        origin,
        receipt,
    )?];
    sources.extend(verified_action_status_sources(
        &session.conversation,
        observed,
        origin,
        receipt,
    )?);
    let Some(rule) = contract.contract().permissions.iter().find(|rule|
        rule.provider_id == observed.provider_id && rule.tool_name == observed.tool_name
            && matches!(rule.input, desk_agent_protocol::schedule::contract::TaskInputConstraint::GeneratedTextArtifact { .. }))
    else { return Ok(sources); };
    if rule.capability_id != observed.capability_id
        || rule.tool_schema_version != observed.tool_schema_version
        || rule.effect != observed.effect
        || rule.risk_tier != observed.risk_tier
    {
        return Err(InvalidReadSource::Conflict);
    }
    let step = contract
        .contract()
        .steps
        .iter()
        .find(|step| step.rule_id == rule.rule_id)
        .ok_or(InvalidReadSource::Conflict)?;
    let calls: Vec<_> = session
        .conversation
        .iter()
        .filter(|message| {
            message.role == ChatRole::Assistant
                && message.turn_id.as_deref() == Some(origin.turn_fence.turn_id.as_str())
        })
        .flat_map(|message| &message.tool_calls)
        .filter(|call| call.id == origin.tool_call_id)
        .collect();
    if calls.len() != 1
        || calls[0].name != observed.tool_name
        || origin.turn_fence.conversation_id != session.conversation_id
    {
        return Err(InvalidReadSource::Conflict);
    }
    let call = ToolCall {
        id: calls[0].id.clone(),
        name: calls[0].name.clone(),
        arguments_json: calls[0].arguments_json.clone(),
    };
    let canonical = crate::permission_tools::canonical_tool_permission_input_json(
        &call.name,
        serde_json::from_str(&call.arguments_json).map_err(|_| InvalidReadSource::Invalid)?,
    )
    .map_err(|_| InvalidReadSource::Invalid)?;
    use sha2::{Digest, Sha256};
    if format!("{:x}", Sha256::digest(canonical.as_bytes())) != observed.canonical_input_sha256 {
        return Err(InvalidReadSource::Conflict);
    }
    crate::schedule::source_graph::attachment::verify_text_artifact_output(
        &call.name,
        &canonical,
        artifact.ok_or(InvalidReadSource::Conflict)?,
    )
    .map_err(|_| InvalidReadSource::Conflict)?;
    let stable = crate::schedule::contract::artifact::bind_directory(
        contract,
        session,
        &call,
        &step.step_id,
        &observed.resources,
        completed_at,
    )
    .map_err(|_| InvalidReadSource::Conflict)?;
    for source in &mut sources {
        if source.authority.authority != TaskSourceAuthority::Scopes(observed.resources.clone()) {
            return Err(InvalidReadSource::Conflict);
        }
        source.authority.authority = TaskSourceAuthority::Scopes(stable.clone());
    }
    Ok(sources)
}
