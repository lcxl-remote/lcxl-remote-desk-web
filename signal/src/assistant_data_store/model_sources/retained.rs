use super::*;

/// Stable native identities remain after natural revision-body cleanup. This
/// path neither validates nor returns the old body/canonical value for reuse.
pub(super) async fn memory_identity_sources_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    actor: &str,
    reference: &CurrentMemoryUse,
) -> Result<(Vec<evidence::Model>, Vec<HistoryDependency>, Option<u64>), AssistantStoreError> {
    memory_identity_on(db, actor, reference, false).await
}

/// Owner copies use only the exact memory references of an accepted consumed
/// read. Forgotten identities grant no body read or renewed current-fact use.
pub(crate) async fn owner_memory_identity_sources_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    view: &desk_diagnose_core::ai_assistant::retained_copy::OwnerHistoryReadValidation,
    reference: &CurrentMemoryUse,
) -> Result<(Vec<evidence::Model>, Vec<HistoryDependency>, Option<u64>), AssistantStoreError> {
    if !view.usage().memories.contains(reference) {
        return Err(AssistantStoreError::Inaccessible);
    }
    memory_identity_on(db, view.actor_id(), reference, true).await
}

async fn memory_identity_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    reference: &CurrentMemoryUse,
    owner_copy: bool,
) -> Result<(Vec<evidence::Model>, Vec<HistoryDependency>, Option<u64>), AssistantStoreError> {
    use desk_diagnose_core::native_ids::{self, Kind};
    if !owner_copy && reference.purpose != MemoryUsePurpose::HistoricalReference {
        return Err(AssistantStoreError::Inaccessible);
    }
    let fact = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(actor))
        .filter(memory::Column::MemoryId.eq(&reference.memory_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if !native_ids::is(Kind::Memory, &fact.memory_id)
        || !(matches!(
            fact.status.as_str(),
            "active" | "pending_conflict" | "superseded"
        ) || (owner_copy && fact.status == "forgotten"))
        || fact.revision <= 0
        || fact.state_generation <= 0
        || fact.accepted_input_seq <= 0
        || reference.revision > unsigned(fact.revision)?
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if fact.scope_key == "personal" {
        if !fact.device_ref.is_empty() {
            return Err(AssistantStoreError::Inaccessible);
        }
    } else if fact.scope_key != format!("device:{}", fact.device_ref)
        || super::super::memory::source_device_on(db, actor, &fact.device_ref)
            .await?
            .is_none()
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let original = revision::Entity::find()
        .filter(revision::Column::ActorId.eq(actor))
        .filter(revision::Column::MemoryId.eq(&reference.memory_id))
        .filter(revision::Column::Revision.eq(signed(reference.revision)?))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if !(matches!(
        original.status.as_str(),
        "active" | "pending_conflict" | "superseded"
    ) || (owner_copy && original.status == "forgotten"))
        || original.accepted_input_seq <= 0
        || original.accepted_input_seq > fact.accepted_input_seq
        || original.created_at_ms <= 0
        || original.state_generation <= 0
        || original.state_generation > fact.state_generation
        || original.content_sha256.len() != 64
        || !original
            .content_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || original
            .expires_at_ms
            .is_some_and(|until| until <= original.created_at_ms)
        || (reference.revision < unsigned(fact.revision)?
            && original.expires_at_ms.is_none()
            && !(owner_copy && original.status == "forgotten"))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let rows = evidence::Entity::find()
        .filter(evidence::Column::ActorId.eq(actor))
        .filter(evidence::Column::MemoryId.eq(&reference.memory_id))
        .filter(evidence::Column::MemoryRevision.eq(signed(reference.revision)?))
        .all(db)
        .await?;
    if rows.is_empty() || !rows.iter().any(|row| row.role == "evidence") {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok((
        rows,
        decode_dependencies(&original.dependencies_json)?,
        None,
    ))
}
