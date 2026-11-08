//! Owner-only fact management reads. Model-purpose retrieval uses a separate gate.

use super::{AssistantStoreError, ensure_runtime_on, runtime_on, unsigned};
use crate::{
    config::ConfigConnection,
    entity::{
        agent_assistant_device_binding as binding, agent_memory as memory,
        agent_memory_revision as revision, agent_memory_source as source,
    },
};
use desk_agent_protocol::ai_assistant::{
    memory::{MemoryListRequest, MemoryListResult, MemoryRecord, MemoryScope, MemoryStatus},
    source::AssistantSourceDevice,
};
use desk_diagnose_core::native_ids::{self, Kind};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    QueryTrait, TransactionTrait,
};

pub(crate) async fn memories(
    db: &crate::config::connection::DatabaseConnection,
    request: &MemoryListRequest,
    now: u64,
) -> Result<MemoryListResult, AssistantStoreError> {
    request.validate().map_err(AssistantStoreError::Invalid)?;
    let txn = db.begin().await?;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    ensure_runtime_on(&txn, &actor).await?;
    let result = list_on(&txn, &actor, request, now).await?;
    txn.commit().await?;
    Ok(result)
}

pub(crate) async fn list_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    request: &MemoryListRequest,
    now: u64,
) -> Result<MemoryListResult, AssistantStoreError> {
    let state = runtime_on(db, actor).await?;
    let generation = unsigned(state.memory_state_generation)?;
    let control = unsigned(state.control_generation)?;
    if request
        .memory_state_generation
        .is_some_and(|seen| seen != generation)
        || request
            .control_generation
            .is_some_and(|seen| seen != control)
    {
        return Err(AssistantStoreError::Stale {
            current_revision: generation,
        });
    }
    let config = db.config_read().await;
    let valid_devices = binding::Entity::find()
        .filter(binding::Column::InstanceId.eq(&config.metadata.instance))
        .select_only()
        .column(binding::Column::BindingId)
        .into_query();
    let allowed_scopes = Condition::any()
        .add(
            Condition::all()
                .add(memory::Column::ScopeKey.eq("personal"))
                .add(memory::Column::DeviceRef.eq("")),
        )
        .add(memory::Column::DeviceRef.in_subquery(valid_devices));
    let mut statuses = vec!["active", "pending_conflict"];
    if request.include_superseded {
        statuses.push("superseded");
    }
    let mut query = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(actor))
        .filter(memory::Column::Status.is_in(statuses))
        .filter(allowed_scopes);
    if let Some(scope) = request.scope {
        query = match scope {
            MemoryScope::Personal => query.filter(memory::Column::ScopeKey.eq("personal")),
            MemoryScope::Device => query.filter(memory::Column::DeviceRef.ne("")),
        };
    }
    if let Some(cursor) = &request.cursor {
        if !native_ids::is(Kind::Memory, cursor) {
            return Err(AssistantStoreError::Invalid("invalid memory continuation"));
        }
        query = query.filter(memory::Column::MemoryId.gt(cursor));
    }
    let mut rows = query
        .order_by_asc(memory::Column::MemoryId)
        .limit(u64::from(request.limit) + 1)
        .all(db)
        .await?;
    let has_more = rows.len() > request.limit as usize;
    rows.truncate(request.limit as usize);
    let next_cursor = has_more
        .then(|| rows.last().map(|row| row.memory_id.clone()))
        .flatten();
    let mut memories = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(record) = owner_record_on(db, actor, &row, now).await? {
            memories.push(record);
        }
    }
    Ok(MemoryListResult {
        memories,
        next_cursor,
        memory_state_generation: generation,
        control_generation: control,
    })
}

pub(crate) async fn owner_record_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    row: &memory::Model,
    now: u64,
) -> Result<Option<MemoryRecord>, AssistantStoreError> {
    if row.actor_id != actor
        || !native_ids::is(Kind::Memory, &row.memory_id)
        || row.revision <= 0
        || row.body.is_empty()
    {
        return Ok(None);
    }
    let status = match row.status.as_str() {
        "active" => MemoryStatus::Active,
        "pending_conflict" => MemoryStatus::PendingConflict,
        "superseded" => MemoryStatus::Superseded,
        _ => return Ok(None),
    };
    let (scope, source_device) = if row.scope_key == "personal" && row.device_ref.is_empty() {
        (MemoryScope::Personal, None)
    } else {
        let Some(source_device) = source_device_on(db, actor, &row.device_ref).await? else {
            return Ok(None);
        };
        if row.scope_key != format!("device:{}", row.device_ref) {
            return Ok(None);
        }
        (MemoryScope::Device, Some(source_device))
    };
    let Some(current_revision) = revision::Entity::find()
        .filter(revision::Column::ActorId.eq(actor))
        .filter(revision::Column::MemoryId.eq(&row.memory_id))
        .filter(revision::Column::Revision.eq(row.revision))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    if current_revision.body != row.body
        || current_revision.context != row.context
        || current_revision.status != row.status
        || current_revision.accepted_input_seq != row.accepted_input_seq
        || current_revision.state_generation != row.state_generation
    {
        return Ok(None);
    }
    let rows = source::Entity::find()
        .filter(source::Column::ActorId.eq(actor))
        .filter(source::Column::MemoryId.eq(&row.memory_id))
        .filter(source::Column::MemoryRevision.eq(row.revision))
        .order_by_asc(source::Column::SourceId)
        .all(db)
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut source_ids = Vec::with_capacity(rows.len());
    let mut has_assertion = false;
    for evidence in rows {
        if !evidence_allowed_on(db, actor, &evidence, now).await? {
            return Ok(None);
        }
        has_assertion |= evidence.role == "evidence";
        source_ids.push(evidence.source_id);
    }
    if !has_assertion {
        return Ok(None);
    }
    Ok(Some(MemoryRecord {
        memory_id: row.memory_id.clone(),
        revision: unsigned(row.revision)?,
        scope,
        source_device,

        content: row.body.clone(),
        context: row.context.clone(),
        status,
        source_ids,
        observed_at_unix_ms: unsigned(row.observed_at_ms)?,
        updated_at_unix_ms: unsigned(row.updated_at_ms)?,
        valid_from_unix_ms: unsigned(row.valid_from_ms)?,
        valid_until_unix_ms: row.valid_until_ms.map(unsigned).transpose()?,
    }))
}

pub(crate) async fn evidence_allowed_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    row: &source::Model,
    now: u64,
) -> Result<bool, AssistantStoreError> {
    desk_diagnose_core::future::boxed(|| super::evidence::allowed_on(db, actor, row, now)).await
}

pub(crate) async fn source_device_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    device_ref: &str,
) -> Result<Option<AssistantSourceDevice>, AssistantStoreError> {
    if device_ref.is_empty() {
        return Ok(None);
    }
    if actor != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let config = db.config_read().await;
    let row = binding::Entity::find()
        .filter(binding::Column::InstanceId.eq(&config.metadata.instance))
        .filter(binding::Column::BindingId.eq(device_ref))
        .one(db)
        .await?;
    Ok(row
        .filter(|row| {
            native_ids::stable(
                Kind::AssistantDeviceBinding,
                &(&row.instance_id, &row.audience),
            ) == row.binding_id
        })
        .map(|row| AssistantSourceDevice {
            device_ref: row.binding_id,
            recorded_name: row.device_name.clone(),
            current_name: row.device_name,
            historical: true,
        }))
}
