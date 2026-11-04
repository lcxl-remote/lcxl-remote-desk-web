//! Source verification precedes the metadata commit phase. The original caller
//! retains actor/device/task/session fences and checks the native producer.

use super::{AssistantStoreError, unsigned};
use crate::config::connection::DatabaseTransaction;
use crate::entity::agent_assistant_read_projection as projection;
use desk_agent_protocol::ai_assistant::{
    source::SourceReadPurpose, user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::ai_assistant::{
    data_access::AssistantDataToolAccess,
    read_projection::ReadProjectionProof,
    retrieval_response::ToolReplyBudget,
    seam::{AssistantDataCall, AssistantDataRead, AssistantDataReadUse},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use serde::{Serialize, de::DeserializeOwned};

pub(crate) struct PreparedRetrieval<'a, T> {
    db: &'a DatabaseTransaction,
    context: &'a AssistantDataCall<'a>,
    access: AssistantDataToolAccess,
    result: T,
    content: String,
    proof: Box<ReadProjectionProof>,
    policy: AssistantDataPolicy,
}
pub(crate) struct RetrievalResultInput<'a, T> {
    pub result: T,
    pub purpose: SourceReadPurpose,
    pub selected: &'a [AssistantDataReadUse],
}

/// No quota writes occur here. Final original native SELECT/raw-state fences
/// must still run before store_on. Store the reply before query-cache metadata;
/// no source/native reads may be moved after either final quota mutation.
pub(crate) async fn prepare_on<'a, T: Serialize>(
    db: &'a DatabaseTransaction,
    context: &'a AssistantDataCall<'a>,
    access: &AssistantDataToolAccess,
    policy: &AssistantDataPolicy,
    input: RetrievalResultInput<'_, T>,
    now: u64,
) -> Result<Box<PreparedRetrieval<'a, T>>, AssistantStoreError> {
    fresh_on(db, context, access, policy).await?;
    let content = ToolReplyBudget::new(&context.tool_call.id, context.maximum_response_bytes)
        .map_err(AssistantStoreError::Invalid)?
        .encode(&input.result)
        .map_err(AssistantStoreError::Invalid)?
        .content;
    let mut proof = Box::new(
        ReadProjectionProof::for_retrieval(
            context,
            access,
            &content,
            input.purpose,
            input.selected,
            now,
        )
        .map_err(AssistantStoreError::Invalid)?,
    );
    validate_sources_on(db, context, access, policy, &proof, now).await?;
    let view = desk_diagnose_core::ai_assistant::retained_copy::OwnerHistoryReadValidation::for_prepared_projection(&proof)
        .map_err(AssistantStoreError::Invalid)?;
    proof.snapshot_policy = super::owner_copy::inspect_on(db, &view, now)
        .await?
        .map(|copy| copy.policy);

    fresh_on(db, context, access, policy).await?;
    Ok(Box::new(PreparedRetrieval {
        db,
        context,
        access: access.clone(),
        result: input.result,
        content,
        proof,
        policy: policy.clone(),
    }))
}

/// Revalidate in the same original transaction, then write only metadata. A
/// concurrent logical result is never substituted without its own verification;
/// every failure rolls back the complete caller transaction.
pub(crate) async fn store_on<T>(
    prepared: Box<PreparedRetrieval<'_, T>>,
    policy: &AssistantDataPolicy,
    now: u64,
) -> Result<AssistantDataRead<T>, AssistantStoreError> {
    if &prepared.policy != policy {
        return Err(AssistantStoreError::Inaccessible);
    }
    fresh_on(prepared.db, prepared.context, &prepared.access, policy).await?;
    validate_sources_on(
        prepared.db,
        prepared.context,
        &prepared.access,
        policy,
        &prepared.proof,
        now,
    )
    .await?;
    fresh_on(prepared.db, prepared.context, &prepared.access, policy).await?;
    let stored =
        super::projection::prepare_on(prepared.db, &prepared.proof, &prepared.content, policy, now)
            .await?;
    if stored.proof != prepared.proof
        || stored.row.result_json.as_deref() != Some(prepared.content.as_str())
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(AssistantDataRead {
        result: prepared.result,
        projection: Some(stored.proof),
    })
}

/// Call before scanning or changing a query's delivered-version ledger. A retry
/// returns the original body, with its original deadline and complete source set.
pub(crate) async fn replay_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
    T: DeserializeOwned + Serialize,
>(
    db: &C,
    context: &AssistantDataCall<'_>,
    access: &AssistantDataToolAccess,
    policy: &AssistantDataPolicy,
    purpose: SourceReadPurpose,
    now: u64,
) -> Result<Option<AssistantDataRead<T>>, AssistantStoreError> {
    fresh_on(db, context, access, policy).await?;
    let episode = &context
        .session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?
        .retrieval_budget
        .episode_id;
    let Some(row) = projection::Entity::find()
        .filter(projection::Column::ActorId.eq(&context.session.actor_id))
        .filter(projection::Column::RootConversationId.eq(&context.session.conversation_id))
        .filter(projection::Column::RetrievalEpisodeId.eq(episode))
        .filter(projection::Column::LogicalCallId.eq(context.logical_call_id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let stored = super::projection::decode_row(row)?;
    if stored.row.revoked_at_ms.is_some()
        || stored.row.first_consumed_at_ms.is_some()
        || stored.proof.usage.purpose != purpose
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let content = stored
        .row
        .result_json
        .as_deref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    stored
        .proof
        .validate_retrieval_replay(context, access, content)
        .map_err(AssistantStoreError::Invalid)?;
    validate_sources_on(db, context, access, policy, &stored.proof, now).await?;
    fresh_on(db, context, access, policy).await?;
    let result: T = serde_json::from_str(content)
        .map_err(|_| AssistantStoreError::Invalid("invalid stored assistant response shape"))?;
    if serde_json::to_string(&result)
        .map_err(|_| AssistantStoreError::Invalid("invalid stored assistant response"))?
        != content
    {
        return Err(AssistantStoreError::Invalid(
            "stored assistant response encoding changed",
        ));
    }
    Ok(Some(AssistantDataRead {
        result,
        projection: Some(stored.proof),
    }))
}

async fn validate_sources_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &AssistantDataCall<'_>,
    access: &AssistantDataToolAccess,
    policy: &AssistantDataPolicy,
    proof: &ReadProjectionProof,
    now: u64,
) -> Result<(), AssistantStoreError> {
    let deadline = super::model_sources::validate_session_sources_on(
        db,
        context.session,
        &proof.usage,
        access,
        context.destination,
        policy,
        now,
        None,
    )
    .await?;
    // The label must already contain every current content deadline. Narrowing
    // after hashing would change the frozen result proof and its native binding.
    if deadline != proof.usage.expires_at_unix_ms {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

async fn fresh_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &AssistantDataCall<'_>,
    access: &AssistantDataToolAccess,
    policy: &AssistantDataPolicy,
) -> Result<(), AssistantStoreError> {
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    access.validate().map_err(AssistantStoreError::Invalid)?;
    if context.session.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let state = super::reconcile_settings_on(db, &context.session.actor_id).await?;
    let config = db.config_read().await;
    if state.settings_instance_id.as_deref() != Some(config.metadata.instance.as_str())
        || &config.assistant_data_policy != policy
        || access.policy_revision != policy.revision
        || config.assistant_data_settings != access.settings
        || unsigned(state.memory_state_generation)? != access.memory_state_generation
        || unsigned(state.control_generation)? != access.control_generation
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}
