//! Planner-budget entry points own transactions and never hold them over I/O.
use super::*;
use desk_diagnose_core::{
    goal::GoalUsage,
    subagent::{
        budget::Usage,
        reservation::{CallAdmission, DelegationCallKind, DelegationCallReservation},
    },
};

impl SubAgentStore {
    pub async fn reserve_runtime_call(
        &self,
        session: &PersistedAgentSession,
        logical_id: &str,
        kind: DelegationCallKind,
        arguments_sha256: &str,
        upper: GoalUsage,
        now_ms: i64,
    ) -> Result<CallAdmission, DbErr> {
        let Some(group_id) = session.delegation_group_id.as_deref() else {
            return if session.agent_role.is_main()
                && session.trigger_origin
                    != desk_diagnose_core::session::TriggerOrigin::ScheduledTask
            {
                Ok(CallAdmission::Untracked)
            } else {
                Err(invalid())
            };
        };
        if upper.slices != 0 {
            return Err(invalid());
        }
        let allocation = Usage {
            model_calls: u64::from(upper.model_calls),
            tool_calls: u64::from(upper.tool_calls),
            tokens: upper.total_tokens().ok_or_else(invalid)?,
        };
        let root = session
            .agent_role
            .binding()
            .map_or(session.conversation_id.as_str(), |binding| {
                binding.root_conversation_id.as_str()
            });
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;

        let group = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(group_id))
            .filter(group_row::Column::RootConversationId.eq(root))
            .filter(group_row::Column::ActorId.eq(&session.actor_id))
            .filter(group_row::Column::DeviceId.eq(&session.device_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let group = decode_group(&group)?;
        if session.trigger_origin == desk_diagnose_core::session::TriggerOrigin::GoalContinuation {
            let segment = session
                .focus_epoch
                .goal_segment
                .as_ref()
                .ok_or_else(invalid)?;
            if group.source.goal_id() != Some(segment.goal_id.as_str()) {
                return Err(invalid());
            }
        }
        let goal_upper = group.source.goal_id().map(|_| upper);
        let result = reserve_call_budget_on(
            &txn,
            session,
            logical_id,
            kind,
            arguments_sha256,
            allocation,
            goal_upper,
            now_ms,
        )
        .await;
        let receipt = match result {
            Ok(BudgetAdmission::Reserved(reservation)) => CallAdmission::Reserved(reservation),
            Ok(BudgetAdmission::Exhausted) => {
                txn.rollback().await?;
                return Ok(CallAdmission::Exhausted);
            }
            // Retries resolve the original operation ledger. Returning a prior
            // receipt here would authorize a second physical call with one charge.
            Ok(BudgetAdmission::AlreadyReserved(_) | BudgetAdmission::Settled(_, _)) => {
                return Err(DbErr::Custom(
                    "delegation logical call was already admitted".into(),
                ));
            }
            Err(error) => return Err(error),
        };
        txn.commit().await?;
        Ok(receipt)
    }

    pub async fn settle_runtime_call(
        &self,
        reservation: &DelegationCallReservation,
        actual: Option<GoalUsage>,
        now_ms: i64,
    ) -> Result<(), DbErr> {
        let txn = crate::db::begin_write(&self.db, session_row::Entity).await?;

        let row = crate::entity::agent_delegation_reservation::Entity::find()
            .filter(
                crate::entity::agent_delegation_reservation::Column::ReservationId
                    .eq(&reservation.reservation_id),
            )
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        if budget::decode_reservation(&row)? != *reservation {
            return Err(invalid());
        }
        let actual = if row.provider_receipt_id.is_some() {
            let recorded = model_receipt::provider_usage_on(&txn, &row).await?;
            if let (Some(claimed), Some(recorded)) = (actual, recorded)
                && (claimed.input_tokens != recorded.input_tokens
                    || claimed.output_tokens != recorded.output_tokens
                    || claimed.cache_read_tokens != recorded.cache_read_tokens
                    || claimed.cache_write_tokens != recorded.cache_write_tokens
                    || claimed.model_calls != recorded.model_calls
                    || claimed.tool_calls != recorded.tool_calls)
            {
                return Err(invalid());
            }
            // Both regular completion and crash reconciliation use the same
            // immutable timestamp/usage fact, so settlement replays agree.
            recorded
        } else if reservation.kind != DelegationCallKind::Tool {
            // Linkage and provider-start now commit together. Under the same
            // source/control lock, no link proves no physical call was admitted.
            if actual.is_some_and(|usage| usage != GoalUsage::default()) {
                return Err(invalid());
            }
            Some(GoalUsage::default())
        } else {
            actual
        };
        let actual_allocation = actual
            .map(|actual| -> Result<Usage, DbErr> {
                if actual.slices != 0 {
                    return Err(invalid());
                }
                Ok(Usage {
                    model_calls: u64::from(actual.model_calls),
                    tool_calls: u64::from(actual.tool_calls),
                    tokens: actual.total_tokens().ok_or_else(invalid)?,
                })
            })
            .transpose()?;
        let actual_goal = reservation.source_goal_upper.and(actual);
        settle_call_budget_on(&txn, reservation, actual_allocation, actual_goal, now_ms).await?;
        txn.commit().await?;
        Ok(())
    }
}
