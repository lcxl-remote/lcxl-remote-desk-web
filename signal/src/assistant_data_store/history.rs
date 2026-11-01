//! History capture is the final savepoint of an already-authorized business transaction.

use crate::entity::agent_assistant_runtime as runtime;
use crate::entity::{
    agent_assistant_history as history, agent_assistant_history_conversation as conversation,
    agent_assistant_history_coverage as coverage, agent_assistant_origin as origin,
    agent_assistant_quota as quota, agent_assistant_search_document as document,
    agent_assistant_search_term as term,
};
use desk_agent_protocol::ai_assistant::{
    history::HistoryAdmission, user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::ai_assistant::source::{
    HistoryCapture, PreparedHistoryCapture, prepare_capture,
};
use desk_diagnose_core::native_ids::{self, Kind};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, QueryFilter,
    QuerySelect, Set, TransactionTrait, sea_query::OnConflict,
};

use super::{AssistantStoreError, ensure_quota_on, increment, signed};
use crate::config::{ConfigConnection, connection::DatabaseTransaction};
use crate::entity::agent_assistant_device_binding as binding;

pub(crate) mod staged;

pub(crate) struct CaptureOutcome {
    pub admission: HistoryAdmission,
    #[cfg_attr(not(test), allow(dead_code))]
    pub source_ids: Vec<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub partial: bool,
}

pub(crate) async fn capture_user_input_on(
    txn: &DatabaseTransaction,
    session: &desk_diagnose_core::session::PersistedAgentSession,
    message: &desk_diagnose_core::chat::ChatMessage,
    recorded_at_unix_ms: u64,
) -> Result<(), AssistantStoreError> {
    use desk_agent_protocol::ai_assistant::source::AssistantSourceKind;
    if session.surface != desk_diagnose_core::session::AgentSessionSurface::AiAssistant
        || session.agent_role.binding().is_some()
    {
        return Ok(());
    }
    if session.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let data = session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Invalid(
            "missing assistant input data state",
        ))?;
    if data.temporary {
        return Ok(());
    }
    let config = txn.config_read().await;
    let source_device_ref = native_ids::stable(
        Kind::AssistantDeviceBinding,
        &(&config.metadata.instance, &session.device_id),
    );
    if data.source_device_ref != source_device_ref
        || data.source_instance_id.as_deref() != Some(config.metadata.instance.as_str())
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let source_policy = desk_diagnose_core::ai_assistant::source::owner_text_policy(
        config.assistant_data_policy.revision,
        false,
    );
    let envelope = message
        .data_envelope
        .as_ref()
        .ok_or(AssistantStoreError::Invalid(
            "missing input source envelope",
        ))?;
    let capture = HistoryCapture {
        native: None,
        actor_id: &session.actor_id,
        device_ref: &source_device_ref,
        directory: Some(
            desk_diagnose_core::ai_assistant::source::HistorySourceDirectory {
                instance_id: &config.metadata.instance,
                audience: &session.device_id,
                device_name: None,
            },
        ),
        conversation_id: &session.conversation_id,
        root_conversation_id: &session.conversation_id,
        native_id: &message.message_id,
        kind: AssistantSourceKind::Input,
        role: "user",
        accepted_input_seq: data.current_actor_input_seq,
        content: &message.text,
        envelope,
        source_policy: &source_policy,
        dependencies: &[],
        recorded_at_unix_ms,
        statement_timezone: &data.current_input_timezone,
        revision: 1,
    };
    let pending = staged::prepare_many_on(txn, &[&capture], &config.assistant_data_policy).await?;
    pending.finalize_on().await?;
    Ok(())
}

enum AdmissionFailure {
    Capacity,
    Policy,
    Storage(AssistantStoreError),
}
impl From<sea_orm::DbErr> for AdmissionFailure {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Storage(error.into())
    }
}
impl From<AssistantStoreError> for AdmissionFailure {
    fn from(error: AssistantStoreError) -> Self {
        Self::Storage(error)
    }
}

/// The caller must not issue any original business SELECT or write after this call.
#[cfg(test)]
pub(crate) async fn capture_on(
    txn: &DatabaseTransaction,
    capture: &HistoryCapture<'_>,
    policy: &AssistantDataPolicy,
) -> Result<CaptureOutcome, AssistantStoreError> {
    let prepared = staged::prepare_many_on(txn, &[capture], policy).await?;
    prepared
        .finalize_on()
        .await?
        .pop()
        .ok_or(AssistantStoreError::Invalid(
            "missing history capture outcome",
        ))
}

async fn record_gap_on(
    txn: &DatabaseTransaction,
    capture: &HistoryCapture<'_>,
    policy: &AssistantDataPolicy,
    reason: &str,
) -> Result<(), AssistantStoreError> {
    record_scope_gap_on(
        txn,
        capture.actor_id,
        capture.recorded_at_unix_ms,
        policy,
        reason,
    )
    .await
}

/// The caller has authenticated an eligible original native scope. A coverage
/// gap contains no invented source body, input sequence, envelope or permission.
pub(crate) async fn record_scope_gap_on(
    txn: &DatabaseTransaction,
    actor: &str,
    recorded_at: u64,
    policy: &AssistantDataPolicy,
    reason: &str,
) -> Result<(), AssistantStoreError> {
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    desk_agent_protocol::ai_assistant::source::validate_source_id(actor)
        .map_err(AssistantStoreError::Invalid)?;
    if !matches!(
        reason,
        "omitted_policy" | "omitted_capacity" | "omitted_error"
    ) || recorded_at == 0
    {
        return Err(AssistantStoreError::Invalid(
            "invalid native history coverage gap",
        ));
    }
    let savepoint = txn.begin().await?;
    let until = recorded_at
        .checked_add(u64::from(policy.history_retention_days) * 86_400_000)
        .ok_or(AssistantStoreError::Invalid("invalid coverage expiry"))?;
    ensure_quota_on(&savepoint, actor).await?;
    ensure_coverage_slots_on(&savepoint, actor, signed(until)?).await?;
    update_actor_coverage_on(&savepoint, actor, recorded_at, signed(until)?, reason, 1).await?;
    savepoint.commit().await?;
    Ok(())
}

async fn ensure_coverage_slots_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    expires: i64,
) -> Result<(), AssistantStoreError> {
    // Fixed actor-level aggregates consume the quota row's metadata reserve.
    // They conservatively report affected intervals; they never claim exact coverage.
    for reason in [
        "indexed",
        "partial",
        "omitted_capacity",
        "omitted_policy",
        "omitted_error",
    ] {
        coverage::Entity::insert(coverage::ActiveModel {
            actor_id: Set(actor.into()),
            conversation_id: Set(String::new()),
            day_bucket: Set(0),
            reason: Set(reason.into()),
            earliest_recorded_at_ms: Set(0),
            latest_recorded_at_ms: Set(0),
            indexed_through_ms: Set(None),
            source_count: Set(0),
            revision: Set(1),
            expires_at_ms: Set(expires),
            ..Default::default()
        })
        .on_conflict(
            OnConflict::columns([
                coverage::Column::ActorId,
                coverage::Column::ConversationId,
                coverage::Column::DayBucket,
                coverage::Column::Reason,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(db)
        .await?;
    }
    Ok(())
}

async fn update_coverage_on<C: ConnectionTrait>(
    db: &C,
    capture: &HistoryCapture<'_>,
    expires: i64,
    reason: &str,
    count: i64,
) -> Result<(), AssistantStoreError> {
    update_actor_coverage_on(
        db,
        capture.actor_id,
        capture.recorded_at_unix_ms,
        expires,
        reason,
        count,
    )
    .await
}

async fn update_actor_coverage_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    recorded_at: u64,
    expires: i64,
    reason: &str,
    count: i64,
) -> Result<(), AssistantStoreError> {
    let row = coverage::Entity::find()
        .filter(coverage::Column::ActorId.eq(actor))
        .filter(coverage::Column::ConversationId.eq(""))
        .filter(coverage::Column::DayBucket.eq(0))
        .filter(coverage::Column::Reason.eq(reason))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Invalid(
            "missing reserved history coverage slot",
        ))?;
    let now = signed(recorded_at)?;
    let mut active: coverage::ActiveModel = row.clone().into();
    active.earliest_recorded_at_ms = Set(if row.source_count == 0 {
        now
    } else {
        row.earliest_recorded_at_ms.min(now)
    });
    active.latest_recorded_at_ms = Set(row.latest_recorded_at_ms.max(now));
    active.source_count = Set(row
        .source_count
        .checked_add(count)
        .ok_or(AssistantStoreError::Invalid("coverage counter exhausted"))?);
    active.revision = Set(increment(row.revision)?);
    active.expires_at_ms = Set(row.expires_at_ms.max(expires));
    if reason == "indexed" {
        active.indexed_through_ms = Set(Some(row.indexed_through_ms.unwrap_or(0).max(now)));
    }
    active.update(db).await?;
    Ok(())
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, AssistantStoreError> {
    serde_json::to_string(value)
        .map_err(|_| AssistantStoreError::Invalid("invalid assistant history metadata"))
}
fn enum_string<T: serde::Serialize>(value: &T) -> Result<String, AssistantStoreError> {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or(AssistantStoreError::Invalid("invalid history source kind"))
}
fn add(current: i64, delta: i64) -> Result<i64, AssistantStoreError> {
    if current < 0 || delta < 0 {
        return Err(AssistantStoreError::Invalid(
            "invalid assistant quota state",
        ));
    }
    current
        .checked_add(delta)
        .ok_or(AssistantStoreError::Invalid("assistant quota exhausted"))
}
