//! Complete model-purpose fact reads. Owner formatting follows authorization;
//! neither an owner DTO nor an index hit grants permission to send its body.

use super::{AssistantStoreError, unsigned};
use crate::config::connection::DatabaseTransaction;
use crate::entity::agent_memory as memory;
use desk_agent_protocol::ai_assistant::{
    history::AssistantDeviceScope,
    memory::{MemoryRecord, MemoryUsePurpose},
    source::{HistoryDependency, SourceReadPurpose},
};
use desk_diagnose_core::{
    ai_assistant::{
        data_control::CurrentMemoryUse,
        retrieval_query::QuerySourceVersion,
        seam::{AssistantDataReadUse, AssistantReadRoot},
    },
    native_ids::{self, Kind},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use std::collections::BTreeMap;

pub(crate) use desk_assistant_data_store::memory_keyword::{
    MemoryReadAuthorization, readable_statuses,
};

pub(super) struct NativeMemoryReader;
#[async_trait::async_trait]
impl<C: ConnectionTrait + crate::config::ConfigConnection>
    desk_assistant_data_store::memory_keyword::MemorySourceReader<C> for NativeMemoryReader
{
    async fn validate_context_on(
        db: &C,
        context: &MemoryReadAuthorization<'_>,
    ) -> Result<(), AssistantStoreError> {
        validate_context_on(db, context).await
    }
    async fn authorize_version_on(
        db: &C,
        context: &MemoryReadAuthorization<'_>,
        version: &QuerySourceVersion,
        purpose: MemoryUsePurpose,
    ) -> Result<(), AssistantStoreError> {
        read_on(db, context, version, purpose).await?;
        Ok(())
    }
}
#[async_trait::async_trait]
impl<C: ConnectionTrait + crate::config::ConfigConnection>
    desk_assistant_data_store::memory_history::MemoryHistoryReader<C> for NativeMemoryReader
{
    async fn read_version_on(
        db: &C,
        context: &MemoryReadAuthorization<'_>,
        version: &desk_assistant_data_store::entity::agent_memory_revision::Model,
    ) -> Result<Box<AuthorizedMemoryHistory>, AssistantStoreError> {
        desk_diagnose_core::future::send_phase(|| read_history_on(db, context, version)).await
    }
}

/// The personal entry exposes the selected request's devices to memory tools.
/// A fixed-device entry keeps its existing owner-wide retrieval authority.
pub(super) async fn retrieval_devices_on(
    db: &DatabaseTransaction,
    fence: &super::retrieval_native::NativeAssistantDataFence<'_>,
    now: u64,
) -> Result<std::collections::BTreeSet<String>, AssistantStoreError> {
    let session = fence.context.session;
    let scope = desk_diagnose_core::ai_assistant::memory_access::usable_memory_scope(session, now)
        .ok_or(AssistantStoreError::Inaccessible)?;
    if scope.request_device_refs.is_empty() {
        super::conversation_search::owner_devices_on(db, fence).await
    } else {
        Ok(super::current_memory::fact_devices_on(db, session, scope)
            .await?
            .into_iter()
            .take(fence.policy.limits.retrieval.episode.query_devices as usize)
            .collect())
    }
}

pub(crate) struct AuthorizedMemoryView {
    pub row: memory::Model,
    pub record: MemoryRecord,
    pub usage: AssistantDataReadUse,
}

pub(crate) async fn read_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &MemoryReadAuthorization<'_>,
    version: &QuerySourceVersion,
    purpose: MemoryUsePurpose,
) -> Result<Box<AuthorizedMemoryView>, AssistantStoreError> {
    validate_context_on(db, context).await?;
    let row = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(context.actor_id))
        .filter(memory::Column::MemoryId.eq(&version.source_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if row.revision <= 0
        || !readable_statuses(context).contains(&row.status.as_str())
        || unsigned(row.revision)? != version.revision
        || row.content_sha256 != version.content_sha256
        || row.state_generation <= 0
        || unsigned(row.state_generation)? > context.observation_generation
        || (!row.device_ref.is_empty()
            && (!context.requested_devices.contains(&row.device_ref)
                || ((purpose == MemoryUsePurpose::CurrentFact
                    || context.scope.device_rule == AssistantDeviceScope::CurrentDevice)
                    && !context
                        .scope
                        .current_device_refs()
                        .contains(&row.device_ref))))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let usage = desk_diagnose_core::future::send_phase(|| {
        read_proof_on(db, context, &row, version, purpose)
    })
    .await?;
    let record = super::memory::owner_record_on(db, context.actor_id, &row, context.now_unix_ms)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    // Formatting is a separate read. Re-read the exact body row afterward so
    // neither transaction reuse nor an unchanged revision hides a raw change.
    let current = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(context.actor_id))
        .filter(memory::Column::MemoryId.eq(&version.source_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if current != row {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Box::new(AuthorizedMemoryView { row, record, usage }))
}

async fn read_proof_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &MemoryReadAuthorization<'_>,
    row: &memory::Model,
    version: &QuerySourceVersion,
    purpose: MemoryUsePurpose,
) -> Result<AssistantDataReadUse, AssistantStoreError> {
    let reference = CurrentMemoryUse {
        memory_id: row.memory_id.clone(),
        revision: version.revision,
        purpose,
    };
    let (sources, producer, until) = super::model_sources::memory_sources_on(
        db,
        context.actor_id,
        if purpose == MemoryUsePurpose::CurrentFact {
            &row.device_ref
        } else {
            context.current_device_ref
        },
        &reference,
        context.now_unix_ms,
    )
    .await?;
    let mut dependencies = BTreeMap::new();
    for source in producer {
        add_dependency(&mut dependencies, source)?;
    }
    let mut roots = Vec::new();
    let mut expiry = until;
    for source in sources {
        let (own, parents, until) = super::model_sources::evidence_on(
            db,
            context.actor_id,
            &source,
            false,
            context.destination,
            context.now_unix_ms,
        )
        .await?;
        roots.push(AssistantReadRoot {
            source_id: own.source_id.clone(),
            source_revision: own.source_revision,
        });
        expiry = earlier(expiry, until);
        for dependency in std::iter::once(own).chain(parents) {
            add_dependency(&mut dependencies, dependency)?;
        }
    }
    roots.sort_by(|a, b| (&a.source_id, a.source_revision).cmp(&(&b.source_id, b.source_revision)));
    expiry = earlier(
        expiry,
        dependencies
            .values()
            .filter_map(|source| source.expires_at_unix_ms)
            .min(),
    );
    let projection_id = native_ids::stable(
        Kind::AssistantReadProjection,
        &(
            "memory-candidate-v1",
            context.actor_id,
            &context.scope.root_conversation_id,
            &context.scope.retrieval_episode_id,
            &row.memory_id,
            version.revision,
            purpose,
        ),
    );
    let mut usage = AssistantDataReadUse {
        projection_id,
        source_envelope_id: "assistant-memory-candidate".into(),
        purpose: SourceReadPurpose::ModelMemory,
        retrieval_episode_id: context.scope.retrieval_episode_id.clone(),
        scope_sha256: context.scope_sha256.into(),
        policy_revision: context.access.policy_revision,
        control_generation: context.access.control_generation,
        memory_state_generation: context.access.memory_state_generation,
        roots,
        decision_roots: Vec::new(),
        history_dependencies: dependencies.into_values().collect(),
        memories: vec![reference],
        expires_at_unix_ms: expiry,
    };
    usage.expires_at_unix_ms = if purpose == MemoryUsePurpose::CurrentFact
        || !context.scope.request_device_refs.is_empty()
    {
        super::model_sources::validate_memory_devices_on(
            db,
            context.actor_id,
            context.requested_devices,
            &usage,
            context.access,
            context.destination,
            context.platform,
            context.now_unix_ms,
        )
        .await?
    } else {
        super::model_sources::validate_on(
            db,
            context.actor_id,
            context.current_device_ref,
            &usage,
            context.access,
            None,
            context.destination,
            context.platform,
            context.now_unix_ms,
        )
        .await?
    };
    // Pending values are historical control candidates, not current facts.
    // Their ended truth interval is distinct from retained-body eligibility.

    Ok(usage)
}

pub(crate) use desk_assistant_data_store::memory_history::AuthorizedMemoryHistory;

pub(crate) async fn read_history_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &MemoryReadAuthorization<'_>,
    original: &desk_assistant_data_store::entity::agent_memory_revision::Model,
) -> Result<Box<AuthorizedMemoryHistory>, AssistantStoreError> {
    use desk_agent_protocol::ai_assistant::memory::{MemoryHistoryEntry, MemoryStatus};
    use desk_assistant_data_store::entity::agent_memory_revision as revision;
    validate_context_on(db, context).await?;
    let head = desk_assistant_data_store::memory_history::head_on(db, context, &original.memory_id)
        .await?;
    let version = QuerySourceVersion {
        source_id: original.memory_id.clone(),
        revision: unsigned(original.revision)?,
        content_sha256: original.content_sha256.clone(),
    };
    let usage = desk_diagnose_core::future::send_phase(|| {
        read_proof_on(
            db,
            context,
            &head,
            &version,
            MemoryUsePurpose::HistoricalReference,
        )
    })
    .await?;
    let stored = revision::Entity::find()
        .filter(revision::Column::ActorId.eq(context.actor_id))
        .filter(revision::Column::MemoryId.eq(&version.source_id))
        .filter(revision::Column::Revision.eq(original.revision))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let current = memory::Entity::find_by_id(head.id)
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if &stored != original || current != head {
        return Err(AssistantStoreError::Inaccessible);
    }
    let status = match stored.status.as_str() {
        "active" => MemoryStatus::Active,
        "pending_conflict" => MemoryStatus::PendingConflict,
        "superseded" => MemoryStatus::Superseded,
        _ => return Err(AssistantStoreError::Inaccessible),
    };
    let entry = MemoryHistoryEntry {
        revision: version.revision,
        content: stored.body,
        context: stored.context,
        status,
        recorded_at_unix_ms: unsigned(stored.created_at_ms)?,
        source_ids: usage
            .roots
            .iter()
            .map(|root| root.source_id.clone())
            .collect(),
    };
    validate_context_on(db, context).await?;
    Ok(Box::new(AuthorizedMemoryHistory {
        version,
        entry,
        usage,
    }))
}

pub(crate) async fn validate_context_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &MemoryReadAuthorization<'_>,
) -> Result<(), AssistantStoreError> {
    validate_context_with_runtime_on(db, context, None).await
}

async fn validate_context_with_runtime_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &MemoryReadAuthorization<'_>,
    owned: Option<(
        &crate::entity::agent_assistant_runtime::Model,
        &crate::entity::agent_assistant_runtime::Model,
    )>,
) -> Result<(), AssistantStoreError> {
    context
        .scope
        .validate()
        .map_err(AssistantStoreError::Invalid)?;
    context
        .access
        .validate()
        .map_err(AssistantStoreError::Invalid)?;
    context
        .platform
        .validate()
        .map_err(AssistantStoreError::Invalid)?;
    if context.actor_id != context.scope.actor_id
        || context.current_device_ref != context.scope.current_device_ref
        || !context.access.settings.automatic_memory_enabled
        || context.access.policy_revision != context.platform.revision
        || context
            .scope
            .expires_at_unix_ms
            .is_some_and(|until| until <= context.now_unix_ms)
        || context.now_unix_ms < context.scope.accepted_at_unix_ms
        || context.observation_generation == 0
        || context.observation_generation > context.access.memory_state_generation
        || context.requested_devices.len()
            > context.platform.limits.retrieval.episode.query_devices as usize
        || context.scope_sha256.len() != 64
        || !context
            .scope_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let runtime = super::runtime_on(db, context.actor_id).await?;
    let valid_runtime = match owned {
        Some((original, expected)) => {
            &runtime == expected
                && original.actor_id == context.actor_id
                && unsigned(original.memory_state_generation)?
                    == context.access.memory_state_generation
                && unsigned(original.control_generation)? == context.access.control_generation
        }
        None => {
            unsigned(runtime.memory_state_generation)? == context.access.memory_state_generation
                && unsigned(runtime.control_generation)? == context.access.control_generation
        }
    };
    if !valid_runtime || super::settings_on(db, context.actor_id).await? != context.access.settings
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    // Do not hold a live read guard while requesting another configuration
    // read: a queued writer must not make this reader wait on itself.
    let config = db.config_read().await;
    if runtime.settings_instance_id.as_deref() != Some(config.metadata.instance.as_str())
        || runtime.settings_revision_seen != super::signed(config.assistant_data_settings.revision)?
        || config.assistant_data_settings != context.access.settings
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

impl AuthorizedMemoryView {
    pub(crate) fn version(&self) -> Result<QuerySourceVersion, AssistantStoreError> {
        Ok(QuerySourceVersion {
            source_id: self.row.memory_id.clone(),
            revision: unsigned(self.row.revision)?,
            content_sha256: self.row.content_sha256.clone(),
        })
    }
}

fn add_dependency(
    dependencies: &mut BTreeMap<(String, u64), HistoryDependency>,
    source: HistoryDependency,
) -> Result<(), AssistantStoreError> {
    let key = (source.source_id.clone(), source.source_revision);
    if let Some(previous) = dependencies.get(&key) {
        if previous != &source {
            return Err(AssistantStoreError::Inaccessible);
        }
    } else {
        dependencies.insert(key, source);
    }
    Ok(())
}
fn earlier(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
