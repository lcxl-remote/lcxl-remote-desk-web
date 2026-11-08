//! Original history rows admitted for independent memory snapshots.

use super::*;
use crate::entity::agent_assistant_history as history;
use desk_agent_protocol::data_lineage::DataEnvelope;
use desk_diagnose_core::ai_assistant::{
    retrieval_query::QuerySourceVersion,
    seam::AssistantDataCall,
    source::{HistoryRetentionProof, validate_policy},
};

pub struct MemorySourceRows {
    pub history: history::Model,
    pub origin: origin::Model,
    pub policy: SourceUsePolicy,
    known: Vec<KnownMemorySource>,
}

impl MemorySourceRows {
    /// Re-read the original rows immediately before admission, without decoding them again.
    pub async fn validate_on<C: ConnectionTrait>(&self, db: &C) -> Result<(), AssistantStoreError> {
        if history::Entity::find_by_id(self.history.id)
            .one(db)
            .await?
            .as_ref()
            != Some(&self.history)
            || origin::Entity::find_by_id(self.origin.id)
                .one(db)
                .await?
                .as_ref()
                != Some(&self.origin)
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        Ok(())
    }

    pub fn snapshot(
        &self,
        device: Option<AssistantSourceDevice>,
        input_seq: u64,
    ) -> MemoryWriteSource {
        MemoryWriteSource {
            body: self.history.content.clone(),
            origin: self.origin.clone(),
            policy: self.policy.clone(),
            known: self.known.clone(),
            device,
            role: if self.origin.accepted_input_seq == input_seq as i64
                && matches!(self.origin.kind.as_str(), "input" | "receipt")
            {
                SourceRefRole::Evidence
            } else {
                SourceRefRole::Background
            },
        }
    }
}

pub async fn load_on<C: ConnectionTrait>(
    db: &C,
    context: AssistantDataCall<'_>,
    reference: &AssistantSourceRef,
    labels: &[&DataEnvelope],
    observed: Option<&QuerySourceVersion>,
    state: &runtime::Model,
) -> Result<Option<MemorySourceRows>, AssistantStoreError> {
    let actor = &context.session.actor_id;
    let data = context
        .session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let Some(row) = history::Entity::find()
        .filter(history::Column::ActorId.eq(actor))
        .filter(history::Column::SourceId.eq(&reference.id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let Some(input) = origin::Entity::find()
        .filter(origin::Column::ActorId.eq(actor))
        .filter(origin::Column::OriginId.eq(&row.origin_id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let proof: HistoryRetentionProof = serde_json::from_str(&row.retention_proof_json)
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    let policy: SourceUsePolicy =
        serde_json::from_str(&input.policy_json).map_err(|_| AssistantStoreError::Inaccessible)?;
    validate_policy(&policy).map_err(AssistantStoreError::Invalid)?;
    let seen = observed.is_some_and(|seen| {
        seen.revision == row.revision as u64 && seen.content_sha256 == row.content_sha256
    }) || (input.kind == "input"
        && input.conversation_id == context.session.conversation_id
        && input.device_ref == data.source_device_ref
        && labels.iter().any(|label| {
            label.envelope_id == proof.original_envelope_id
                && label.provenance.source_provider_id == "ai-assistant-user"
                && label.provenance.source_object_id.as_deref() == Some(input.native_id.as_str())
        }));
    if !seen
        || row.revision <= 0
        || reference.revision.is_some_and(|v| v != row.revision as u64)
        || row.deleted_at_ms.is_some()
        || row.expires_at_ms <= signed(context.now_unix_ms)?
        || row.admission != "indexed"
        || !input.history_admitted
        || input.deleted_at_ms.is_some()
        || input.deletion_generation <= 0
        || input.accepted_input_seq <= state.learning_cutoff_input_seq
        || input.accepted_input_seq > data.current_actor_input_seq as i64
        || input.accepted_input_seq <= 0
        || row.kind != input.kind
        || row.device_ref != input.device_ref
        || row.native_id != input.native_id
        || row.conversation_id != input.conversation_id
        || row.root_conversation_id != input.root_conversation_id
        || row.recorded_at_ms != input.created_at_ms
        || row.statement_timezone != input.statement_timezone
        || row.recorded_at_ms <= 0
        || row.recorded_at_ms > signed(context.now_unix_ms)?
        || row.content_sha256 != format!("{:x}", Sha256::digest(row.content.as_bytes()))
        || proof.actor_id != *actor
        || proof.origin_id != row.origin_id
        || proof.device_ref != row.device_ref
        || proof.accepted_input_seq != input.accepted_input_seq as u64
        || proof.source_policy != policy
        || policy.policy_revision
            != data
                .memory_access_scope
                .as_ref()
                .ok_or(AssistantStoreError::Inaccessible)?
                .policy_revision
        || proof.recorded_at_unix_ms != row.recorded_at_ms as u64
        || proof.statement_timezone != row.statement_timezone
        || proof.expires_at_unix_ms != row.expires_at_ms as u64
        || !desk_diagnose_core::ai_assistant::memory_evidence::retained_text_policy(&policy)
        || policy
            .content_hard_expires_at_unix_ms
            .is_some_and(|until| until <= context.now_unix_ms)
    {
        return Err(AssistantStoreError::SourceUnavailable);
    }
    let dependencies: Vec<HistoryDependency> = serde_json::from_str(&row.dependencies_json)
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    desk_diagnose_core::ai_assistant::source::validate_history_dependencies(&dependencies)
        .map_err(AssistantStoreError::Invalid)?;
    crate::history_native::read(&row)?;
    let dependency = HistoryDependency {
        source_id: row.source_id.clone(),
        source_revision: row.revision as u64,
        origin_id: row.origin_id.clone(),
        origin_deletion_generation: input.deletion_generation as u64,
        content_sha256: row.content_sha256.clone(),
        policy_revision: policy.policy_revision,
        expires_at_unix_ms: None,
    };
    if crate::suppression::suppressed_on(db, actor, &dependency).await? {
        return Err(AssistantStoreError::SourceUnavailable);
    }
    let mut known = vec![KnownMemorySource {
        source_id: row.source_id.clone(),
        revision: row.revision as u64,
        origin_id: input.origin_id.clone(),
        content_sha256: row.content_sha256.clone(),
    }];
    known.extend(dependencies.into_iter().map(|source| KnownMemorySource {
        source_id: source.source_id,
        revision: source.source_revision,
        origin_id: source.origin_id,
        content_sha256: source.content_sha256,
    }));
    Ok(Some(MemorySourceRows {
        history: row,
        origin: input,
        policy,
        known,
    }))
}
