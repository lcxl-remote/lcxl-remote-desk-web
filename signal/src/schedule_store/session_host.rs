//! OSS session rows and stores for the shared schedule rules. One write
//! transaction serializes writers, so the lock hooks are no-ops.
use super::{ScheduleStore, ScheduleStoreError};
use crate::config::connection::DatabaseTransaction;
use crate::entity::{
    agent_action_item, agent_exec_task, agent_goal_run as goal_row, agent_run_event, agent_session,
};
use desk_assistant_data_store::schedule::session::{
    ActionClass, ActionFilter, ActionView, LockError, NativeDisposition, OrgMode,
    ScheduleHostStores, ScheduleLocks, SessionGuard, SessionKey, SessionPatch, SessionRow,
    SessionRows, TouchColumn,
};
use desk_diagnose_core::{
    file_scope::transaction::{self, FileScopeReceipt},
    session::PersistedAgentSession,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, Select, Set,
    sea_query::Expr,
};

/// The OSS host for shared schedule rules.
pub(crate) struct SignalScheduleHost;

fn row(model: agent_session::Model) -> SessionRow<agent_session::Model> {
    SessionRow {
        id: model.id,
        conversation_id: model.conversation_id.clone(),
        actor_id: model.actor_id.clone(),
        device_id: model.device_id.clone(),
        state_json: model.state_json.clone(),
        version: model.version,
        lease_token: model.lease_token,
        lease_deadline_ms: model.lease_deadline.map(|at| at.timestamp_millis()),
        personal: true,
        turn_state: None,
        raw: model,
    }
}

fn guarded<Q: QueryFilter>(mut query: Q, guard: &SessionGuard<'_>) -> Q {
    if let Some(id) = guard.id {
        query = query.filter(agent_session::Column::Id.eq(id));
    }
    if let Some(conversation) = guard.conversation {
        query = query.filter(agent_session::Column::ConversationId.eq(conversation));
    }
    if let Some(actor) = guard.actor {
        query = query.filter(agent_session::Column::ActorId.eq(actor));
    }
    if let Some(device) = guard.device {
        query = query.filter(agent_session::Column::DeviceId.eq(device));
    }
    if let Some(version) = guard.version {
        query = query.filter(agent_session::Column::Version.eq(version));
    }
    if let Some(token) = guard.lease_token {
        query = query.filter(agent_session::Column::LeaseToken.eq(token));
    }
    if let Some(state) = guard.state_json {
        query = query.filter(agent_session::Column::StateJson.eq(state));
    }
    if guard.lease_deadline_null {
        query = query.filter(agent_session::Column::LeaseDeadline.is_null());
    }
    query
}

#[async_trait::async_trait]
impl<C: ConnectionTrait> ScheduleLocks<C> for SignalScheduleHost {
    async fn lock_conversation(&self, _: &C, _: &str) -> Result<(), DbErr> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl<C: ConnectionTrait> SessionRows<C> for SignalScheduleHost {
    type Row = agent_session::Model;

    async fn find_session(
        &self,
        txn: &C,
        key: SessionKey<'_>,
        _: OrgMode,
        exclusive: bool,
    ) -> Result<Option<SessionRow<agent_session::Model>>, DbErr> {
        let mut query: Select<agent_session::Entity> = match key {
            SessionKey::Id(id) => agent_session::Entity::find_by_id(id),
            SessionKey::Conversation {
                conversation,
                actor,
                device,
            } => {
                let mut query = agent_session::Entity::find()
                    .filter(agent_session::Column::ConversationId.eq(conversation));
                if let Some(actor) = actor {
                    query = query.filter(agent_session::Column::ActorId.eq(actor));
                }
                if let Some(device) = device {
                    query = query.filter(agent_session::Column::DeviceId.eq(device));
                }
                query
            }
        };
        if exclusive {
            query = query.lock_exclusive();
        }
        Ok(query.one(txn).await?.map(row))
    }

    async fn touch_session(
        &self,
        txn: &C,
        column: TouchColumn,
        guard: SessionGuard<'_>,
    ) -> Result<u64, DbErr> {
        let column = match column {
            TouchColumn::Version => agent_session::Column::Version,
            TouchColumn::LeaseToken => agent_session::Column::LeaseToken,
        };
        let update = agent_session::Entity::update_many().col_expr(column, Expr::col(column));
        Ok(guarded(update, &guard).exec(txn).await?.rows_affected)
    }

    async fn write_session(
        &self,
        txn: &C,
        guard: SessionGuard<'_>,
        patch: SessionPatch<'_>,
    ) -> Result<u64, DbErr> {
        let mut update = agent_session::Entity::update_many();
        if let Some(state) = patch.state_json {
            update = update.col_expr(agent_session::Column::StateJson, Expr::value(state));
        }
        if let Some(version) = patch.version {
            update = update.col_expr(agent_session::Column::Version, Expr::value(version));
        }
        if let Some(token) = patch.lease_token {
            update = update.col_expr(agent_session::Column::LeaseToken, Expr::value(token));
        }
        if let Some(deadline) = patch.lease_deadline_ms {
            let deadline = deadline
                .map(chrono::DateTime::from_timestamp_millis)
                .map(|at| at.ok_or_else(|| DbErr::Custom("invalid lease deadline".into())))
                .transpose()?;
            update = update.col_expr(agent_session::Column::LeaseDeadline, Expr::value(deadline));
        }
        if let Some(at) = patch.updated_at_ms {
            let at = chrono::DateTime::from_timestamp_millis(at)
                .ok_or_else(|| DbErr::Custom("invalid update time".into()))?;
            update = update.col_expr(agent_session::Column::UpdatedAt, Expr::value(at));
        }
        Ok(guarded(update, &guard).exec(txn).await?.rows_affected)
    }
}

#[async_trait::async_trait]
impl ScheduleHostStores<DatabaseTransaction> for SignalScheduleHost {
    fn action_class(&self, status: &str) -> ActionClass {
        Self::action_class(status)
    }

    type Action = agent_action_item::Model;
    type NativeScope = crate::assistant_data_store::NativeStatusScope;

    async fn find_actions(
        &self,
        txn: &DatabaseTransaction,
        conversation: &str,
        filter: ActionFilter<'_>,
        exclusive: bool,
    ) -> Result<Vec<ActionView<agent_action_item::Model>>, DbErr> {
        let mut query = action_query(conversation, filter);
        if exclusive {
            query = query.lock_exclusive();
        }
        Ok(query.all(txn).await?.into_iter().map(action_view).collect())
    }

    async fn find_action(
        &self,
        txn: &DatabaseTransaction,
        conversation: &str,
        filter: ActionFilter<'_>,
        exclusive: bool,
    ) -> Result<Option<ActionView<agent_action_item::Model>>, DbErr> {
        let mut query = action_query(conversation, filter);
        if exclusive {
            query = query.lock_exclusive();
        }
        Ok(query.one(txn).await?.map(action_view))
    }

    async fn verify_manual_outbox(
        &self,
        txn: &DatabaseTransaction,
        action: &ActionView<agent_action_item::Model>,
        execution_id: &str,
        requested: bool,
    ) -> Result<(), ScheduleStoreError> {
        use crate::entity::agent_capability_dispatch_outbox as outbox_row;
        let outbox = outbox_row::Entity::find()
            .filter(outbox_row::Column::WorkId.eq(action.id))
            .filter(outbox_row::Column::DispatchId.eq(execution_id))
            .lock_exclusive()
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::Conflict)?;
        let payload: crate::capability_grant_store::CapabilityDispatchPayload =
            serde_json::from_str(&outbox.payload_json).map_err(|_| ScheduleStoreError::Invalid)?;
        if payload.work_id != action.id
            || payload.dispatch_id != execution_id
            || (requested
                && outbox.state != crate::capability_grant_store::DISPATCH_OUTBOX_OUTCOME_UNKNOWN)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(())
    }

    async fn resolve_unknown_action(
        &self,
        txn: &DatabaseTransaction,
        action: &ActionView<agent_action_item::Model>,
        at_ms: i64,
    ) -> Result<bool, DbErr> {
        let at = chrono::DateTime::from_timestamp_millis(at_ms)
            .ok_or_else(|| DbErr::Custom("invalid resolution time".into()))?;
        let changed = agent_action_item::Entity::update_many()
            .set(agent_action_item::ActiveModel {
                manual_resolved_at: Set(Some(at)),
                updated_at: Set(at),
                ..Default::default()
            })
            .filter(agent_action_item::Column::Id.eq(action.id))
            .filter(
                agent_action_item::Column::Status
                    .eq(crate::capability_grant_store::CAPABILITY_WORK_OUTCOME_UNKNOWN),
            )
            .exec(txn)
            .await?;
        Ok(changed.rows_affected == 1)
    }

    async fn list_task_grants(
        &self,
        txn: &DatabaseTransaction,
        session: &PersistedAgentSession,
    ) -> Result<Vec<desk_agent_protocol::capability_grant::CapabilityGrant>, DbErr> {
        crate::capability_grant_store::SignalCapabilityGrantStore::list_for_subject_on(
            txn,
            &session.conversation_id,
            &session.actor_id,
            &session.device_id,
        )
        .await
    }

    async fn close_fresh_wait_decision(
        &self,
        txn: &DatabaseTransaction,
        session: &PersistedAgentSession,
        request_id: &str,
        now_ms: i64,
    ) -> Result<(), LockError> {
        let now = desk_assistant_data_store::schedule::session::timestamp(now_ms)
            .map_err(|_| LockError::Denied)?;
        crate::agent_session_store::permission_resume::close_fresh_wait_decision_on(
            txn, session, request_id, now,
        )
        .await
        .map_err(|_| LockError::Denied)
    }

    async fn restore_completed_calls(
        &self,
        txn: &DatabaseTransaction,
        work: &crate::entity::agent_schedule_run::Model,
        session: &mut PersistedAgentSession,
        now_ms: i64,
    ) -> Result<bool, ScheduleStoreError> {
        let now = desk_assistant_data_store::schedule::session::timestamp(now_ms)?;
        let context = super::TaskReceiptContext::load(txn, work).await?;
        Ok(
            crate::capability_grant_store::fresh_recovery::restore_completed_calls(
                txn,
                session,
                &context,
                &now.to_rfc3339(),
            )
            .await?,
        )
    }

    async fn open_native_scope(
        &self,
        txn: &DatabaseTransaction,
        actor: &str,
        device: &str,
        root: &str,
    ) -> Result<Option<Box<Self::NativeScope>>, DbErr> {
        crate::assistant_data_store::native_scope_on(txn, actor, device, root).await
    }

    async fn capture_native_scope(
        &self,
        txn: &DatabaseTransaction,
        scope: &Self::NativeScope,
    ) -> Result<(), DbErr> {
        scope.capture_on(txn).await
    }

    fn action_matches_identity(
        &self,
        action: &ActionView<agent_action_item::Model>,
        identity: &desk_diagnose_core::session::ActionIdentity,
    ) -> bool {
        use desk_diagnose_core::session::WorkKind;
        match identity.kind {
            WorkKind::ComputerAction => identity.work_id == action.id,
            WorkKind::CapabilityProvider => {
                identity.work_id == action.id
                    && identity.action_request_id == action.raw.action_request_id
            }
            WorkKind::AgentExec => identity.action_request_id == action.raw.action_request_id,
            _ => false,
        }
    }

    async fn has_open_exec_tasks(
        &self,
        txn: &DatabaseTransaction,
        conversation: &str,
        session: &PersistedAgentSession,
        allow_result: bool,
    ) -> Result<bool, DbErr> {
        use crate::entity::agent_exec_task;
        let executions = agent_exec_task::Entity::find()
            .filter(agent_exec_task::Column::ConversationId.eq(conversation))
            .all(txn)
            .await?;
        Ok(executions.iter().any(|execution| {
            execution.status != "done"
                && !(allow_result
                    && session.execution_state.tasks().into_iter().any(|action| {
                        action.kind == desk_diagnose_core::session::WorkKind::AgentExec
                            && action.work_id == execution.id
                            && action.action_request_id == execution.exec_request_id
                            && action.execution_id == execution.execution_generation
                    }))
        }))
    }

    async fn reconcile_scheduled_actions(
        &self,
        txn: &DatabaseTransaction,
        session: &mut PersistedAgentSession,
        now_ms: i64,
    ) -> Result<(), LockError> {
        let now = u64::try_from(now_ms).map_err(|_| LockError::Denied)?;
        crate::capability_grant_store::scheduled_recovery::reconcile_on(txn, session, now)
            .await
            .map_err(LockError::Backend)
    }

    async fn settle_scheduled_decision(
        &self,
        txn: &DatabaseTransaction,
        session: &PersistedAgentSession,
        now_ms: i64,
    ) -> Result<(), LockError> {
        let now = desk_assistant_data_store::schedule::session::timestamp(now_ms)
            .map_err(|_| LockError::Denied)?;
        crate::agent_session_store::permission_resume::settle_scheduled_decision_on(
            txn, session, now,
        )
        .await
        .map_err(|_| LockError::Denied)
    }

    async fn settle_fresh_decision(
        &self,
        txn: &DatabaseTransaction,
        session: &PersistedAgentSession,
        request_id: &str,
        now_ms: i64,
    ) -> Result<(), LockError> {
        let now = desk_assistant_data_store::schedule::session::timestamp(now_ms)
            .map_err(|_| LockError::Denied)?;
        crate::agent_session_store::permission_resume::settle_fresh_decision_on(
            txn, session, request_id, now,
        )
        .await
        .map_err(|_| LockError::Denied)
    }

    async fn lock_task_subject(
        &self,
        _: &DatabaseTransaction,
        _: i32,
        _: &str,
        _: &str,
    ) -> Result<(), ScheduleStoreError> {
        Ok(())
    }

    async fn lock_root_children(
        &self,
        _: &DatabaseTransaction,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), DbErr> {
        Ok(())
    }

    async fn scheduled_native(
        &self,
        txn: &DatabaseTransaction,
        group: &crate::entity::agent_delegation_group::Model,
    ) -> Result<NativeDisposition, DbErr> {
        use crate::agent_subagent_store::ScheduledNativeDisposition as Native;
        Ok(
            match crate::agent_subagent_store::scheduled_native_on(txn, group).await? {
                Native::Settled => NativeDisposition::Settled,
                Native::Pending => NativeDisposition::Pending,
                Native::Unknown => NativeDisposition::Unknown,
            },
        )
    }

    async fn required_children_complete(
        &self,
        txn: &DatabaseTransaction,
        session: &PersistedAgentSession,
    ) -> Result<bool, DbErr> {
        crate::agent_subagent_store::required_children_complete_on(txn, session).await
    }

    async fn directory_receipt(
        &self,
        txn: &DatabaseTransaction,
        session: &PersistedAgentSession,
        request_id: &str,
    ) -> Result<FileScopeReceipt, ScheduleStoreError> {
        let subject = session
            .file_scope_subject(
                &session.actor_id,
                &session.device_id,
                &session.conversation_id,
            )
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let stored = agent_run_event::Entity::find()
            .filter(
                agent_run_event::Column::EventId
                    .eq(transaction::receipt_event_id(&subject, request_id)),
            )
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let receipt: FileScopeReceipt =
            serde_json::from_str(&stored.payload_json).map_err(|_| ScheduleStoreError::Invalid)?;
        if stored.kind != transaction::FILE_SCOPE_EVENT_KIND
            || stored.payload_schema_version != 1
            || stored.run_id != session.conversation_id
            || stored.event_id != receipt.event_id
            || i64::try_from(receipt.event_seq).ok() != Some(stored.event_seq)
            || receipt.update.subject != subject
            || receipt.update.client_request_id != request_id
            || stored.actor_id.as_deref() != Some(session.actor_id.as_str())
            || stored.correlation_id.as_deref() != Some(request_id)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(receipt)
    }

    async fn lock_child_source(
        &self,
        txn: &DatabaseTransaction,
        session: &PersistedAgentSession,
    ) -> Result<(), DbErr> {
        crate::agent_subagent_store::lock_child_source_on(txn, session).await
    }

    async fn owns_device(&self, _: &DatabaseTransaction, _: i32, _: &str) -> Result<bool, DbErr> {
        Ok(true)
    }

    /// Settled actions and finished exec tasks only; unresolved effects block.
    async fn has_open_actions(
        &self,
        txn: &DatabaseTransaction,
        conversation: &str,
    ) -> Result<bool, DbErr> {
        let actions = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::ConversationId.eq(conversation))
            .lock_exclusive()
            .all(txn)
            .await?;
        Ok(actions.iter().any(|action| {
            !matches!(
                action.status.as_str(),
                "done"
                    | "rejected"
                    | "expired"
                    | "cancelled"
                    | "capability_succeeded"
                    | "capability_failed"
                    | "capability_superseded_before_intent"
                    | "capability_revoked_before_intent"
            )
        }) || agent_exec_task::Entity::find()
            .filter(agent_exec_task::Column::ConversationId.eq(conversation))
            .filter(agent_exec_task::Column::Status.ne("done"))
            .lock_exclusive()
            .one(txn)
            .await?
            .is_some())
    }

    async fn close_wait_decision(
        &self,
        txn: &DatabaseTransaction,
        session: &PersistedAgentSession,
        request_id: &str,
        now_ms: i64,
    ) -> Result<(), ScheduleStoreError> {
        let timestamp = desk_assistant_data_store::schedule::session::timestamp(now_ms)?;
        crate::agent_session_store::permission_resume::close_wait_decision_on(
            txn, session, request_id, timestamp,
        )
        .await
        .map_err(|_| ScheduleStoreError::Conflict)
    }

    async fn lock_permission_owner(
        &self,
        _: &DatabaseTransaction,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), LockError> {
        Ok(())
    }

    async fn consume_scheduled_decision(
        &self,
        txn: &DatabaseTransaction,
        session: &PersistedAgentSession,
        request_id: &str,
        grants: &[desk_agent_protocol::capability_grant::CapabilityGrant],
        now_ms: i64,
    ) -> Result<(), LockError> {
        let now = desk_assistant_data_store::schedule::session::timestamp(now_ms)
            .map_err(|_| LockError::Denied)?;
        crate::agent_session_store::permission_resume::consume_scheduled_decision_on(
            txn, session, request_id, grants, now,
        )
        .await
        .map_err(|_| LockError::Denied)
    }

    async fn assistant_data_limits(
        &self,
        txn: &DatabaseTransaction,
    ) -> Result<Box<desk_agent_protocol::ai_assistant::limits::AssistantDataLimits>, DbErr> {
        Ok(crate::assistant_data_config::policy(txn).await.limits)
    }

    async fn initialize_continuation_group(
        &self,
        txn: &DatabaseTransaction,
        session: &mut PersistedAgentSession,
        now_ms: i64,
    ) -> Result<(), DbErr> {
        crate::agent_subagent_store::initialize_continuation_group_on(txn, session, now_ms).await
    }

    async fn active_goal_conflict(
        &self,
        txn: &DatabaseTransaction,
        conversation: &str,
        owner: i32,
        device: Option<&str>,
    ) -> Result<bool, DbErr> {
        let mut query = goal_row::Entity::find()
            .filter(goal_row::Column::ConversationId.eq(conversation))
            .filter(goal_row::Column::ActorId.eq(owner.to_string()))
            .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]));
        if let Some(device) = device {
            query = query.filter(goal_row::Column::DeviceId.eq(device));
        }
        Ok(query.one(txn).await?.is_some())
    }

    async fn create_draft(
        &self,
        txn: &DatabaseTransaction,
        owner: i32,
        request: &desk_agent_protocol::schedule::ScheduleDraft,
        now_ms: i64,
        original: Option<&PersistedAgentSession>,
    ) -> Result<super::entity::Model, ScheduleStoreError> {
        ScheduleStore::create_draft_on(txn, owner, request, now_ms, original).await
    }

    async fn record_prompt_retention(
        &self,
        txn: &DatabaseTransaction,
        task: &mut super::entity::Model,
        session: &PersistedAgentSession,
        parent: &desk_diagnose_core::chat::ChatMessage,
    ) -> Result<(), ScheduleStoreError> {
        ScheduleStore::record_prompt_retention_on(txn, task, session, parent).await
    }

    async fn validate_model_control(
        &self,
        txn: &DatabaseTransaction,
        session: &PersistedAgentSession,
        call: &desk_diagnose_core::chat::ToolCall,
    ) -> Result<(), DbErr> {
        crate::model_egress_store::SignalModelEgressStore::validate_control_on(txn, session, call)
            .await
    }

    async fn open_for_schedule_proposal(
        &self,
        txn: &DatabaseTransaction,
        row: &agent_session::Model,
        session: &mut PersistedAgentSession,
        now_ms: u64,
    ) -> Result<bool, DbErr> {
        crate::agent_approval_store::open_for_schedule_proposal_on(txn, row, session, now_ms).await
    }

    async fn check_resume_retention(
        &self,
        txn: &DatabaseTransaction,
        spec: &desk_agent_protocol::schedule::ScheduleSpec,
        now_ms: i64,
    ) -> Result<(), ScheduleStoreError> {
        super::check_resume_retention(txn, spec, now_ms).await
    }

    async fn owner_prompt_retention(
        &self,
        txn: &DatabaseTransaction,
        actor: &str,
        device: &str,
    ) -> Result<String, ScheduleStoreError> {
        ScheduleStore::owner_prompt_retention_on(txn, actor, device).await
    }
}

impl SignalScheduleHost {
    /// Raw statuses of a class that a lookup can exclude in SQL.
    fn class_statuses(class: ActionClass) -> &'static [&'static str] {
        use crate::capability_grant_store::{
            CAPABILITY_WORK_DISPATCHING, CAPABILITY_WORK_FAILED, CAPABILITY_WORK_OUTCOME_UNKNOWN,
            CAPABILITY_WORK_REVOKED, CAPABILITY_WORK_SUCCEEDED, CAPABILITY_WORK_SUPERSEDED,
        };
        match class {
            ActionClass::Succeeded => &[CAPABILITY_WORK_SUCCEEDED],
            ActionClass::Failed => &[CAPABILITY_WORK_FAILED],
            ActionClass::Withdrawn => &[CAPABILITY_WORK_SUPERSEDED, CAPABILITY_WORK_REVOKED],
            ActionClass::LegacyClosed => &["done", "rejected", "expired", "cancelled"],
            ActionClass::OutcomeUnknown => &[CAPABILITY_WORK_OUTCOME_UNKNOWN],
            ActionClass::Dispatching => &[CAPABILITY_WORK_DISPATCHING],
            ActionClass::Open => unreachable!("open statuses are not enumerable"),
        }
    }

    /// Action-item status classes. Rows from the earlier action vocabulary
    /// (done, rejected, expired, cancelled) are `LegacyClosed`.
    pub(crate) fn action_class(status: &str) -> ActionClass {
        use crate::capability_grant_store::{
            CAPABILITY_WORK_DISPATCHING, CAPABILITY_WORK_FAILED, CAPABILITY_WORK_OUTCOME_UNKNOWN,
            CAPABILITY_WORK_REVOKED, CAPABILITY_WORK_SUCCEEDED, CAPABILITY_WORK_SUPERSEDED,
        };
        match status {
            CAPABILITY_WORK_SUCCEEDED => ActionClass::Succeeded,
            CAPABILITY_WORK_FAILED => ActionClass::Failed,
            CAPABILITY_WORK_SUPERSEDED | CAPABILITY_WORK_REVOKED => ActionClass::Withdrawn,
            "done" | "rejected" | "expired" | "cancelled" => ActionClass::LegacyClosed,
            CAPABILITY_WORK_OUTCOME_UNKNOWN => ActionClass::OutcomeUnknown,
            CAPABILITY_WORK_DISPATCHING => ActionClass::Dispatching,
            _ => ActionClass::Open,
        }
    }
}

fn action_query(conversation: &str, filter: ActionFilter<'_>) -> Select<agent_action_item::Entity> {
    let mut query = agent_action_item::Entity::find()
        .filter(agent_action_item::Column::ConversationId.eq(conversation));
    if let Some(kind) = filter.kind {
        query = query.filter(agent_action_item::Column::Kind.eq(kind));
    }
    if let Some(class) = filter.exclude {
        query = query.filter(
            agent_action_item::Column::Status
                .is_not_in(SignalScheduleHost::class_statuses(class).iter().copied()),
        );
    }
    if filter.not_cancel_requested {
        query = query.filter(agent_action_item::Column::CancelRequestedAt.is_null());
    }
    query
}

fn action_view(model: agent_action_item::Model) -> ActionView<agent_action_item::Model> {
    ActionView {
        id: model.id,
        kind: model.kind.clone(),
        class: SignalScheduleHost::action_class(&model.status),
        conversation_id: model.conversation_id.clone(),
        turn_id: model.turn_id.clone(),
        actor_id: model.actor_id.clone(),
        target_device_id: model.target_device_id.clone(),
        action_request_id: Some(model.action_request_id.clone()),
        execution_id: model.execution_id.clone(),
        tool_call_id: model.tool_call_id.clone(),
        has_result: model.result_json.is_some(),
        is_side_effecting: model.is_side_effecting,
        dispatch_intent_at_ms: model.dispatch_intent_at.map(|at| at.timestamp_millis()),
        manual_resolved_at_ms: model.manual_resolved_at.map(|at| at.timestamp_millis()),
        raw: model,
    }
}

/// Applies a shared rule's commit or rollback decision to its transaction.
pub(super) async fn finish<T>(
    txn: crate::config::connection::DatabaseTransaction,
    decision: desk_assistant_data_store::schedule::session::Finish<T>,
) -> Result<T, ScheduleStoreError> {
    use desk_assistant_data_store::schedule::session::Finish;
    match decision {
        Finish::Commit(value) => {
            txn.commit().await?;
            Ok(value)
        }
        Finish::Rollback(value) => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn action(db: &crate::config::connection::DatabaseConnection, id: i64, status: &str) {
        use sea_orm::ActiveModelTrait;
        let now = chrono::Utc::now();
        agent_action_item::ActiveModel {
            id: Set(id),
            kind: Set(crate::capability_grant_store::CAPABILITY_WORK_KIND.into()),
            action_request_id: Set(format!("request-{id}")),
            conversation_id: Set("run".into()),
            turn_id: Set("turn".into()),
            tool_call_id: Set(format!("call-{id}")),
            actor_id: Set("1".into()),
            target_device_id: Set("device".into()),
            status: Set(status.into()),
            attempt: Set(1),
            draft_hash: Set("hash".into()),
            policy_revision: Set(1),
            is_side_effecting: Set(true),
            payload_json: Set("{}".into()),
            payload_schema_version: Set(1),
            completion_event_id: Set(format!("event-{id}")),
            completion_delivery_state: Set("pending".into()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn action_lookups_exclude_a_class_in_sql_and_classify_rows() {
        use crate::capability_grant_store::*;
        use sea_orm::TransactionTrait;
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        action(&db, 1, CAPABILITY_WORK_SUCCEEDED).await;
        action(&db, 2, CAPABILITY_WORK_FAILED).await;
        action(&db, 3, "done").await;
        let txn = db.begin().await.unwrap();
        let host = SignalScheduleHost;
        let unsucceeded = host
            .find_actions(
                &txn,
                "run",
                ActionFilter {
                    exclude: Some(ActionClass::Succeeded),
                    ..ActionFilter::default()
                },
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            unsucceeded
                .iter()
                .map(|action| (action.id, action.class))
                .collect::<Vec<_>>(),
            [(2, ActionClass::Failed), (3, ActionClass::LegacyClosed)]
        );
        let first = host
            .find_action(
                &txn,
                "run",
                ActionFilter {
                    exclude: Some(ActionClass::Succeeded),
                    ..ActionFilter::default()
                },
                true,
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.class, ActionClass::Succeeded);
        assert_eq!(
            host.find_actions(&txn, "other", ActionFilter::default(), false)
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn every_action_status_maps_to_its_class() {
        use crate::capability_grant_store::*;
        for (status, class) in [
            (CAPABILITY_WORK_SUCCEEDED, ActionClass::Succeeded),
            (CAPABILITY_WORK_FAILED, ActionClass::Failed),
            (CAPABILITY_WORK_SUPERSEDED, ActionClass::Withdrawn),
            (CAPABILITY_WORK_REVOKED, ActionClass::Withdrawn),
            ("done", ActionClass::LegacyClosed),
            ("rejected", ActionClass::LegacyClosed),
            ("expired", ActionClass::LegacyClosed),
            ("cancelled", ActionClass::LegacyClosed),
            (CAPABILITY_WORK_OUTCOME_UNKNOWN, ActionClass::OutcomeUnknown),
            (CAPABILITY_WORK_DISPATCHING, ActionClass::Dispatching),
            (CAPABILITY_WORK_REVIEW_CLOSED, ActionClass::Open),
            (CAPABILITY_WORK_PREPARED, ActionClass::Open),
            (CAPABILITY_WORK_INTENT_RECORDED, ActionClass::Open),
            ("approved", ActionClass::Open),
            ("dispatched", ActionClass::Open),
            ("unknown", ActionClass::Open),
        ] {
            assert_eq!(SignalScheduleHost::action_class(status), class, "{status}");
        }
    }
}
