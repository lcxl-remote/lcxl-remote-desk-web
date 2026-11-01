//! Versioned owner controls. Execution transcripts and original history stay intact.

use super::{AssistantStoreError, increment, runtime_on, signed, unsigned};
use crate::entity::{
    agent_assistant_control_receipt as receipt, agent_assistant_runtime as runtime,
    agent_memory as memory, agent_memory_revision as revision, agent_memory_source as source,
    agent_memory_tombstone as tombstone,
};
use desk_agent_protocol::ai_assistant::memory::{
    AssistantDataControlReceipt, DataControlOutcome, DataControlReason, MemoryClearRequest,
    MemoryDeleteRequest,
};
use desk_diagnose_core::ai_assistant::memory_control::{
    KnownMemorySource, MemoryControlTarget, canonical_sources, manifest_hash,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set, sea_query::Expr,
};
use sha2::{Digest, Sha256};

mod pending;
pub use pending::{
    ControlPreparation, ForgetGroupRequest, PendingControl, prepare_forget_group_on,
    targets_are_stale_on,
};

pub async fn delete_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    request: &MemoryDeleteRequest,
    now: u64,
) -> Result<AssistantDataControlReceipt, AssistantStoreError> {
    let request_hash = request_hash(&("owner-memory-delete-v1", request))?;
    if let Some(previous) = replay_on(db, actor, &request.operation_id, &request_hash).await? {
        return Ok(previous);
    }
    let state = runtime_on(db, actor).await?;
    let row = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(actor))
        .filter(memory::Column::MemoryId.eq(&request.memory_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if unsigned(row.revision)? != request.expected_revision || row.status == "forgotten" {
        return stale(
            &request.operation_id,
            DataControlReason::MemoryForget,
            &state,
        );
    }
    prepare_apply_on(
        db,
        actor,
        &request.operation_id,
        &request_hash,
        DataControlReason::MemoryForget,
        state,
        vec![row],
        None,
        now,
    )
    .await?
    .finalize_on(db)
    .await
}

pub async fn clear_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    request: &MemoryClearRequest,
    now: u64,
) -> Result<AssistantDataControlReceipt, AssistantStoreError> {
    let request_hash = request_hash(&("owner-memory-clear-v1", request))?;
    if let Some(previous) = replay_on(db, actor, &request.operation_id, &request_hash).await? {
        return Ok(previous);
    }
    let state = runtime_on(db, actor).await?;
    if unsigned(state.memory_state_generation)? != request.expected_memory_state_generation {
        return stale(
            &request.operation_id,
            DataControlReason::MemoryClear,
            &state,
        );
    }
    // The actor fence serializes this snapshot with all accepted inputs and writes.
    let rows = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(actor))
        .filter(memory::Column::Status.ne("forgotten"))
        .order_by_asc(memory::Column::MemoryId)
        .all(db)
        .await?;
    prepare_apply_on(
        db,
        actor,
        &request.operation_id,
        &request_hash,
        DataControlReason::MemoryClear,
        state,
        rows,
        None,
        now,
    )
    .await?
    .finalize_on(db)
    .await
}

#[allow(clippy::too_many_arguments)]
async fn prepare_apply_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    operation_id: &str,
    request_hash: &str,
    reason: DataControlReason,
    state: runtime::Model,
    rows: Vec<memory::Model>,
    group: Option<&ForgetGroupRequest>,
    now: u64,
) -> Result<PendingControl, AssistantStoreError> {
    let cutoff = match group {
        Some(request) => signed(request.owner_input_seq)?,
        None => increment(state.input_seq)?,
    };
    let input_seq = if group.is_some() {
        state.input_seq
    } else {
        cutoff
    };
    let memory_generation = increment(state.memory_state_generation)?;
    let control_generation = increment(state.control_generation)?;
    let now = signed(now)?;
    let mut targets = Vec::with_capacity(rows.len());
    for row in &rows {
        targets.push(target_on(db, actor, row).await?);
    }
    let manifest =
        manifest_hash(actor, unsigned(cutoff)?, &targets).map_err(AssistantStoreError::Invalid)?;
    let original_runtime = state.clone();
    let before_memories = rows.clone();
    let mut before_sources = Vec::with_capacity(rows.len());
    let mut before_revisions = Vec::with_capacity(rows.len());
    let mut control_metadata = 0_i64;
    let mut originals = Vec::with_capacity(rows.len());
    let mut expected_memories = Vec::with_capacity(rows.len());
    let mut expected_sources = Vec::with_capacity(rows.len());
    let mut expected_revisions = Vec::with_capacity(rows.len());
    let mut expected_tombstones = Vec::with_capacity(rows.len());
    let mut expected_suppressions = std::collections::BTreeMap::new();
    for (row, target) in rows.into_iter().zip(&targets) {
        let original_revision = row.revision;
        originals.push((row.memory_id.clone(), original_revision));
        let mut sources = pending::sources_on(db, actor, &row.memory_id, original_revision).await?;
        before_sources.push(sources.clone());
        for source in &mut sources {
            if source.revoked_at_ms.is_none() {
                source.revoked_at_ms = Some(now);
            }
        }
        let mut revisions =
            pending::revisions_on(db, actor, &row.memory_id, original_revision).await?;
        before_revisions.push(revisions.clone());
        for revision in &mut revisions {
            revision.status = "forgotten".into();
        }
        expected_sources.push(sources);
        expected_revisions.push(revisions);
        let mut expected = row.clone();
        expected.revision = increment(original_revision)?;
        expected.state_generation = memory_generation;
        expected.status = "forgotten".into();
        expected.updated_at_ms = now;
        expected.valid_until_ms = Some(now);
        let mut active: memory::ActiveModel = row.into();
        active.revision = Set(increment(original_revision)?);
        active.state_generation = Set(memory_generation);
        active.status = Set("forgotten".into());
        active.updated_at_ms = Set(now);
        active.valid_until_ms = Set(Some(now));
        if active.update(db).await? != expected {
            return Err(AssistantStoreError::Inaccessible);
        }
        expected_memories.push(expected);
        source::Entity::update_many()
            .col_expr(source::Column::RevokedAtMs, Expr::value(now))
            .filter(source::Column::ActorId.eq(actor))
            .filter(source::Column::MemoryId.eq(&target.memory_id))
            .filter(source::Column::MemoryRevision.lte(original_revision))
            .filter(source::Column::RevokedAtMs.is_null())
            .exec(db)
            .await?;
        revision::Entity::update_many()
            .col_expr(revision::Column::Status, Expr::value("forgotten"))
            .filter(revision::Column::ActorId.eq(actor))
            .filter(revision::Column::MemoryId.eq(&target.memory_id))
            .filter(revision::Column::Revision.lte(original_revision))
            .exec(db)
            .await?;
        super::suppression::revoke_memory_uses_on(
            db,
            actor,
            &target.memory_id,
            original_revision,
            now,
        )
        .await?;
        let added = pending::suppress_sources_on(
            db,
            actor,
            &target.known_sources,
            cutoff,
            control_generation,
            now,
            &mut expected_suppressions,
        )
        .await?;
        control_metadata =
            control_metadata
                .checked_add(added)
                .ok_or(AssistantStoreError::Invalid(
                    "memory control metadata overflow",
                ))?;
        let mut expected_stone = tombstone::Model {
            id: 0,
            actor_id: actor.into(),
            memory_id: target.memory_id.clone(),
            target_revision: original_revision,
            scope_key: target.scope_key.clone(),

            cutoff_input_seq: cutoff,
            operation_id: operation_id.into(),
            source_refs_json: serde_json::to_string(&target.known_sources)
                .map_err(|_| AssistantStoreError::Invalid("invalid forgotten sources"))?,
            generation: control_generation,
            created_at_ms: now,
        };
        let mut active: tombstone::ActiveModel = expected_stone.clone().into();
        active.id = sea_orm::ActiveValue::NotSet;
        let stone = active.insert(db).await?;
        expected_stone.id = stone.id;
        if stone != expected_stone {
            return Err(AssistantStoreError::Inaccessible);
        }
        control_metadata = control_metadata
            .checked_add(tombstone_metadata_bytes(&stone)?)
            .ok_or(AssistantStoreError::Invalid(
                "memory control metadata overflow",
            ))?;
        expected_tombstones.push(stone);
    }
    let mut active: runtime::ActiveModel = state.clone().into();
    active.input_seq = Set(input_seq);
    active.control_generation = Set(control_generation);
    active.memory_state_generation = Set(memory_generation);
    active.revision = Set(increment(state.revision)?);
    if reason == DataControlReason::MemoryClear {
        active.learning_cutoff_input_seq = Set(cutoff);
    }
    let mut expected_runtime = state.clone();
    expected_runtime.input_seq = input_seq;
    expected_runtime.control_generation = control_generation;
    expected_runtime.memory_state_generation = memory_generation;
    expected_runtime.revision = increment(state.revision)?;
    if reason == DataControlReason::MemoryClear {
        expected_runtime.learning_cutoff_input_seq = cutoff;
    }
    if active.update(db).await? != expected_runtime {
        return Err(AssistantStoreError::Inaccessible);
    }
    let count = i32::try_from(targets.len())
        .map_err(|_| AssistantStoreError::Invalid("too many memory control targets"))?;
    let reason_text = match reason {
        DataControlReason::MemoryForget => "memory_forget",
        DataControlReason::MemoryClear => "memory_clear",
        DataControlReason::HistoryRestricted => {
            return Err(AssistantStoreError::Invalid(
                "owner memory control reason is invalid",
            ));
        }
    };
    let mut expected_receipt = receipt::Model {
        id: 0,
        actor_id: actor.into(),
        operation_id: operation_id.into(),
        request_sha256: request_hash.into(),
        reason: reason_text.into(),
        target_manifest_sha256: manifest,
        outcome: "applied".into(),
        accepted_input_seq: cutoff,
        control_generation,
        memory_state_generation: memory_generation,
        affected_count: count,
        cleanup_pending: count > 0,
        tool_group_id: group.map(|group| group.producer_message_id.clone()),
        tool_group_sha256: None,

        created_at_ms: now,
    };
    let mut active: receipt::ActiveModel = expected_receipt.clone().into();
    active.id = sea_orm::ActiveValue::NotSet;
    let row = active.insert(db).await?;
    expected_receipt.id = row.id;
    if row != expected_receipt {
        return Err(AssistantStoreError::Inaccessible);
    }
    control_metadata = control_metadata
        .checked_add(receipt_metadata_bytes(&row)?)
        .ok_or(AssistantStoreError::Invalid(
            "memory control metadata overflow",
        ))?;
    Ok(PendingControl {
        actor: actor.into(),
        row,
        metadata_delta: control_metadata,
        original_runtime,
        before_memories,
        before_sources,
        before_revisions,
        expected_runtime,
        originals,
        expected_memories,
        expected_sources,
        expected_revisions,
        expected_tombstones,
        expected_suppressions,
    })
}

async fn target_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    row: &memory::Model,
) -> Result<MemoryControlTarget, AssistantStoreError> {
    let evidence = source::Entity::find()
        .filter(source::Column::ActorId.eq(actor))
        .filter(source::Column::MemoryId.eq(&row.memory_id))
        .filter(source::Column::MemoryRevision.eq(row.revision))
        .order_by_asc(source::Column::SourceId)
        .all(db)
        .await?;
    let mut known = Vec::new();
    for source in evidence {
        // Revocation must remain available even if an evidence payload is corrupt.
        // A fact tombstone also fences old learning by identity and input cutoff.
        if let Ok(sources) = super::evidence::forgetting_sources(&source) {
            known.extend(sources);
        }
        let own = KnownMemorySource {
            source_id: source.source_id,
            revision: unsigned(source.memory_revision)?,
            origin_id: source.origin_id,
            content_sha256: source.content_sha256,
        };
        if own.validate().is_ok() {
            known.push(own);
        }
    }
    Ok(MemoryControlTarget {
        memory_id: row.memory_id.clone(),
        revision: unsigned(row.revision)?,
        scope_key: row.scope_key.clone(),

        accepted_input_seq: unsigned(row.accepted_input_seq)?,
        known_sources: canonical_sources(known).map_err(AssistantStoreError::Invalid)?,
    })
}

async fn replay_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    operation_id: &str,
    request_hash: &str,
) -> Result<Option<AssistantDataControlReceipt>, AssistantStoreError> {
    let row = receipt::Entity::find()
        .filter(receipt::Column::ActorId.eq(actor))
        .filter(receipt::Column::OperationId.eq(operation_id))
        .one(db)
        .await?;
    row.map(|row| {
        if row.request_sha256 != request_hash {
            return Err(AssistantStoreError::Invalid(
                "memory operation identity has different parameters",
            ));
        }
        project_receipt(&row, true)
    })
    .transpose()
}

pub fn project_receipt(
    row: &receipt::Model,
    replay: bool,
) -> Result<AssistantDataControlReceipt, AssistantStoreError> {
    let reason = match row.reason.as_str() {
        "memory_forget" => DataControlReason::MemoryForget,
        "memory_clear" => DataControlReason::MemoryClear,
        _ => {
            return Err(AssistantStoreError::Invalid(
                "invalid memory control receipt",
            ));
        }
    };
    if row.outcome != "applied" {
        return Err(AssistantStoreError::Invalid(
            "invalid memory control outcome",
        ));
    }
    Ok(AssistantDataControlReceipt {
        operation_id: row.operation_id.clone(),
        outcome: if replay {
            DataControlOutcome::AlreadyApplied
        } else {
            DataControlOutcome::Applied
        },
        reason,
        affected_count: u32::try_from(row.affected_count)
            .map_err(|_| AssistantStoreError::Invalid("invalid memory control count"))?,
        control_generation: unsigned(row.control_generation)?,
        memory_state_generation: unsigned(row.memory_state_generation)?,
        cleanup_pending: row.cleanup_pending,
    })
}

fn stale(
    operation_id: &str,
    reason: DataControlReason,
    state: &runtime::Model,
) -> Result<AssistantDataControlReceipt, AssistantStoreError> {
    Ok(AssistantDataControlReceipt {
        operation_id: operation_id.into(),
        outcome: DataControlOutcome::StaleTarget,
        reason,
        affected_count: 0,
        control_generation: unsigned(state.control_generation)?,
        memory_state_generation: unsigned(state.memory_state_generation)?,
        cleanup_pending: false,
    })
}

fn request_hash(value: &impl serde::Serialize) -> Result<String, AssistantStoreError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| AssistantStoreError::Invalid("invalid memory control request"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub fn tombstone_metadata_bytes(row: &tombstone::Model) -> Result<i64, AssistantStoreError> {
    control_bytes(
        5,
        &[
            &row.actor_id,
            &row.memory_id,
            &row.scope_key,
            &row.operation_id,
            &row.source_refs_json,
        ],
    )
}

pub fn receipt_metadata_bytes(row: &receipt::Model) -> Result<i64, AssistantStoreError> {
    control_bytes(
        8,
        &[
            &row.actor_id,
            &row.operation_id,
            &row.request_sha256,
            &row.reason,
            &row.target_manifest_sha256,
            &row.outcome,
            row.tool_group_id.as_deref().unwrap_or(""),
            row.tool_group_sha256.as_deref().unwrap_or(""),
        ],
    )
}

fn control_bytes(scalars: usize, values: &[&str]) -> Result<i64, AssistantStoreError> {
    let bytes = values
        .iter()
        .try_fold(scalars * 8, |sum, value| sum.checked_add(value.len()))
        .ok_or(AssistantStoreError::Invalid(
            "memory control metadata size overflow",
        ))?;
    i64::try_from(bytes)
        .map_err(|_| AssistantStoreError::Invalid("memory control metadata size overflow"))
}
