//! Bind the original user-authored requirement to the reviewed task definition.
use crate::{
    chat::ChatRole,
    model_egress::ModelInputLineage,
    model_message_labels::model_bound_user_message,
    schedule::{
        contract::ValidatedTaskContract,
        source_graph::{TaskSourceAuthority, TaskSourceBinding},
    },
    session::PersistedAgentSession,
};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidFixedTaskInput;

/// This is a data-source scope, never a device operation or export grant.
/// It remains stable across runs of the same task requirement.
pub fn task_input_scope(contract: &ValidatedTaskContract) -> String {
    let task = contract.contract();
    let mut hash = Sha256::new();
    let revision = task.task_revision.to_string();
    for field in [
        "task-input-v1",
        &task.schedule_id,
        &revision,
        &task.target_device_id,
        &task.prompt_sha256,
    ] {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    format!("task-input:sha256:{hash:x}", hash = hash.finalize())
}

/// The caller verifies owner, task revision and frozen session before use.
/// A model-authored summary, different prompt or additional input cannot stand
/// in for the original fixed requirement. Send scope is checked separately.
pub fn fixed_task_input_source(
    session: &PersistedAgentSession,
    original_input_id: &str,
    contract: &ValidatedTaskContract,
) -> Result<(ModelInputLineage, TaskSourceBinding), InvalidFixedTaskInput> {
    let inputs: Vec<_> = session
        .conversation
        .iter()
        .filter(|message| {
            message.role == ChatRole::User
                && !crate::permission_resume::is_resume_control_message(message)
        })
        .collect();
    if inputs.len() != 1
        || inputs[0].message_id != original_input_id
        || session.device_id != contract.contract().target_device_id
        || session
            .conversation
            .iter()
            .filter(|message| message.message_id == original_input_id)
            .count()
            != 1
    {
        return Err(InvalidFixedTaskInput);
    }
    let input = inputs[0];
    if input.text.trim().is_empty()
        || input.image_data_url.is_some()
        || !input.tool_calls.is_empty()
        || input.tool_call_id.is_some()
        || input.background_task_id.is_some()
        || format!("{:x}", Sha256::digest(input.text.as_bytes()))
            != contract.contract().prompt_sha256
    {
        return Err(InvalidFixedTaskInput);
    }
    let label = input.data_envelope.as_ref().ok_or(InvalidFixedTaskInput)?;
    if label.allowed_destinations.len() != 1
        || !matches!(
            label.allowed_destinations[0],
            desk_agent_protocol::data_lineage::DestinationIdentity::Model { .. }
        )
    {
        return Err(InvalidFixedTaskInput);
    }
    let expected = model_bound_user_message(
        input.message_id.clone(),
        input.text.clone(),
        label.allowed_destinations[0].clone(),
    )
    .map_err(|_| InvalidFixedTaskInput)?;
    if expected.data_envelope.as_ref() != Some(label) {
        return Err(InvalidFixedTaskInput);
    }
    Ok((
        ModelInputLineage {
            public_system_prompt: false,
            envelope_id: label.envelope_id.clone(),
            digest_sha256: label.digest_sha256.clone(),
            source_provider_id: label.provenance.source_provider_id.clone(),
            source_tool_name: label.provenance.source_tool_name.clone(),
            source_envelope_ids: label.provenance.source_envelope_ids.clone(),
        },
        TaskSourceBinding {
            envelope_id: label.envelope_id.clone(),
            digest_sha256: label.digest_sha256.clone(),
            authority: TaskSourceAuthority::Scopes(vec![task_input_scope(contract)]),
        },
    ))
}
