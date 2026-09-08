//! Single-instance durable permission continuation claims and maintenance.
use super::*;
use crate::entity::agent_permission_resume as resume;
use desk_diagnose_core::{dynamic_run::PermissionDecidedEvent, session::TriggerOrigin};
use sea_orm::{ConnectionTrait, DatabaseTransaction};

#[derive(Clone)]
pub(super) struct ClaimBinding {
    request_id: String,
    expected_version: i64,
    grants: Vec<desk_agent_protocol::capability_grant::CapabilityGrant>,
}

fn invalid() -> AgentError {
    internal("Original permission continuation is inconsistent")
}
fn storage(_: sea_orm::DbErr) -> AgentError {
    transport("Permission continuation storage is unavailable; retry")
}

pub fn turn_id(run_id: &str, request_id: &str) -> String {
    let mut digest = Sha256::new();
    for value in [run_id, request_id] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    format!("permission-resume-{:x}", digest.finalize())
}

pub(super) async fn insert_pending(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
    event: &PermissionDecidedEvent,
) -> Result<(), AgentError> {
    resume::ActiveModel {
        permission_id: Set(turn_id(&session.conversation_id, &event.request_id)),
        decision_event_id: Set(event.event.event_id.clone()),
        run_id: Set(session.conversation_id.clone()),
        request_id: Set(event.request_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        device_id: Set(session.device_id.clone()),
        input_revision: Set(i64::try_from(event.request_input_revision).map_err(|_| invalid())?),
        state: Set("pending".into()),
        turn_id: Set(None),
        version: Set(1),
        created_at: Set(now_from(&event.event.created_at)),
        updated_at: Set(now_from(&event.event.created_at)),
        ..Default::default()
    }
    .insert(db)
    .await
    .map_err(storage)?;
    Ok(())
}

async fn record(
    db: &impl ConnectionTrait,
    session: &PersistedAgentSession,
    request_id: &str,
) -> Result<(resume::Model, PermissionDecidedEvent), AgentError> {
    let row = resume::Entity::find()
        .filter(resume::Column::PermissionId.eq(turn_id(&session.conversation_id, request_id)))
        .one(db)
        .await
        .map_err(storage)?
        .ok_or_else(invalid)?;
    let decision = permission_receipt::decided_on(db, session, request_id)
        .await?
        .ok_or_else(invalid)?;
    if row.run_id != session.conversation_id
        || row.request_id != request_id
        || row.actor_id != session.actor_id
        || row.device_id != session.device_id
        || row.decision_event_id != decision.event.event_id
        || row.input_revision
            != i64::try_from(decision.request_input_revision).map_err(|_| invalid())?
        || row.created_at
            != DateTime::parse_from_rfc3339(&decision.event.created_at)
                .map_err(|_| invalid())?
                .with_timezone(&Utc)
        || row.version < 1
        || !match (row.state.as_str(), row.turn_id.as_deref()) {
            ("pending" | "superseded", None) => true,
            ("started" | "settled", Some(id)) => {
                id == row.permission_id
                    || (session.trigger_origin == TriggerOrigin::ScheduledTask
                        && id == format!("{}-turn", session.conversation_id))
            }
            _ => false,
        }
    {
        return Err(invalid());
    }
    Ok((row, decision))
}

async fn transition(
    db: &DatabaseTransaction,
    row: &resume::Model,
    state: &str,
    started: bool,
    now: DateTime<Utc>,
) -> Result<(), AgentError> {
    let result = resume::Entity::update_many()
        .col_expr(resume::Column::State, Expr::value(state))
        .col_expr(
            resume::Column::TurnId,
            Expr::value(started.then(|| row.permission_id.clone())),
        )
        .col_expr(
            resume::Column::Version,
            Expr::value(row.version.checked_add(1).ok_or_else(invalid)?),
        )
        .col_expr(resume::Column::UpdatedAt, Expr::value(now))
        .filter(resume::Column::Id.eq(row.id))
        .filter(resume::Column::Version.eq(row.version))
        .filter(resume::Column::State.eq(&row.state))
        .exec(db)
        .await
        .map_err(storage)?;
    if result.rows_affected != 1 {
        return Err(transport("Permission continuation claim conflicted"));
    }
    Ok(())
}

fn current_decision(
    session: &PersistedAgentSession,
    decision: &PermissionDecidedEvent,
) -> Result<(), AgentError> {
    if !session.permission_requests.iter().any(|request| {
        request.request_id == decision.request_id
            && request.input_revision == decision.request_input_revision
            && request.state == decision.resulting_state
    }) {
        return Err(invalid());
    }
    Ok(())
}

impl SignalAgentSessionStore {
    /// Read a scheduled decision and its grants coherently without recovering
    /// or claiming either lease. The occurrence claim rechecks this snapshot.
    pub(crate) async fn pending_scheduled_permission(
        &self,
        run_id: &str,
        request_id: &str,
        device_id: &str,
    ) -> Result<
        Option<(
            PersistedAgentSession,
            Vec<desk_agent_protocol::capability_grant::CapabilityGrant>,
        )>,
        AgentError,
    > {
        let txn = self.db.begin().await.map_err(storage)?;
        let row = find(&txn, run_id)
            .await
            .map_err(storage)?
            .ok_or_else(invalid)?;
        let session = permission_receipt::session(
            &row,
            run_id,
            &crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string(),
            device_id,
        )?;
        if session.trigger_origin != TriggerOrigin::ScheduledContinuation
            || session.turn_state.is_active()
        {
            return Ok(None);
        }
        let (receipt, decision) = record(&txn, &session, request_id).await?;
        if receipt.state != "pending" || session.input_revision != decision.request_input_revision {
            return Ok(None);
        }
        current_decision(&session, &decision)?;
        desk_diagnose_core::assistant_policy::require_current_policy(session.policy_revision)?;
        let grants =
            crate::capability_grant_store::SignalCapabilityGrantStore::list_for_subject_on(
                &txn,
                run_id,
                &session.actor_id,
                device_id,
            )
            .await
            .map_err(storage)?;
        txn.commit().await.map_err(storage)?;
        Ok(Some((session, grants)))
    }

    pub fn with_permission_resume(
        mut self,
        request_id: String,
        expected_version: i64,
        grants: Vec<desk_agent_protocol::capability_grant::CapabilityGrant>,
    ) -> Self {
        self.permission_resume = Some(ClaimBinding {
            request_id,
            expected_version,
            grants,
        });
        self
    }

    pub async fn permission_resume_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<resume::Model>, AgentError> {
        if after_id < 0 || limit == 0 || limit > 256 {
            return Err(invalid());
        }
        resume::Entity::find()
            .filter(resume::Column::Id.gt(after_id))
            .filter(resume::Column::State.is_in(["pending", "started"]))
            .order_by_asc(resume::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(storage)
    }

    /// Scan rows are hints only. Recheck identity before recovering an original
    /// lease, then read the decision and session together in a fresh transaction.
    pub async fn pending_permission_resume(
        &self,
        candidate: &resume::Model,
        now: DateTime<Utc>,
    ) -> Result<Option<PersistedAgentSession>, AgentError> {
        let row = find(&self.db, &candidate.run_id)
            .await
            .map_err(storage)?
            .ok_or_else(invalid)?;
        let session = permission_receipt::session(
            &row,
            &candidate.run_id,
            &candidate.actor_id,
            &candidate.device_id,
        )?;
        // Only the scheduled executor may pair this session with its occurrence.
        if matches!(
            session.trigger_origin,
            TriggerOrigin::ScheduledContinuation | TriggerOrigin::ScheduledTask
        ) {
            return Ok(None);
        }
        let (original, _) = record(&self.db, &session, &candidate.request_id).await?;
        if original.id != candidate.id || original.permission_id != candidate.permission_id {
            return Err(invalid());
        }
        if (original.state == "started" && session.current_turn_id == original.turn_id)
            || (original.state == "pending"
                && i64::try_from(session.input_revision).ok() == Some(original.input_revision))
        {
            self.settle_lapsed_session(&row, now).await?;
        }
        let txn = self.db.begin().await.map_err(storage)?;
        let row = find(&txn, &candidate.run_id)
            .await
            .map_err(storage)?
            .ok_or_else(invalid)?;
        let session = permission_receipt::session(
            &row,
            &candidate.run_id,
            &candidate.actor_id,
            &candidate.device_id,
        )?;
        // Only the scheduled executor may pair this session with its occurrence.
        if matches!(
            session.trigger_origin,
            TriggerOrigin::ScheduledContinuation | TriggerOrigin::ScheduledTask
        ) {
            return Ok(None);
        }
        let (resume, decision) = record(&txn, &session, &candidate.request_id).await?;
        if resume.state == "started" {
            if session.current_turn_id.as_deref() != resume.turn_id.as_deref()
                || !session.turn_state.is_active()
            {
                transition(&txn, &resume, "settled", true, now).await?;
            }
            txn.commit().await.map_err(storage)?;
            return Ok(None);
        }
        if resume.state != "pending" {
            return Ok(None);
        }
        if session.input_revision != decision.request_input_revision {
            transition(&txn, &resume, "superseded", false, now).await?;
            txn.commit().await.map_err(storage)?;
            return Ok(None);
        }
        current_decision(&session, &decision)?;
        if session.turn_state.is_active() {
            return Ok(None);
        }
        desk_diagnose_core::assistant_policy::require_current_policy(session.policy_revision)?;
        txn.commit().await.map_err(storage)?;
        Ok(Some(session))
    }

    pub(super) async fn claim_permission_resume(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let Some(ClaimBinding {
            request_id,
            expected_version,
            grants,
        }) = &self.permission_resume
        else {
            return Err(ClaimError::Backend(invalid()));
        };
        if params.trigger_origin != TriggerOrigin::PermissionDecision
            || params.turn_id != turn_id(&params.conversation_id, request_id)
            || self.surface != AgentSessionSurface::DeviceAssistant
        {
            return Err(ClaimError::Backend(invalid()));
        }
        let now = DateTime::parse_from_rfc3339(&params.now)
            .map_err(|_| ClaimError::Backend(invalid()))?
            .with_timezone(&Utc);
        let txn = self
            .db
            .begin()
            .await
            .map_err(|e| ClaimError::Backend(storage(e)))?;
        let row = find(&txn, &params.conversation_id)
            .await
            .map_err(|e| ClaimError::Backend(storage(e)))?
            .ok_or_else(|| ClaimError::Backend(invalid()))?;
        let mut session = permission_receipt::session(
            &row,
            &params.conversation_id,
            &params.actor_id,
            &params.device_id,
        )
        .map_err(ClaimError::Backend)?;
        if matches!(
            session.trigger_origin,
            TriggerOrigin::ScheduledContinuation | TriggerOrigin::ScheduledTask
        ) {
            return Err(ClaimError::Busy);
        }
        let (resume, decision) = record(&txn, &session, request_id)
            .await
            .map_err(ClaimError::Backend)?;
        if resume.state != "pending" {
            return Err(ClaimError::Busy);
        }
        if session.input_revision != decision.request_input_revision {
            transition(&txn, &resume, "superseded", false, now)
                .await
                .map_err(ClaimError::Backend)?;
            txn.commit()
                .await
                .map_err(|e| ClaimError::Backend(storage(e)))?;
            return Err(ClaimError::Busy);
        }
        if *expected_version < 1
            || row.version != *expected_version
            || self.expected_input_revision != Some(session.input_revision)
            || session.client_conversation_id != self.client_conversation_id
            || session.turn_state.is_active()
        {
            return Err(ClaimError::Busy);
        }
        current_decision(&session, &decision).map_err(ClaimError::Backend)?;
        let current_grants =
            crate::capability_grant_store::SignalCapabilityGrantStore::list_for_subject_on(
                &txn,
                &session.conversation_id,
                &session.actor_id,
                &session.device_id,
            )
            .await
            .map_err(|e| ClaimError::Backend(storage(e)))?;
        if &current_grants != grants {
            return Err(ClaimError::Busy);
        }
        desk_diagnose_core::assistant_policy::validate_claim(
            self.surface,
            Some(session.policy_revision),
            &params,
        )
        .map_err(ClaimError::Backend)?;
        self.reconcile_context_selection(&mut session)
            .map_err(ClaimError::Backend)?;
        session
            .begin_turn(
                params.turn_id.clone(),
                params.request_id,
                params.connection_id,
                params.policy_revision,
                params.current_pdp_scope,
                params.now,
            )
            .map_err(|_| ClaimError::Busy)?;
        session.adopt_trigger(TriggerOrigin::PermissionDecision, &params.turn_id);
        session.version = row
            .version
            .checked_add(1)
            .ok_or_else(|| ClaimError::Backend(invalid()))?;
        let encoded = session
            .encode_json_for_storage()
            .map_err(|_| ClaimError::Backend(invalid()))?;
        let changed = agent_session::Entity::update_many()
            .col_expr(agent_session::Column::StateJson, Expr::value(encoded))
            .col_expr(agent_session::Column::Version, Expr::value(session.version))
            .col_expr(
                agent_session::Column::LeaseToken,
                Expr::value(
                    i64::try_from(session.lease_token)
                        .map_err(|_| ClaimError::Backend(invalid()))?,
                ),
            )
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(Some(now + Duration::seconds(LEASE_TTL_SECS))),
            )
            .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::Version.eq(row.version))
            .exec(&txn)
            .await
            .map_err(|e| ClaimError::Backend(storage(e)))?;
        if changed.rows_affected != 1 {
            return Err(ClaimError::Busy);
        }
        transition(&txn, &resume, "started", true, now)
            .await
            .map_err(ClaimError::Backend)?;
        txn.commit()
            .await
            .map_err(|e| ClaimError::Backend(storage(e)))?;
        Ok(session)
    }
}

/// Consume the original decision inside the scheduler's task/session/run transaction.
/// This neither creates grants nor writes a session or occurrence lease.
pub(crate) async fn consume_scheduled_decision_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request_id: &str,
    expected_grants: &[desk_agent_protocol::capability_grant::CapabilityGrant],
    now: DateTime<Utc>,
) -> Result<(), AgentError> {
    if !matches!(
        session.trigger_origin,
        TriggerOrigin::ScheduledContinuation | TriggerOrigin::ScheduledTask
    ) || session.turn_state != TurnState::Idle
    {
        return Err(invalid());
    }
    let (resume, decision) = record(txn, session, request_id).await?;
    if resume.state != "pending" || session.input_revision != decision.request_input_revision {
        return Err(invalid());
    }
    current_decision(session, &decision)?;
    let grants = crate::capability_grant_store::SignalCapabilityGrantStore::list_for_subject_on(
        txn,
        &session.conversation_id,
        &session.actor_id,
        &session.device_id,
    )
    .await
    .map_err(storage)?;
    if grants != expected_grants {
        return Err(invalid());
    }
    desk_diagnose_core::assistant_policy::require_current_policy(session.policy_revision)?;
    transition(txn, &resume, "started", true, now).await?;
    if session.trigger_origin == TriggerOrigin::ScheduledTask {
        let changed = resume::Entity::update_many()
            .col_expr(
                resume::Column::TurnId,
                Expr::value(session.current_turn_id.clone()),
            )
            .filter(resume::Column::Id.eq(resume.id))
            .filter(resume::Column::Version.eq(resume.version.checked_add(1).ok_or_else(invalid)?))
            .filter(resume::Column::State.eq("started"))
            .exec(txn)
            .await
            .map_err(storage)?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Resolve an already consumed decision without changing authority.
/// The caller holds the schedule/session/run fence and supplies the persisted session.
pub(crate) async fn scheduled_decision_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<Option<String>, AgentError> {
    if session.turn_state != TurnState::Running {
        return Err(invalid());
    }
    Ok(scheduled_record_on(txn, session)
        .await?
        .map(|row| row.request_id))
}

/// Finish only the current consumed approval in the occurrence transaction.
pub(crate) async fn settle_scheduled_decision_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    now: DateTime<Utc>,
) -> Result<(), AgentError> {
    if !matches!(session.turn_state, TurnState::Idle | TurnState::Failed) {
        return Err(invalid());
    }
    if let Some(row) = scheduled_record_on(txn, session).await? {
        transition(txn, &row, "settled", true, now).await?;
    }
    Ok(())
}

async fn scheduled_record_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<Option<resume::Model>, AgentError> {
    if session.trigger_origin != TriggerOrigin::ScheduledContinuation {
        return Err(invalid());
    }
    let run_id = session.current_request_id.as_deref().ok_or_else(invalid)?;
    let current_turn = session.current_turn_id.as_deref().ok_or_else(invalid)?;
    if current_turn == format!("{run_id}-turn") {
        return Ok(None);
    }
    let rows = resume::Entity::find()
        .filter(resume::Column::RunId.eq(&session.conversation_id))
        .filter(resume::Column::ActorId.eq(&session.actor_id))
        .filter(resume::Column::DeviceId.eq(&session.device_id))
        .filter(resume::Column::TurnId.eq(current_turn))
        .limit(2)
        .all(txn)
        .await
        .map_err(storage)?;
    if rows.len() != 1 {
        return Err(invalid());
    }
    let (row, decision) = record(txn, session, &rows[0].request_id).await?;
    if row.state != "started" || session.input_revision != decision.request_input_revision {
        return Err(invalid());
    }
    current_decision(session, &decision)?;
    Ok(Some(row))
}

/// Settle only the original decision consumed by the task's current segment.
pub(crate) async fn settle_fresh_decision_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request_id: &str,
    now: DateTime<Utc>,
) -> Result<(), AgentError> {
    if session.trigger_origin != TriggerOrigin::ScheduledTask {
        return Err(invalid());
    }
    let (row, decision) = record(txn, session, request_id).await?;
    current_decision(session, &decision)?;
    if row.state != "started" || row.turn_id != session.current_turn_id {
        return Err(invalid());
    }
    let changed = resume::Entity::update_many()
        .col_expr(resume::Column::State, Expr::value("settled"))
        .col_expr(
            resume::Column::Version,
            Expr::value(row.version.checked_add(1).ok_or_else(invalid)?),
        )
        .col_expr(resume::Column::UpdatedAt, Expr::value(now))
        .filter(resume::Column::Id.eq(row.id))
        .filter(resume::Column::Version.eq(row.version))
        .filter(resume::Column::State.eq("started"))
        .exec(txn)
        .await
        .map_err(storage)?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(())
}

/// Close an unconsumed owner decision without rewriting its immutable receipt.
pub(crate) async fn close_fresh_wait_decision_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request_id: &str,
    now: DateTime<Utc>,
) -> Result<(), AgentError> {
    if session.trigger_origin != TriggerOrigin::ScheduledTask {
        return Err(invalid());
    }
    if permission_receipt::decided_on(txn, session, request_id)
        .await?
        .is_none()
    {
        return Ok(());
    }
    let (row, decision) = record(txn, session, request_id).await?;
    current_decision(session, &decision)?;
    if row.state != "pending" {
        return Err(invalid());
    }
    transition(txn, &row, "superseded", false, now).await
}

/// Recover a pause only from its original durable request/decision events.
pub(crate) async fn verify_fresh_wait_request_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request_id: &str,
) -> Result<(), AgentError> {
    if session.trigger_origin != TriggerOrigin::ScheduledTask {
        return Err(invalid());
    }
    let mut expected = permission_receipt::requested_on(txn, session, request_id)
        .await?
        .request;
    if let Some(decision) = permission_receipt::decided_on(txn, session, request_id).await? {
        let (row, original) = record(txn, session, request_id).await?;
        if row.state != "pending" || original != decision {
            return Err(invalid());
        }
        expected
            .apply_user_decision(&decision.items)
            .map_err(|_| invalid())?;
    }
    if !session.permission_requests.contains(&expected) {
        return Err(invalid());
    }
    Ok(())
}

/// Authenticate the immutable proposal only; this never attests to a decision or grant.
pub(crate) async fn verify_control_request_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request_id: &str,
) -> Result<(), AgentError> {
    let requested = permission_receipt::requested_on(txn, session, request_id).await?;
    let mut current = session
        .permission_requests
        .iter()
        .find(|request| request.request_id == request_id)
        .cloned()
        .ok_or_else(invalid)?;
    current.state = desk_diagnose_core::dynamic_run::PermissionRequestState::Pending;
    if current != requested.request {
        return Err(invalid());
    }
    Ok(())
}
