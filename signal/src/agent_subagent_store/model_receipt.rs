//! Link budget admission to the existing immutable provider usage ledger.
use super::*;
use crate::entity::agent_delegation_reservation as reservation_row;
use desk_diagnose_core::subagent::reservation::{DelegationCallKind, DelegationCallReservation};

pub(super) async fn provider_usage_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    row: &reservation_row::Model,
) -> Result<Option<desk_diagnose_core::goal::GoalUsage>, DbErr> {
    let (Some(kind), Some(id), Some(_started)) = (
        &row.provider_receipt_kind,
        &row.provider_receipt_id,
        row.provider_started_at_ms,
    ) else {
        return if row.provider_receipt_kind.is_none()
            && row.provider_receipt_id.is_none()
            && row.provider_started_at_ms.is_none()
        {
            Ok(None)
        } else {
            Err(invalid())
        };
    };
    if kind != "oss_model_egress" || row.operation_kind == "tool" {
        return Err(invalid());
    }
    let Some(call) = crate::entity::model_egress_receipt::Entity::find_by_id(id)
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let (Some(encoded), Some(completed)) = (call.usage_json, call.usage_recorded_at) else {
        return Ok(None);
    };
    let elapsed = u64::try_from(
        completed
            .timestamp_millis()
            .checked_sub(call.authorized_at.timestamp_millis())
            .ok_or_else(invalid)?,
    )
    .map_err(|_| invalid())?;
    let tokens = serde_json::from_str(&encoded).map_err(|_| invalid())?;
    Ok(desk_diagnose_core::subagent::reservation::known_model_usage(tokens, elapsed))
}

/// The caller holds owner/root/child controls. A reserved allocation never
/// substitutes for the planner lease or current source admission.
async fn validate_dispatch_on(
    txn: &crate::config::connection::DatabaseTransaction,
    reservation: &DelegationCallReservation,
    now_ms: i64,
) -> Result<(), DbErr> {
    let source = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&reservation.group_id))
        .filter(group_row::Column::RootConversationId.eq(&reservation.root_conversation_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let group = decode_group(&source)?;
    if group.source_admission != SourceAdmission::Open
        || group.source_epoch != reservation.source_epoch
        || now_ms >= group.limits.deadline_ms
    {
        return Err(invalid());
    }
    super::scheduled_source::current_scheduled_source_on(txn, &source).await?;
    parent_on(
        txn,
        &group.root_conversation_id,
        &group.actor_id,
        &group.device_id,
    )
    .await?;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&reservation.conversation_id))
        .filter(session_row::Column::ActorId.eq(&group.actor_id))
        .filter(session_row::Column::DeviceId.eq(&group.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    if session.surface != AgentSessionSurface::AiAssistant
        || session.version != row.version
        || session.delegation_group_id.as_ref() != Some(&reservation.group_id)
        || session.input_revision != reservation.input_revision
        || session.control_revision != reservation.control_revision
    {
        return Err(invalid());
    }
    if let Some(review) = &reservation.review_authority {
        super::review_budget::validate_review_scope_on(txn, review, now_ms).await?;
    } else {
        let token = reservation.planning_lease_token.ok_or_else(invalid)?;
        if !session.turn_state.is_active()
            || session.lease_token != token
            || row.lease_token != token as i64
            || row
                .lease_deadline
                .is_none_or(|deadline| deadline.timestamp_millis() <= now_ms)
        {
            return Err(invalid());
        }
    }
    if let Some(task_id) = &reservation.task_id {
        let task = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(task_id))
            .filter(run_row::Column::GroupId.eq(&group.group_id))
            .filter(run_row::Column::RootConversationId.eq(&group.root_conversation_id))
            .filter(run_row::Column::ActorId.eq(&group.actor_id))
            .filter(run_row::Column::DeviceId.eq(&group.device_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let run = decode_run(&task)?;
        run.validate_session(&session).map_err(|_| invalid())?;
        run.require_current(run.fence()).map_err(|_| invalid())?;
        if run.binding.source_epoch != group.source_epoch
            || run.binding.source != group.source
            || now_ms >= run.binding.deadline_ms
        {
            return Err(invalid());
        }
    } else if !session.agent_role.is_main()
        || session.main_stopped
        || !group.can_interpret(session.input_revision, session.control_revision)
    {
        return Err(invalid());
    }
    Ok(())
}

impl SubAgentStore {
    /// A second boundary cannot reuse this reservation, including exact replay.
    /// No provider I/O occurs until this short transaction has committed.
    #[cfg(test)]
    pub async fn link_model_receipt(
        &self,
        reservation: &DelegationCallReservation,
        provider_receipt_id: &str,
        now_ms: i64,
    ) -> Result<(), DbErr> {
        reservation.validate().map_err(|_| invalid())?;
        if reservation.kind == DelegationCallKind::Tool
            || provider_receipt_id.is_empty()
            || provider_receipt_id.len() > 256
            || now_ms <= 0
        {
            return Err(invalid());
        }
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;
        Self::link_model_receipt_on(&txn, reservation, provider_receipt_id, now_ms).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Join source/lease validation, linkage and the durable provider-start fence.
    /// The caller rolls back on error and sends no request before commit.
    pub(crate) async fn link_model_receipt_on(
        txn: &crate::config::connection::DatabaseTransaction,
        reservation: &DelegationCallReservation,
        provider_receipt_id: &str,
        now_ms: i64,
    ) -> Result<(), DbErr> {
        reservation.validate().map_err(|_| invalid())?;
        if reservation.kind == DelegationCallKind::Tool
            || provider_receipt_id.is_empty()
            || provider_receipt_id.len() > 256
            || now_ms <= 0
        {
            return Err(invalid());
        }
        validate_dispatch_on(txn, reservation, now_ms).await?;
        let old = reservation_row::Entity::find()
            .filter(reservation_row::Column::ReservationId.eq(&reservation.reservation_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        if budget::decode_reservation(&old)? != *reservation
            || old.state != "reserved"
            || old.provider_receipt_id.is_some()
            || old.provider_receipt_kind.is_some()
            || old.provider_started_at_ms.is_some()
        {
            return Err(invalid());
        }
        super::scheduled_budget::validate_dispatch_on(txn, &old, reservation).await?;
        if let Some(review) = &reservation.review_authority {
            let claimed = crate::entity::agent_approval_review::Entity::find()
                .filter(
                    crate::entity::agent_approval_review::Column::CandidateId
                        .eq(&review.candidate_id),
                )
                .one(txn)
                .await?
                .ok_or_else(invalid)?;
            if claimed.delegation_reservation_id.as_deref()
                != Some(reservation.reservation_id.as_str())
            {
                return Err(invalid());
            }
            super::review_budget::mark_review_dispatch_on(
                txn,
                review,
                "oss_model_egress",
                provider_receipt_id,
                now_ms,
            )
            .await?;
        }
        let changed = reservation_row::Entity::update_many()
            .set(reservation_row::ActiveModel {
                provider_receipt_id: Set(Some(provider_receipt_id.into())),
                provider_receipt_kind: Set(Some("oss_model_egress".into())),
                provider_started_at_ms: Set(Some(now_ms)),
                version: Set(old.version.checked_add(1).ok_or_else(invalid)?),
                ..Default::default()
            })
            .filter(reservation_row::Column::Id.eq(old.id))
            .filter(reservation_row::Column::Version.eq(old.version))
            .filter(reservation_row::Column::State.eq("reserved"))
            .filter(reservation_row::Column::ProviderReceiptId.is_null())
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
        Ok(())
    }

    /// Recover an allocation whose planner disappeared before the atomic
    /// provider-start commit. A live queued call keeps its reservation.
    async fn reconcile_unstarted_model(
        &self,
        reservation: &DelegationCallReservation,
        now_ms: i64,
    ) -> Result<bool, DbErr> {
        if reservation.kind == DelegationCallKind::Tool {
            return Ok(false);
        }
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;
        let source = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&reservation.group_id))
            .filter(group_row::Column::RootConversationId.eq(&reservation.root_conversation_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let group = decode_group(&source)?;
        let row = reservation_row::Entity::find()
            .filter(reservation_row::Column::ReservationId.eq(&reservation.reservation_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        if budget::decode_reservation(&row)? != *reservation {
            return Err(invalid());
        }
        if row.state == "settled" || row.provider_receipt_id.is_some() {
            return Ok(false);
        }
        let live = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&reservation.conversation_id))
            .filter(session_row::Column::ActorId.eq(&group.actor_id))
            .filter(session_row::Column::DeviceId.eq(&group.device_id))
            .one(&txn)
            .await?;
        let mut planner_live = false;
        if group.source_admission == SourceAdmission::Open
            && group.source_epoch == reservation.source_epoch
            && now_ms < group.limits.deadline_ms
            && let Some(live) = live
        {
            let held =
                PersistedAgentSession::decode_json(&live.state_json).map_err(|_| invalid())?;
            planner_live = held.turn_state.is_active()
                && Some(held.lease_token) == reservation.planning_lease_token
                && u64::try_from(live.lease_token).ok() == reservation.planning_lease_token
                && held.input_revision == reservation.input_revision
                && held.control_revision == reservation.control_revision
                && live
                    .lease_deadline
                    .is_some_and(|deadline| deadline.timestamp_millis() > now_ms);
            if let Some(review) = &reservation.review_authority {
                planner_live = super::review_budget::validate_review_scope_on(&txn, review, now_ms)
                    .await
                    .is_ok();
            }
            if reservation.task_id.is_none() {
                planner_live &= held.agent_role.is_main()
                    && !held.main_stopped
                    && group.can_interpret(held.input_revision, held.control_revision);
            } else if let Some(task_id) = &reservation.task_id {
                let task = run_row::Entity::find()
                    .filter(run_row::Column::TaskId.eq(task_id))
                    .filter(run_row::Column::GroupId.eq(&group.group_id))
                    .filter(run_row::Column::ActorId.eq(&group.actor_id))
                    .filter(run_row::Column::DeviceId.eq(&group.device_id))
                    .one(&txn)
                    .await?
                    .ok_or_else(invalid)?;
                let run = decode_run(&task)?;
                planner_live &=
                    run.validate_session(&held).is_ok() && run.require_current(run.fence()).is_ok();
            }
        }
        if planner_live {
            return Ok(false);
        }
        super::funding::settle_call_budget_on(
            &txn,
            reservation,
            Some(Default::default()),
            reservation.source_goal_upper.map(|_| Default::default()),
            now_ms,
        )
        .await?;
        txn.commit().await?;
        Ok(true)
    }

    /// Usage reconciliation alone cannot wake a planner or re-arm provider I/O.
    pub async fn reconcile_model_usage(
        &self,
        after_id: i64,
        limit: u32,
    ) -> Result<(i64, u32), DbErr> {
        if after_id < 0 || !(1..=100).contains(&limit) {
            return Err(invalid());
        }
        let rows = reservation_row::Entity::find()
            .filter(reservation_row::Column::Id.gt(after_id))
            .filter(reservation_row::Column::State.is_in(["reserved", "usage_unknown"]))
            .filter(reservation_row::Column::OperationKind.is_in([
                "model",
                "context_summary",
                "approval_review",
                "safety_review",
            ]))
            .order_by_asc(reservation_row::Column::Id)
            .limit(u64::from(limit))
            .all(&self.db)
            .await?;
        let next_cursor = if rows.len() < limit as usize {
            0
        } else {
            rows.last().map_or(0, |row| row.id)
        };
        let mut settled = 0;
        for row in rows {
            let result = async {
                let receipt = budget::decode_reservation(&row)?;
                if row.provider_receipt_id.is_none() {
                    return self
                        .reconcile_unstarted_model(&receipt, chrono::Utc::now().timestamp_millis())
                        .await;
                }
                if let Some(actual) = provider_usage_on(&self.db, &row).await? {
                    self.settle_runtime_call(
                        &receipt,
                        Some(actual),
                        chrono::Utc::now().timestamp_millis(),
                    )
                    .await?;
                    Ok::<_, DbErr>(true)
                } else {
                    Ok(false)
                }
            }
            .await;
            match result {
                Ok(true) => settled += 1,
                Ok(false) => {}
                Err(error) => log::warn!(
                    "[subagent] provider usage reconciliation failed for reservation {}: {error}",
                    row.reservation_id
                ),
            }
        }
        Ok((next_cursor, settled))
    }

    pub async fn run_usage_reconciler(self) {
        let mut cursor = 0;
        loop {
            match self.reconcile_model_usage(cursor, 50).await {
                Ok((next, _)) => cursor = next,
                Err(error) => log::warn!("[subagent] provider usage scan failed: {error}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    }
}
