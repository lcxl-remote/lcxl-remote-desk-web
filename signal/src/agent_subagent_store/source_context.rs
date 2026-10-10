//! Frozen child input survives compression; reading it grants no execution authority.
use super::*;
use crate::config::connection::DatabaseTransaction;
use desk_diagnose_core::subagent::creation::TaskCreationEnvelope;

/// Callers hold owner, root and child control before using this in an authority
/// transaction. Read-only callers may use one coherent snapshot without a lease.
pub(crate) async fn task_context_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    session: &PersistedAgentSession,
) -> Result<Option<(TaskCreationEnvelope, chrono::DateTime<chrono::Utc>)>, DbErr> {
    let Some(binding) = session.agent_role.binding() else {
        return Ok(None);
    };
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&binding.task_id))
        .filter(run_row::Column::ChildConversationId.eq(&session.conversation_id))
        .filter(run_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(run_row::Column::ActorId.eq(&session.actor_id))
        .filter(run_row::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let run = decode_run(&row)?;
    run.validate_session(session).map_err(|_| invalid())?;
    let source = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&binding.group_id))
        .filter(group_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(group_row::Column::ActorId.eq(&session.actor_id))
        .filter(group_row::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let group = decode_group(&source)?;
    let creation: TaskCreationEnvelope =
        serde_json::from_str(&row.creation_envelope_json).map_err(|_| invalid())?;
    creation.validate_task(binding).map_err(|_| invalid())?;
    if session.delegated_owner_requirement.as_ref() != Some(&creation.source.owner_requirement)
        || creation.source != decode_creation(&source)?
        || group.source != binding.source
        || group.source_epoch != binding.source_epoch
        || creation.response_locale != session.response_locale
    {
        return Err(invalid());
    }
    let created_at = chrono::DateTime::parse_from_rfc3339(&run.created_at)
        .map_err(|_| invalid())?
        .with_timezone(&chrono::Utc);
    Ok(Some((creation, created_at)))
}

/// An unclaimed real decision must retain its dedicated continuation path.
pub(crate) async fn child_pending_decision_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    session: &PersistedAgentSession,
) -> Result<bool, DbErr> {
    if session.agent_role.is_main() {
        return Ok(false);
    }
    use crate::entity::agent_permission_resume as permission;
    Ok(permission::Entity::find()
        .filter(permission::Column::RunId.eq(&session.conversation_id))
        .filter(permission::Column::ActorId.eq(&session.actor_id))
        .filter(permission::Column::DeviceId.eq(&session.device_id))
        .filter(
            permission::Column::InputRevision
                .eq(i64::try_from(session.input_revision).map_err(|_| invalid())?),
        )
        .filter(permission::Column::State.eq("pending"))
        .one(db)
        .await?
        .is_some())
}

/// A saved approval may be inspected while paused, but only an open original
/// source may claim planning. This check never consumes the permission decision.
pub(crate) async fn child_resume_admitted_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    session: &PersistedAgentSession,
    now_ms: i64,
) -> Result<bool, DbErr> {
    let Some(binding) = session.agent_role.binding() else {
        return Ok(!session.main_stopped);
    };
    task_context_on(db, session).await?.ok_or_else(invalid)?;
    if !check_child_permission_on(db, session, now_ms).await? {
        return Ok(false);
    }
    let run = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&binding.task_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let run = decode_run(&run)?;
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&binding.group_id))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    let group = decode_group(&group)?;
    let charged = group
        .budget
        .charged
        .checked_add(group.budget.outstanding)
        .map_err(|_| invalid())?;
    let children = group
        .budget
        .child_charged
        .checked_add(group.budget.child_outstanding)
        .map_err(|_| invalid())?;
    Ok(!run.source_paused
        && group.source_admission == SourceAdmission::Open
        && group.limits.total.has_model_capacity(charged)
        && group.limits.child_ceiling().has_model_capacity(children))
}

/// Called after begin_turn in the same approval/session transaction. Task rows
/// receive status only; the existing child session remains the sole lease owner.
pub(crate) async fn record_child_resume_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    now_ms: i64,
) -> Result<(), DbErr> {
    let Some(binding) = session.agent_role.binding() else {
        return Ok(());
    };
    if !child_resume_admitted_on(txn, session, now_ms).await? {
        return Err(invalid());
    }
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&binding.task_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let mut run = decode_run(&row)?;
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&binding.group_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let group = decode_group(&group)?;
    let facts = super::facts::runtime_facts_on(txn, session, now_ms).await?;
    let now = chrono::DateTime::from_timestamp_millis(now_ms)
        .ok_or_else(invalid)?
        .to_rfc3339();
    if run
        .synchronize_dependencies(facts.dependencies, true, &now)
        .map_err(|_| invalid())?
    {
        replace_run_on(txn, &row, &run, now_ms).await?;
        append_state_event_on(txn, &group, &run, now_ms).await?;
    }
    Ok(())
}

impl SubAgentStore {
    /// A terminal child can receive native facts but cannot retain a model wake.
    pub async fn child_is_terminal(&self, session: &PersistedAgentSession) -> Result<bool, DbErr> {
        let Some(binding) = session.agent_role.binding() else {
            return Ok(false);
        };
        let row = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&binding.task_id))
            .filter(run_row::Column::RootConversationId.eq(&binding.root_conversation_id))
            .filter(run_row::Column::ChildConversationId.eq(&session.conversation_id))
            .filter(run_row::Column::ActorId.eq(&session.actor_id))
            .filter(run_row::Column::DeviceId.eq(&session.device_id))
            .one(&self.db)
            .await?
            .ok_or_else(invalid)?;
        let run = decode_run(&row)?;
        run.validate_session(session).map_err(|_| invalid())?;
        Ok(run.state.is_terminal() || run.state == SubAgentState::Cancelling)
    }

    pub async fn child_resume_available(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<bool, DbErr> {
        let txn = self.db.begin().await?;
        let result =
            child_resume_admitted_on(&txn, session, chrono::Utc::now().timestamp_millis()).await?;
        txn.commit().await?;
        Ok(result)
    }

    pub async fn child_context_for_subject(
        &self,
        conversation_id: &str,
        actor_id: &str,
        device_id: &str,
        expected_version: i64,
    ) -> Result<Option<(TaskCreationEnvelope, PersistedAgentSession)>, DbErr> {
        let txn = self.db.begin().await?;
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(conversation_id))
            .filter(session_row::Column::ActorId.eq(actor_id))
            .filter(session_row::Column::DeviceId.eq(device_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
        if row.version != expected_version || session.version != row.version {
            return Err(invalid());
        }
        let result = task_context_on(&txn, &session)
            .await?
            .map(|(creation, _)| (creation, session));
        txn.commit().await?;
        Ok(result)
    }

    pub async fn child_creation_context(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<Option<TaskCreationEnvelope>, DbErr> {
        let txn = self.db.begin().await?;
        let result = task_context_on(&txn, session)
            .await?
            .map(|(creation, _)| creation);
        txn.commit().await?;
        Ok(result)
    }
}
