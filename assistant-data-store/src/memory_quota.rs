use crate::entity::{
    agent_assistant_quota as quota, agent_assistant_search_document as document,
    agent_assistant_search_term as term, agent_memory as memory, agent_memory_revision as revision,
    agent_memory_source as source,
};
use crate::{AssistantStoreError, increment, signed};
use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, QueryFilter, QuerySelect,
    sea_query::Expr,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FactQuotaDelta {
    pub memory_records: i64,
    pub memory_body_bytes: i64,
    pub term_records: i64,
    pub metadata_bytes: i64,
}

pub async fn apply_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    delta: FactQuotaDelta,
    policy: &AssistantDataPolicy,
) -> Result<(), AssistantStoreError> {
    if delta == FactQuotaDelta::default() {
        return Ok(());
    }
    let mut select = quota::Entity::find().filter(quota::Column::ActorId.eq(actor));
    if db.get_database_backend() == DatabaseBackend::Postgres {
        select = select.lock_exclusive();
    }
    let row = select
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let records = project(
        row.memory_records,
        delta.memory_records,
        0,
        policy.max_memory_records,
    )?;
    let bodies = project(
        row.memory_body_bytes,
        delta.memory_body_bytes,
        0,
        policy.max_memory_body_bytes,
    )?;
    let terms = project(
        row.term_records,
        delta.term_records,
        0,
        policy.max_term_records,
    )?;
    let metadata = project(
        row.metadata_bytes,
        delta.metadata_bytes,
        8192,
        policy.max_metadata_bytes,
    )?;
    let changed = quota::Entity::update_many()
        .col_expr(quota::Column::MemoryRecords, Expr::value(records))
        .col_expr(quota::Column::MemoryBodyBytes, Expr::value(bodies))
        .col_expr(quota::Column::TermRecords, Expr::value(terms))
        .col_expr(quota::Column::MetadataBytes, Expr::value(metadata))
        .col_expr(
            quota::Column::Revision,
            Expr::value(increment(row.revision)?),
        )
        .filter(quota::Column::ActorId.eq(actor))
        .filter(quota::Column::Revision.eq(row.revision))
        .filter(quota::Column::MemoryRecords.eq(row.memory_records))
        .filter(quota::Column::MemoryBodyBytes.eq(row.memory_body_bytes))
        .filter(quota::Column::TermRecords.eq(row.term_records))
        .filter(quota::Column::MetadataBytes.eq(row.metadata_bytes))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

fn project(
    current: i64,
    delta: i64,
    minimum: i64,
    maximum: u64,
) -> Result<i64, AssistantStoreError> {
    if current < minimum {
        return Err(AssistantStoreError::Invalid("invalid memory quota state"));
    }
    let next = current
        .checked_add(delta)
        .filter(|next| *next >= minimum)
        .ok_or(AssistantStoreError::Capacity)?;
    if next > signed(maximum)? && delta > 0 {
        return Err(AssistantStoreError::Capacity);
    }
    Ok(next)
}
fn bytes(slots: usize, enums: usize, texts: &[&str]) -> Result<i64, AssistantStoreError> {
    let initial = slots
        .checked_mul(8)
        .and_then(|size| size.checked_add(enums * 32))
        .ok_or(AssistantStoreError::Capacity)?;
    texts
        .iter()
        .try_fold(initial, |size, text| {
            size.checked_add(text.len())
                .ok_or(AssistantStoreError::Capacity)
        })?
        .try_into()
        .map_err(|_| AssistantStoreError::Capacity)
}
pub fn memory_body(row: &memory::Model) -> Result<i64, AssistantStoreError> {
    bytes(0, 0, &[&row.body, row.context.as_deref().unwrap_or("")])
}
pub fn version_body(row: &revision::Model) -> Result<i64, AssistantStoreError> {
    bytes(0, 0, &[&row.body, row.context.as_deref().unwrap_or("")])
}
pub fn memory_metadata(row: &memory::Model) -> Result<i64, AssistantStoreError> {
    bytes(
        10,
        2,
        &[
            &row.memory_id,
            &row.actor_id,
            &row.scope_key,
            &row.device_ref,
            &row.content_sha256,
        ],
    )
}
pub fn version_metadata(row: &revision::Model) -> Result<i64, AssistantStoreError> {
    bytes(
        9,
        2,
        &[
            &row.memory_id,
            &row.actor_id,
            &row.producer_call_id,
            &row.tool_call_id,
            &row.request_sha256,
            &row.logical_mutation_id,
            &row.content_sha256,
            &row.dependencies_json,
        ],
    )
}
pub fn source_metadata(row: &source::Model) -> Result<i64, AssistantStoreError> {
    bytes(
        12,
        1,
        &[
            &row.source_id,
            &row.memory_id,
            &row.actor_id,
            &row.origin_id,
            &row.content_sha256,
            &row.dependencies_json,
            &row.known_sources_json,
            &row.forget_sources_json,
            &row.retention_proof_json,
            &row.policy_json,
            row.source_device_json.as_deref().unwrap_or(""),
            &row.statement_timezone,
        ],
    )
}
pub fn document_metadata(row: &document::Model) -> Result<i64, AssistantStoreError> {
    bytes(
        12,
        0,
        &[
            &row.document_id,
            &row.actor_id,
            &row.device_ref,
            &row.source_kind,
            &row.source_id,
            &row.origin_id,
            &row.root_conversation_id,
            &row.conversation_id,
            &row.content_sha256,
        ],
    )
}
pub fn term_metadata(row: &term::Model) -> Result<i64, AssistantStoreError> {
    bytes(3, 0, &[&row.actor_id, &row.document_id, &row.term])
}
