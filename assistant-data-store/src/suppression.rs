//! Indexed exact-fragment suppression. It is independent of owner history viewing.

use super::{AssistantStoreError, signed};
use crate::entity::{
    agent_assistant_data_authorization as authorization,
    agent_assistant_source_suppression as suppression, agent_assistant_source_use as source_use,
    agent_memory_use as memory_use,
};
use desk_agent_protocol::ai_assistant::source::HistoryDependency;
use desk_diagnose_core::ai_assistant::memory_control::{KnownMemorySource, canonical_sources};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect, QueryTrait,
    sea_query::{Expr, OnConflict},
};

/// Run under the actor's original control fence. Identity-only rows do not store
/// remembered content. The caller accounts for the returned metadata delta after
/// its final original business write and rolls back this entire transaction on error.
pub async fn suppress_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    known: &[KnownMemorySource],
    cutoff: i64,
    generation: i64,
    now: i64,
) -> Result<i64, AssistantStoreError> {
    if cutoff <= 0 || generation <= 0 || now <= 0 {
        return Err(AssistantStoreError::Invalid(
            "invalid source suppression control",
        ));
    }
    let known = canonical_sources(known.to_vec()).map_err(AssistantStoreError::Invalid)?;
    if known.is_empty() {
        return Ok(0);
    }
    let mut exact = Condition::any();
    for source in &known {
        exact = exact.add(known_condition(source)?);
    }
    let old = suppression::Entity::find()
        .filter(suppression::Column::ActorId.eq(actor))
        .filter(exact)
        .limit(known.len() as u64 + 1)
        .all(db)
        .await?;
    let mut existing = std::collections::BTreeSet::new();
    for row in old {
        if row.cutoff_input_seq <= 0
            || row.cutoff_input_seq > cutoff
            || row.control_generation <= 0
            || row.control_generation > generation
            || row.created_at_ms <= 0
            || !existing.insert((
                row.source_id,
                row.source_revision,
                row.origin_id,
                row.content_sha256,
            ))
        {
            return Err(AssistantStoreError::Invalid(
                "stored source suppression changed",
            ));
        }
    }
    let mut delta = 0_i64;
    for chunk in known.chunks(64) {
        let mut rows = Vec::with_capacity(chunk.len());
        for source in chunk {
            let revision = signed(source.revision)?;
            let row = suppression::Model {
                id: 0,
                actor_id: actor.into(),
                source_id: source.source_id.clone(),
                source_revision: revision,
                origin_id: source.origin_id.clone(),
                content_sha256: source.content_sha256.clone(),
                cutoff_input_seq: cutoff,
                control_generation: generation,
                created_at_ms: now,
            };
            if !existing.contains(&(
                row.source_id.clone(),
                row.source_revision,
                row.origin_id.clone(),
                row.content_sha256.clone(),
            )) {
                delta = delta.checked_add(metadata_bytes(&row)?).ok_or(
                    AssistantStoreError::Invalid("source suppression metadata overflow"),
                )?;
            }
            let mut active: suppression::ActiveModel = row.into();
            active.id = sea_orm::ActiveValue::NotSet;
            rows.push(active);
        }
        suppression::Entity::insert_many(rows)
            .on_conflict(
                OnConflict::columns([
                    suppression::Column::ActorId,
                    suppression::Column::SourceId,
                    suppression::Column::SourceRevision,
                    suppression::Column::OriginId,
                    suppression::Column::ContentSha256,
                ])
                .update_columns([
                    suppression::Column::CutoffInputSeq,
                    suppression::Column::ControlGeneration,
                ])
                .to_owned(),
            )
            .exec_without_returning(db)
            .await?;
    }
    let mut used_sources = Condition::any();
    for source in &known {
        used_sources = used_sources.add(
            Condition::all()
                .add(source_use::Column::SourceId.eq(&source.source_id))
                .add(source_use::Column::SourceRevision.eq(signed(source.revision)?)),
        );
    }
    let uses = source_use::Entity::find()
        .select_only()
        .column(source_use::Column::AuthorizationId)
        .filter(source_use::Column::ActorId.eq(actor))
        .filter(used_sources)
        .into_query();
    authorization::Entity::update_many()
        .col_expr(authorization::Column::RevokedAtMs, Expr::value(Some(now)))
        .filter(authorization::Column::ActorId.eq(actor))
        .filter(authorization::Column::RevokedAtMs.is_null())
        .filter(authorization::Column::AuthorizationId.in_subquery(uses))
        .exec(db)
        .await?;
    Ok(delta)
}

/// Indexed by the full immutable fragment identity. A shared message/origin is
/// deliberately insufficient; no actor-wide tombstone JSON scan is performed.
pub async fn suppressed_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    dependency: &HistoryDependency,
) -> Result<bool, AssistantStoreError> {
    suppressed_known_on(
        db,
        actor,
        &KnownMemorySource {
            source_id: dependency.source_id.clone(),
            revision: dependency.source_revision,
            origin_id: dependency.origin_id.clone(),
            content_sha256: dependency.content_sha256.clone(),
        },
    )
    .await
}

pub async fn suppressed_known_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    source: &KnownMemorySource,
) -> Result<bool, AssistantStoreError> {
    Ok(known_row_on(db, actor, source).await?.is_some())
}

pub async fn known_row_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    source: &KnownMemorySource,
) -> Result<Option<suppression::Model>, AssistantStoreError> {
    source.validate().map_err(AssistantStoreError::Invalid)?;
    let row = suppression::Entity::find()
        .filter(suppression::Column::ActorId.eq(actor))
        .filter(suppression::Column::SourceId.eq(&source.source_id))
        .filter(suppression::Column::SourceRevision.eq(signed(source.revision)?))
        .filter(suppression::Column::OriginId.eq(&source.origin_id))
        .filter(suppression::Column::ContentSha256.eq(&source.content_sha256))
        .one(db)
        .await?;
    Ok(row)
}

pub async fn revoke_memory_uses_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    memory: &str,
    through: i64,
    now: i64,
) -> Result<(), AssistantStoreError> {
    let uses = memory_use::Entity::find()
        .select_only()
        .column(memory_use::Column::AuthorizationId)
        .filter(memory_use::Column::ActorId.eq(actor))
        .filter(memory_use::Column::MemoryId.eq(memory))
        .filter(memory_use::Column::MemoryRevision.lte(through))
        .into_query();
    authorization::Entity::update_many()
        .col_expr(authorization::Column::RevokedAtMs, Expr::value(Some(now)))
        .filter(authorization::Column::ActorId.eq(actor))
        .filter(authorization::Column::RevokedAtMs.is_null())
        .filter(authorization::Column::AuthorizationId.in_subquery(uses))
        .exec(db)
        .await?;
    Ok(())
}

fn known_condition(source: &KnownMemorySource) -> Result<Condition, AssistantStoreError> {
    source.validate().map_err(AssistantStoreError::Invalid)?;
    Ok(Condition::all()
        .add(suppression::Column::SourceId.eq(&source.source_id))
        .add(suppression::Column::SourceRevision.eq(signed(source.revision)?))
        .add(suppression::Column::OriginId.eq(&source.origin_id))
        .add(suppression::Column::ContentSha256.eq(&source.content_sha256)))
}

pub fn metadata_bytes(row: &suppression::Model) -> Result<i64, AssistantStoreError> {
    let bytes = [
        &row.actor_id,
        &row.source_id,
        &row.origin_id,
        &row.content_sha256,
    ]
    .iter()
    .try_fold(5 * 8_usize, |sum, field| sum.checked_add(field.len()))
    .ok_or(AssistantStoreError::Invalid(
        "source suppression size overflow",
    ))?;
    i64::try_from(bytes)
        .map_err(|_| AssistantStoreError::Invalid("source suppression size overflow"))
}

/// Exact bounded control rows for a writer's original source set.
pub async fn matching_rows_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    known: &[KnownMemorySource],
) -> Result<Vec<suppression::Model>, AssistantStoreError> {
    let known = canonical_sources(known.to_vec()).map_err(AssistantStoreError::Invalid)?;
    if known.is_empty() {
        return Ok(Vec::new());
    }
    let mut exact = Condition::any();
    for source in &known {
        exact = exact.add(known_condition(source)?);
    }
    let rows = suppression::Entity::find()
        .filter(suppression::Column::ActorId.eq(actor))
        .filter(exact)
        .limit(known.len() as u64 + 1)
        .all(db)
        .await?;
    if rows.len() > known.len() {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(rows)
}
