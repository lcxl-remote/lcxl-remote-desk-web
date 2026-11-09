//! Native source authorization for the common keyword search engine.

use super::{AssistantStoreError, unsigned};
use crate::entity::{agent_assistant_history as history, agent_assistant_origin as origin};
use desk_agent_protocol::ai_assistant::{
    history::ConversationSearchRequest,
    source::{HistoryDependency, SourceReadPurpose, SourceUsePolicy},
};
use desk_assistant_data_store::keyword_search::{self as shared, HistorySourceReader};
pub(crate) use desk_assistant_data_store::keyword_search::{
    HistorySearchAuthorization, KeywordCandidates, ReadableHistorySource,
};
use desk_diagnose_core::{
    ai_assistant::seam::{AssistantDataReadUse, AssistantReadRoot},
    native_ids::{self, Kind},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use std::collections::BTreeSet;
#[cfg(test)]
use std::time::Duration;
struct NativeHistoryReader;
#[async_trait::async_trait]
impl<C: ConnectionTrait + crate::config::ConfigConnection> HistorySourceReader<C>
    for NativeHistoryReader
{
    async fn validate_context_on(
        db: &C,
        context: &HistorySearchAuthorization<'_>,
    ) -> Result<(), AssistantStoreError> {
        validate_context_on(db, context).await
    }
    async fn readable_history_on(
        db: &C,
        context: &HistorySearchAuthorization<'_>,
        source_id: &str,
        revision: u64,
    ) -> Result<Box<ReadableHistorySource>, AssistantStoreError> {
        readable_history_on(db, context, source_id, revision).await
    }
}
pub(crate) async fn relevance_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    request: &ConversationSearchRequest,
    owner_devices: &BTreeSet<String>,
) -> Result<KeywordCandidates, AssistantStoreError> {
    shared::relevance_on::<C, NativeHistoryReader>(db, context, request, owner_devices).await
}
#[cfg(test)]
pub(super) async fn scan_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    request: &ConversationSearchRequest,
    owner_devices: &BTreeSet<String>,
    time_budget: Duration,
) -> Result<KeywordCandidates, AssistantStoreError> {
    shared::scan_on::<C, NativeHistoryReader>(db, context, request, owner_devices, time_budget)
        .await
}

/// No model output label is created here. This reusable read builds the complete
/// actual dependency closure and passes the existing model-purpose source gate.
pub(crate) async fn readable_history_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    source_id: &str,
    revision: u64,
) -> Result<Box<ReadableHistorySource>, AssistantStoreError> {
    validate_context_on(db, context).await?;
    let row = history::Entity::find()
        .filter(history::Column::ActorId.eq(context.actor_id))
        .filter(history::Column::SourceId.eq(source_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if unsigned(row.revision)? != revision
        || row.admitted_generation <= 0
        || unsigned(row.admitted_generation)? > context.corpus_generation
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let original = origin::Entity::find()
        .filter(origin::Column::ActorId.eq(context.actor_id))
        .filter(origin::Column::OriginId.eq(&row.origin_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if !super::source_lifecycle::origin_readable_on(db, context.actor_id, &original).await? {
        return Err(AssistantStoreError::Inaccessible);
    }
    let policy: SourceUsePolicy = serde_json::from_str(&original.policy_json)
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    let own = HistoryDependency {
        source_id: row.source_id.clone(),
        source_revision: revision,
        origin_id: row.origin_id.clone(),
        origin_deletion_generation: unsigned(original.deletion_generation)?,
        policy_revision: policy.policy_revision,
        expires_at_unix_ms: Some(unsigned(row.expires_at_ms)?),
    };
    let mut dependencies = super::model_sources::decode_dependencies(&row.dependencies_json)?;
    if dependencies.iter().any(|source| {
        source.source_id == own.source_id && source.source_revision == own.source_revision
    }) {
        return Err(AssistantStoreError::Inaccessible);
    }
    dependencies.push(own);
    let expiry = dependencies
        .iter()
        .filter_map(|source| source.expires_at_unix_ms)
        .min();
    let mut usage = AssistantDataReadUse {
        decision_roots: Vec::new(),
        projection_id: native_ids::stable(
            Kind::AssistantReadProjection,
            &(
                "history-candidate-v1",
                context.actor_id,
                &context.scope.root_conversation_id,
                &context.scope.retrieval_episode_id,
                source_id,
                revision,
            ),
        ),
        source_envelope_id: "assistant-history-candidate".into(),
        purpose: SourceReadPurpose::ModelHistory,
        retrieval_episode_id: context.scope.retrieval_episode_id.clone(),
        scope: Box::new(context.assistant_scope.clone()),
        policy_revision: context.access.policy_revision,
        control_generation: context.access.control_generation,
        memory_state_generation: context.access.memory_state_generation,
        roots: vec![AssistantReadRoot {
            source_id: source_id.into(),
            source_revision: revision,
        }],
        history_dependencies: dependencies,
        memories: Vec::new(),
        expires_at_unix_ms: expiry,
    };
    let expiry = super::model_sources::validate_on(
        db,
        context.actor_id,
        context.current_device_ref,
        &usage,
        context.access,
        Some(context.scope),
        context.destination,
        context.platform,
        context.now_unix_ms,
    )
    .await?;
    usage.expires_at_unix_ms = expiry;
    Ok(Box::new(ReadableHistorySource { row, usage }))
}

pub(super) async fn validate_context_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
) -> Result<(), AssistantStoreError> {
    context
        .access
        .validate()
        .map_err(AssistantStoreError::Invalid)?;
    context
        .platform
        .validate()
        .map_err(AssistantStoreError::Invalid)?;
    context
        .scope
        .validate()
        .map_err(AssistantStoreError::Invalid)?;
    if context.actor_id != context.scope.actor_id
        || context.current_device_ref != context.scope.current_device_ref
        || !context.access.settings.model_history_search_enabled
        || context.access.policy_revision != context.platform.revision
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if context.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let runtime = super::reconcile_settings_on(db, context.actor_id).await?;
    let config = db.config_read().await;
    if unsigned(runtime.memory_state_generation)? != context.access.memory_state_generation
        || unsigned(runtime.control_generation)? != context.access.control_generation
        || unsigned(runtime.history_data_generation)? < context.corpus_generation
        || config.assistant_data_settings != context.access.settings
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}
