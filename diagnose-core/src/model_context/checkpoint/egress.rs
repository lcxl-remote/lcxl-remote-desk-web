//! Authorization for compression is checked against the original messages,
//! not inferred from their redacted or summarized representation.

use std::collections::{BTreeMap, BTreeSet};

use desk_agent_protocol::data_lineage::{ContentRef, DataEnvelope};

use super::*;
use crate::{
    chat::ModelTurn,
    data_policy::{ConservativeDerivation, derive_conservatively},
    model_egress::{ModelEgressPolicy, model_turn_content_bytes},
    prompt::ResponseFormatSpec,
    seam::ModelRequest,
    sink_authorizer::{DefaultSinkAuthorizer, SinkAuthorizer, SinkInput},
};

// Reserve one lineage slot for a prior checkpoint's own model-output envelope.
const MAX_SUMMARY_SOURCES: usize = desk_agent_protocol::data_lineage::MAX_LINEAGE_ITEMS - 1;

/// Reconcile the model-only floor before freezing compression sources. Never
/// delete transcript data or extend a source's authority/retention boundary.
pub fn reconcile_context_eligibility(
    egress: &ModelEgressPolicy,
    conversation: &[ChatMessage],
    state: &ModelContextState,
    policy: &PinnedContextPolicy,
    protection: &ContextProtectionSet,
    version: i64,
) -> Result<Option<FloorReconciliationPlan>, ModelContextError> {
    if policy.strategy != ContextManagementStrategy::CheckpointSummary || conversation.is_empty() {
        return Ok(None);
    }
    let groups = group_messages(conversation, &policy.source_context_key)?;
    let protected = protected_group_indices(conversation, &groups, protection)?;
    let key = policy.key();
    let entry = state.entries.iter().find(|entry| entry.policy_key == key);
    let floor = resolve_floor(conversation, &groups, entry)?;
    let retained = egress.retained_history_ids(conversation);
    let current_turn = conversation
        .iter()
        .rev()
        .find_map(|message| message.turn_id.as_ref());
    let cutoff = egress
        .now_unix_ms
        .saturating_add(crate::model_egress::MODEL_CALL_RETENTION_HEADROOM_MS);
    let last_invalid = groups
        .iter()
        .enumerate()
        .skip(floor)
        .filter_map(|(index, group)| {
            let ineligible = conversation[group.start..group.end].iter().any(|message| {
                !retained.contains(&message.message_id)
                    || message
                        .data_envelope
                        .as_ref()
                        .and_then(|envelope| envelope.retention.expires_at_unix_ms)
                        .is_some_and(|expiry| expiry <= cutoff)
            });
            ineligible.then_some(index)
        })
        .max();
    if let Some(last) = last_invalid {
        if protected.range(..=last).next().is_some()
            || conversation[..groups[last].end]
                .iter()
                .any(|message| current_turn.is_some() && message.turn_id.as_ref() == current_turn)
        {
            return Err(ModelContextError::InvalidProtectionReference(
                conversation[groups[last].start].message_id.clone(),
            ));
        }
        return floor_reconciliation_plan(
            conversation,
            state,
            policy,
            version,
            &groups,
            last + 1,
            None,
        )
        .map(Some);
    }
    if entry.and_then(|entry| entry.checkpoint.as_ref()).is_some()
        && authorize_context_checkpoint(egress, state, &key, conversation).is_err()
    {
        // A checkpoint summarizes only groups before its floor. Keep that
        // monotonic floor when dropping a summary that may no longer be sent.
        if floor == 0 {
            return Err(ModelContextError::StaleCompressionPlan);
        }
        return floor_reconciliation_plan(
            conversation,
            state,
            policy,
            version,
            &groups,
            floor,
            None,
        )
        .map(Some);
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSummarySourceV1 {
    pub message_id: String,
    pub message_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSummaryLineageV1 {
    pub envelope: DataEnvelope,
    /// Includes continuation-lens dependencies, not just cited/covered history.
    pub sources: Vec<ContextSummarySourceV1>,
}

pub struct AuthorizedCompressionInput {
    pub messages: Vec<ChatMessage>,
    sources: Vec<ContextSummarySourceV1>,
}

/// Recheck a persisted checkpoint without reminting its export authority or TTL.
/// Missing legacy labels are accepted only by non-strict model seams.
pub fn authorize_context_checkpoint(
    policy: &ModelEgressPolicy,
    state: &ModelContextState,
    key: &ContextPolicyKey,
    conversation: &[ChatMessage],
) -> Result<(), ModelContextError> {
    let Some(checkpoint) = state
        .entries
        .iter()
        .find(|entry| &entry.policy_key == key)
        .and_then(|entry| entry.checkpoint.as_ref())
    else {
        return Ok(());
    };
    let checkpoint = checkpoint.v1();
    let lineage = checkpoint.lineage.as_ref().ok_or_else(lineage_error)?;
    let canonical = canonical_json(&checkpoint.summary)?;
    validate_summary_lineage(lineage, &canonical, conversation)?;
    authorize_sources(policy, &lineage.sources, conversation)?;
    authorize_bytes(policy, &lineage.envelope, canonical.as_bytes())
}

/// The frozen compression plan must match the full, unchanged conversation.
/// Unlike a normal history request, none of its required inputs may be silently
/// pruned: that would leave the citation range and checkpoint CAS inconsistent.
pub fn authorize_compression_input(
    policy: &ModelEgressPolicy,
    plan: &CompressionPlan,
    conversation: &[ChatMessage],
) -> Result<AuthorizedCompressionInput, ModelContextError> {
    match plan_model_context(
        conversation,
        &plan.base_state,
        &plan.policy,
        &plan.protection,
        plan.base_session_version,
    )? {
        ContextBuildPlan::NeedsCompression(rebuilt) if rebuilt.as_ref() == plan => {}
        _ => return Err(ModelContextError::StaleCompressionPlan),
    }
    authorize_context_checkpoint(policy, &plan.base_state, &plan.policy_key, conversation)?;
    let sources = compression_source_bindings(plan, conversation)?;
    let mut parents = authorize_sources(policy, &sources, conversation)?;
    if let Some(checkpoint) = prior_checkpoint(plan) {
        let lineage = checkpoint.lineage.as_ref().ok_or_else(lineage_error)?;
        // Prior model output is itself a source: its shorter TTL and stricter
        // destination intersection must survive another compression generation.
        parents.push(lineage.envelope.clone());
    }
    let mut messages = compression_request_messages(plan);
    let packed = messages.last_mut().ok_or_else(lineage_error)?;
    packed.data_envelope = Some(derive_projection(
        &packed.text,
        &parents,
        "compression-input",
        plan.generation,
    )?);
    // Check the exact packed payload, too; a transform cannot enlarge the sink
    // byte budget or change its destination merely by relabeling its output.
    authorize_bytes(
        policy,
        packed.data_envelope.as_ref().ok_or_else(lineage_error)?,
        packed.text.as_bytes(),
    )?;
    Ok(AuthorizedCompressionInput { messages, sources })
}

/// Canonicalization and deterministic omitted-evidence annotations derive from
/// the accepted provider output. They never become a fresh user export grant.
pub fn bind_context_summary_lineage(
    policy: &ModelEgressPolicy,
    validated: &mut ValidatedContextSummary,
    turn: &ModelTurn,
    input: &AuthorizedCompressionInput,
) -> Result<(), ModelContextError> {
    let output = turn
        .provider_meta
        .data_envelope
        .as_ref()
        .ok_or_else(lineage_error)?;
    let bytes = model_turn_content_bytes(turn).map_err(|_| lineage_error())?;
    authorize_bytes(policy, output, &bytes)?;
    let packed = input
        .messages
        .last()
        .and_then(|message| message.data_envelope.as_ref())
        .ok_or_else(lineage_error)?;
    let canonical = canonical_json(&validated.summary)?;
    let mut envelope = derive_projection(
        &canonical,
        &[output.clone(), packed.clone()],
        "checkpoint-summary",
        0,
    )?;
    envelope.provenance.source_object_id = Some(source_binding_digest(&envelope, &input.sources)?);
    authorize_bytes(policy, &envelope, canonical.as_bytes())?;
    validated.lineage = Some(ContextSummaryLineageV1 {
        envelope,
        sources: input.sources.clone(),
    });
    Ok(())
}

pub(super) fn compression_source_bindings(
    plan: &CompressionPlan,
    conversation: &[ChatMessage],
) -> Result<Vec<ContextSummarySourceV1>, ModelContextError> {
    let mut ids = plan
        .input
        .summarize_prefix
        .iter()
        .chain(plan.input.continuation_lens.iter())
        .map(|message| message.message_id.as_str())
        .collect::<BTreeSet<_>>();
    if let Some(checkpoint) = prior_checkpoint(plan) {
        let lineage = checkpoint.lineage.as_ref().ok_or_else(lineage_error)?;
        validate_source_bindings(&lineage.sources, conversation)?;
        ids.extend(
            lineage
                .sources
                .iter()
                .map(|source| source.message_id.as_str()),
        );
    }
    if ids.is_empty() || ids.len() > MAX_SUMMARY_SOURCES {
        return Err(lineage_error());
    }
    let messages = conversation_index(conversation)?;
    ids.into_iter()
        .map(|id| {
            let message = messages.get(id).ok_or_else(lineage_error)?;
            Ok(ContextSummarySourceV1 {
                message_id: id.into(),
                message_sha256: sha256_hex(&canonical_bytes(message)?),
            })
        })
        .collect()
}

fn prior_checkpoint(plan: &CompressionPlan) -> Option<&ContextCheckpointV1> {
    plan.base_state
        .entries
        .iter()
        .find(|entry| entry.policy_key == plan.policy_key)
        .and_then(|entry| entry.checkpoint.as_ref())
        .map(ContextCheckpoint::v1)
}

fn conversation_index(
    conversation: &[ChatMessage],
) -> Result<BTreeMap<&str, &ChatMessage>, ModelContextError> {
    let messages = conversation
        .iter()
        .map(|message| (message.message_id.as_str(), message))
        .collect::<BTreeMap<_, _>>();
    if messages.len() != conversation.len() {
        return Err(lineage_error());
    }
    Ok(messages)
}

fn validate_source_bindings(
    sources: &[ContextSummarySourceV1],
    conversation: &[ChatMessage],
) -> Result<(), ModelContextError> {
    if sources.is_empty() || sources.len() > MAX_SUMMARY_SOURCES {
        return Err(lineage_error());
    }
    let messages = conversation_index(conversation)?;
    let mut previous: Option<&str> = None;
    for source in sources {
        if previous.is_some_and(|id| id >= source.message_id.as_str()) {
            return Err(lineage_error());
        }
        let message = messages
            .get(source.message_id.as_str())
            .ok_or_else(lineage_error)?;
        if sha256_hex(&canonical_bytes(message)?) != source.message_sha256 {
            return Err(lineage_error());
        }
        previous = Some(&source.message_id);
    }
    Ok(())
}

pub(super) fn validate_summary_lineage(
    lineage: &ContextSummaryLineageV1,
    canonical: &str,
    conversation: &[ChatMessage],
) -> Result<(), ModelContextError> {
    validate_source_bindings(&lineage.sources, conversation)?;
    lineage.envelope.validate().map_err(|_| lineage_error())?;
    if lineage.envelope.digest_sha256 != sha256_hex(canonical.as_bytes())
        || lineage.envelope.provenance.source_object_id.as_deref()
            != Some(source_binding_digest(&lineage.envelope, &lineage.sources)?.as_str())
        || !matches!(&lineage.envelope.content, ContentRef::ImmutableBlob { size_bytes, .. }
            if *size_bytes == canonical.len() as u64)
    {
        return Err(lineage_error());
    }
    Ok(())
}

fn source_binding_digest(
    envelope: &DataEnvelope,
    sources: &[ContextSummarySourceV1],
) -> Result<String, ModelContextError> {
    Ok(sha256_hex(&canonical_bytes(&(
        &envelope.digest_sha256,
        sources,
    ))?))
}

fn authorize_sources(
    policy: &ModelEgressPolicy,
    sources: &[ContextSummarySourceV1],
    conversation: &[ChatMessage],
) -> Result<Vec<DataEnvelope>, ModelContextError> {
    validate_source_bindings(sources, conversation)?;
    let messages = conversation_index(conversation)?;
    sources
        .iter()
        .map(|source| {
            let message = messages
                .get(source.message_id.as_str())
                .ok_or_else(lineage_error)?;
            if message.role == ChatRole::Tool
                && message.data_envelope.as_ref().is_some_and(|envelope| {
                    envelope.provenance.source_provider_id
                        != crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
                        && !policy
                            .selected_source_tools
                            .contains(&envelope.provenance.source_tool_name)
                })
            {
                return Err(lineage_error());
            }
            let authorized = policy
                .authorize_request(ModelRequest::text_only(
                    vec![(*message).clone()],
                    ResponseFormatSpec::None,
                ))
                .map_err(|_| lineage_error())?;
            if authorized.request.messages.len() != 1 {
                return Err(lineage_error());
            }
            authorized
                .input_envelopes
                .into_iter()
                .next()
                .ok_or_else(lineage_error)
        })
        .collect()
}

fn authorize_bytes(
    policy: &ModelEgressPolicy,
    envelope: &DataEnvelope,
    bytes: &[u8],
) -> Result<(), ModelContextError> {
    DefaultSinkAuthorizer
        .authorize(
            &policy.destination,
            &[SinkInput { envelope, bytes }],
            policy.now_unix_ms,
            policy.byte_cap,
        )
        .map(|_| ())
        .map_err(|_| lineage_error())
}

fn derive_projection(
    text: &str,
    inputs: &[DataEnvelope],
    tool: &str,
    generation: u32,
) -> Result<DataEnvelope, ModelContextError> {
    let digest = sha256_hex(text.as_bytes());
    // Bind the label identity to all input labels as well as transformed bytes.
    let binding = sha256_hex(&canonical_bytes(&(tool, generation, &digest, inputs))?);
    let content = ContentRef::ImmutableBlob {
        blob_id: format!("context-projection:{binding}"),
        sha256: digest.clone(),
        size_bytes: text.len() as u64,
        media_type: "application/json".into(),
    };
    let (mut envelope, _) = derive_conservatively(
        inputs,
        ConservativeDerivation {
            output_envelope_id: &format!("context-lineage:{binding}"),
            content,
            digest_sha256: &digest,
            source_provider_id: "device-assistant-context",
            source_tool_name: tool,
            source_object_id: Some(&binding),
        },
    )
    .map_err(|_| lineage_error())?;
    // An observation's content expiry is independently binding even when its
    // retention field was absent. An immutable projection must not erase it.
    for input in inputs {
        if let ContentRef::EphemeralObservation {
            expires_at_unix_ms, ..
        } = &input.content
        {
            envelope.retention.expires_at_unix_ms = Some(
                envelope
                    .retention
                    .expires_at_unix_ms
                    .map_or(*expires_at_unix_ms, |expiry| {
                        expiry.min(*expires_at_unix_ms)
                    }),
            );
        }
    }
    envelope.validate().map_err(|_| lineage_error())?;
    Ok(envelope)
}

fn lineage_error() -> ModelContextError {
    ModelContextError::InvalidCheckpoint(
        "context source authorization is unavailable or stale".into(),
    )
}

#[cfg(test)]
mod tests;
