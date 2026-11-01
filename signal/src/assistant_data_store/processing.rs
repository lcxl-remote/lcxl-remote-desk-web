//! Bounded owner metadata from the original history, budget and recovery stores.
use super::{AssistantStoreError, signed, unsigned};
use crate::config::ConfigConnection;
use crate::entity::{
    agent_assistant_history_coverage as coverage, agent_assistant_quota as quota,
    agent_assistant_runtime as runtime,
};
use desk_agent_protocol::ai_assistant::{
    processing::*,
    user_instructions::{AssistantDataPolicy, AssistantDataSettings},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

pub(crate) async fn read_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    _instance: Option<&str>,
    _settings: &AssistantDataSettings,
    policy: &AssistantDataPolicy,
    now: u64,
) -> Result<AssistantProcessingStatus, AssistantStoreError> {
    let runtime = runtime::Entity::find()
        .filter(runtime::Column::ActorId.eq(actor))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let rows = coverage::Entity::find()
        .filter(coverage::Column::ActorId.eq(actor))
        .filter(coverage::Column::ExpiresAtMs.gt(signed(now)?))
        .order_by_desc(coverage::Column::LatestRecordedAtMs)
        .order_by_desc(coverage::Column::Id)
        .limit(u64::from(policy.limits.processing.status_coverage) + 1)
        .all(db)
        .await?;
    let partial = rows.len() > policy.limits.processing.status_coverage as usize;
    let coverage = rows
        .into_iter()
        .take(policy.limits.processing.status_coverage as usize)
        .map(|row| {
            Ok(AssistantHistoryCoverageStatus {
                conversation_id: (!row.conversation_id.is_empty()).then_some(row.conversation_id),
                reason: row.reason,
                source_count: unsigned(row.source_count)?,
                first_recorded_at_unix_ms: unsigned(row.earliest_recorded_at_ms)?,
                last_recorded_at_unix_ms: unsigned(row.latest_recorded_at_ms)?,
                indexed_through_unix_ms: row.indexed_through_ms.map(unsigned).transpose()?,
            })
        })
        .collect::<Result<Vec<_>, AssistantStoreError>>()?;
    let counters = quota::Entity::find()
        .filter(quota::Column::ActorId.eq(actor))
        .one(db)
        .await?;
    let stored = counters
        .as_ref()
        .map(|row| unsigned(row.history_records))
        .transpose()?
        .unwrap_or(0);
    let terms = counters
        .as_ref()
        .map(|row| unsigned(row.term_records))
        .transpose()?
        .unwrap_or(0);
    Ok(AssistantProcessingStatus {
        observed_at_unix_ms: now,
        history_retention_days: policy.history_retention_days,
        history_generation: unsigned(runtime.history_data_generation)?,
        stored_history_fragments: stored,
        indexed_terms: terms,
        coverage,
        partial,
    })
}

pub(crate) async fn processing(
    db: &crate::config::connection::DatabaseConnection,
) -> Result<AssistantProcessingStatus, AssistantStoreError> {
    use sea_orm::TransactionTrait;
    let txn = db.begin().await?;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    super::ensure_runtime_on(&txn, &actor).await?;
    let config = txn.config_read().await;
    let now = unsigned(chrono::Utc::now().timestamp_millis())?;
    let result = desk_diagnose_core::future::boxed(|| {
        read_on(
            &txn,
            &actor,
            Some(config.metadata.instance.as_str()),
            &config.assistant_data_settings,
            &config.assistant_data_policy,
            now,
        )
    })
    .await?;
    txn.commit().await?;
    Ok(result)
}
