//! Persisted read preparation. Callers retain the original actor/native transaction fence.

use super::{AssistantStoreError, ensure_quota_on, increment, signed, unsigned};
use crate::entity::{
    agent_assistant_quota as quota, agent_assistant_read_projection as projection,
};
use desk_agent_protocol::ai_assistant::{
    source::SourceReadPurpose, user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::ai_assistant::read_projection::{ReadProjectionKind, ReadProjectionProof};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, QueryFilter,
    QuerySelect, sea_query::Expr,
};

pub struct StoredProjection {
    pub row: projection::Model,
    pub proof: Box<ReadProjectionProof>,
}

/// Run under the original actor fence. All authority/source reads precede the
/// quota write; no original business writes may follow this storage operation.
/// A first accepted result is frozen rather than replaced by a retry's query.
pub async fn prepare_on<C: ConnectionTrait>(
    db: &C,
    proof: &ReadProjectionProof,
    content: &str,
    policy: &AssistantDataPolicy,
    now: u64,
) -> Result<StoredProjection, AssistantStoreError> {
    proof
        .validate_cached_result(content)
        .map_err(AssistantStoreError::Invalid)?;
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    if proof.usage.policy_revision != policy.revision || !proof.preparation_available(now, None) {
        return Err(AssistantStoreError::Invalid(
            "assistant read preparation policy or time changed",
        ));
    }
    let old = projection::Entity::find()
        .filter(projection::Column::ActorId.eq(&proof.actor_id))
        .filter(projection::Column::RootConversationId.eq(&proof.root_conversation_id))
        .filter(projection::Column::RetrievalEpisodeId.eq(&proof.usage.retrieval_episode_id))
        .filter(projection::Column::LogicalCallId.eq(&proof.logical_call_id))
        .one(db)
        .await?;
    if let Some(old) = old {
        let stored = decode_row(old)?;
        if !stored.proof.same_logical_input(proof)
            || stored.proof.usage.policy_revision != proof.usage.policy_revision
            || stored.proof.usage.control_generation != proof.usage.control_generation
            || stored.proof.usage.memory_state_generation != proof.usage.memory_state_generation
            || stored.row.revoked_at_ms.is_some()
            || stored.row.result_json.is_none()
            || !stored.proof.preparation_available(
                now,
                stored.row.first_consumed_at_ms.map(unsigned).transpose()?,
            )
        {
            return Err(AssistantStoreError::Invalid(
                "original assistant read is no longer replayable",
            ));
        }
        return Ok(stored);
    }
    let proof_json = serde_json::to_string(proof)
        .map_err(|_| AssistantStoreError::Invalid("invalid assistant read proof"))?;
    let row = projection::Model {
        id: 0,
        projection_id: proof.usage.projection_id.clone(),
        actor_id: proof.actor_id.clone(),
        root_conversation_id: proof.root_conversation_id.clone(),
        retrieval_episode_id: proof.usage.retrieval_episode_id.clone(),
        kind: kind(proof.kind).into(),
        logical_call_id: proof.logical_call_id.clone(),
        producer_message_id: proof.producer_message_id.clone(),
        tool_call_id: proof.tool_call_id.clone(),
        source_envelope_id: proof.source_envelope.envelope_id.clone(),
        request_sha256: proof.request_sha256.clone(),
        scope_sha256: proof.usage.scope_sha256.clone(),
        purpose: purpose(proof.usage.purpose).into(),
        proof_json,
        result_json: Some(content.into()),
        created_at_ms: signed(proof.created_at_unix_ms)?,
        unused_until_ms: signed(proof.unused_until_unix_ms)?,
        first_consumed_at_ms: None,
        last_consumed_at_ms: None,
        revoked_at_ms: None,
        revision: 1,
    };
    adjust_metadata_on(
        db,
        &row.actor_id,
        metadata_bytes(&row)?,
        Some(policy.max_metadata_bytes),
    )
    .await?;
    let mut active: projection::ActiveModel = row.into();
    active.id = sea_orm::ActiveValue::NotSet;
    decode_row(active.insert(db).await?)
}

/// Indexed identity lookup only. This does not authorize the source for a model.
pub async fn load_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    projection_id: &str,
) -> Result<Option<StoredProjection>, AssistantStoreError> {
    projection::Entity::find()
        .filter(projection::Column::ActorId.eq(actor))
        .filter(projection::Column::ProjectionId.eq(projection_id))
        .one(db)
        .await?
        .map(decode_row)
        .transpose()
}

/// Reread the original identity and reuse its decoded proof only while every
/// raw field remains unchanged. Current use-ledger checks stay with the caller.
pub async fn validate_unchanged_on<C: ConnectionTrait>(
    db: &C,
    expected: &StoredProjection,
) -> Result<(), AssistantStoreError> {
    if projection::Entity::find()
        .filter(projection::Column::ActorId.eq(&expected.row.actor_id))
        .filter(projection::Column::ProjectionId.eq(&expected.row.projection_id))
        .one(db)
        .await?
        .as_ref()
        != Some(&expected.row)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

pub fn decode_row(row: projection::Model) -> Result<StoredProjection, AssistantStoreError> {
    let proof = ReadProjectionProof::decode_snapshot(&row.proof_json, row.result_json.as_deref())
        .map_err(AssistantStoreError::Invalid)?;
    if row.revision <= 0
        || row.projection_id != proof.usage.projection_id
        || row.actor_id != proof.actor_id
        || row.root_conversation_id != proof.root_conversation_id
        || row.retrieval_episode_id != proof.usage.retrieval_episode_id
        || row.kind != kind(proof.kind)
        || row.logical_call_id != proof.logical_call_id
        || row.producer_message_id != proof.producer_message_id
        || row.tool_call_id != proof.tool_call_id
        || row.source_envelope_id != proof.source_envelope.envelope_id
        || row.request_sha256 != proof.request_sha256
        || row.scope_sha256 != proof.usage.scope_sha256
        || row.purpose != purpose(proof.usage.purpose)
        || unsigned(row.created_at_ms)? != proof.created_at_unix_ms
        || unsigned(row.unused_until_ms)? != proof.unused_until_unix_ms
        || row.first_consumed_at_ms.is_some() != row.last_consumed_at_ms.is_some()
    {
        return Err(AssistantStoreError::Invalid(
            "stored assistant read binding changed",
        ));
    }
    if let (Some(first), Some(last)) = (row.first_consumed_at_ms, row.last_consumed_at_ms)
        && (first < row.created_at_ms || first >= row.unused_until_ms || last < first)
    {
        return Err(AssistantStoreError::Invalid(
            "stored assistant read consumption changed",
        ));
    }
    Ok(StoredProjection { row, proof })
}

/// Logical UTF-8 metadata accounting. Every scalar slot has a fixed charge;
/// source text cached in result_json is charged until it is explicitly released.
pub fn metadata_bytes(row: &projection::Model) -> Result<i64, AssistantStoreError> {
    let fields = [
        &row.projection_id,
        &row.actor_id,
        &row.root_conversation_id,
        &row.retrieval_episode_id,
        &row.kind,
        &row.logical_call_id,
        &row.source_envelope_id,
        &row.request_sha256,
        &row.scope_sha256,
        &row.purpose,
        &row.proof_json,
    ];
    let bytes = fields
        .iter()
        .try_fold(7 * 8_usize, |sum, field| sum.checked_add(field.len()))
        .and_then(|sum| sum.checked_add(row.producer_message_id.as_ref().map_or(0, String::len)))
        .and_then(|sum| sum.checked_add(row.tool_call_id.as_ref().map_or(0, String::len)))
        .and_then(|sum| sum.checked_add(row.result_json.as_ref().map_or(0, String::len)))
        .ok_or(AssistantStoreError::Invalid(
            "assistant projection metadata size overflow",
        ))?;
    i64::try_from(bytes)
        .map_err(|_| AssistantStoreError::Invalid("assistant projection metadata size overflow"))
}

/// Actor fencing is held by the caller. A failed CAS invalidates the entire
/// transaction; no caller may commit the cache mutation after this returns Err.
pub async fn adjust_metadata_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    delta: i64,
    maximum: Option<u64>,
) -> Result<(), AssistantStoreError> {
    ensure_quota_on(db, actor).await?;
    change_metadata_on(db, actor, delta, maximum, false).await
}

/// Retention owns all original native locks and has finished their mutations.
/// Never recreate a quota partition already removed by account deletion. A
/// missing partition grants no data access and has no remaining charge to refund.
pub async fn refund_existing_metadata_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    bytes: i64,
) -> Result<(), AssistantStoreError> {
    if bytes <= 0 {
        return Err(AssistantStoreError::Invalid(
            "metadata refund must be positive",
        ));
    }
    change_metadata_on(db, actor, -bytes, None, true).await
}

async fn change_metadata_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    delta: i64,
    maximum: Option<u64>,
    missing_allowed: bool,
) -> Result<(), AssistantStoreError> {
    let mut select = quota::Entity::find().filter(quota::Column::ActorId.eq(actor));
    if db.get_database_backend() == DatabaseBackend::Postgres {
        select = select.lock_exclusive();
    }
    let Some(row) = select.one(db).await? else {
        return if missing_allowed {
            Ok(())
        } else {
            Err(AssistantStoreError::Invalid(
                "missing assistant metadata quota",
            ))
        };
    };
    let next = row
        .metadata_bytes
        .checked_add(delta)
        .filter(|value| *value >= 8192)
        .ok_or(AssistantStoreError::Invalid(
            "assistant metadata accounting underflow or overflow",
        ))?;
    if maximum.is_some_and(|limit| next as u64 > limit) {
        return Err(AssistantStoreError::Capacity);
    }
    let changed = quota::Entity::update_many()
        .col_expr(quota::Column::MetadataBytes, Expr::value(next))
        .col_expr(
            quota::Column::Revision,
            Expr::value(increment(row.revision)?),
        )
        .filter(quota::Column::ActorId.eq(actor))
        .filter(quota::Column::Revision.eq(row.revision))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(AssistantStoreError::Invalid(
            "assistant metadata quota changed",
        ));
    }
    Ok(())
}

fn kind(value: ReadProjectionKind) -> &'static str {
    match value {
        ReadProjectionKind::Tool => "tool",
        ReadProjectionKind::CurrentMemories => "current_memories",
    }
}

fn purpose(value: SourceReadPurpose) -> &'static str {
    match value {
        SourceReadPurpose::OwnerHistory => "owner_history",
        SourceReadPurpose::ModelHistory => "model_history",
        SourceReadPurpose::MemoryEvidence => "memory_evidence",
        SourceReadPurpose::ModelMemory => "model_memory",
    }
}

/// Read original observation metadata of an accepted conversation snapshot.
/// This proves what the original model observed; live fact-source eligibility
/// remains the caller's responsibility and grants no model-send authorization.
pub async fn load_conversation_snapshot_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    conversation: &str,
    device: &str,
    label: &desk_agent_protocol::data_lineage::DataEnvelope,
) -> Result<StoredProjection, AssistantStoreError> {
    let id = label
        .provenance
        .source_object_id
        .as_deref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let stored = load_on(db, actor, id)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if stored.row.first_consumed_at_ms.is_none()
        || stored.proof.root_conversation_id != conversation
        || stored.proof.source_device_ref != device
        || stored
            .proof
            .snapshot_envelope()
            .map_err(AssistantStoreError::Invalid)?
            != *label
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(stored)
}
