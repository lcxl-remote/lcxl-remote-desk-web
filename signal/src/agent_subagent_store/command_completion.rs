//! Exact command-result interpretation claims only the existing child session.
use super::*;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::{
    seam::ClaimTurnParams,
    session::{TriggerOrigin, WorkKind},
};

impl SubAgentStore {
    pub async fn claim_child_completion(
        &self,
        params: &ClaimTurnParams,
        destination: &DestinationIdentity,
        event_id: &str,
    ) -> Result<Option<PersistedAgentSession>, DbErr> {
        if !matches!(
            params.trigger_origin,
            TriggerOrigin::ExecCompletion
                | TriggerOrigin::WorkCompletion {
                    kind: WorkKind::AgentExec
                }
        ) || !desk_diagnose_core::subagent::valid_id(event_id)
        {
            return Err(invalid());
        }
        let now = chrono::DateTime::parse_from_rfc3339(&params.now)
            .map_err(|_| invalid())?
            .with_timezone(&chrono::Utc);
        let txn = desk_diagnose_core::future::boxed(|| async {
            crate::db::begin_write(&self.db, session_row::Entity).await
        })
        .await?;

        let row = desk_diagnose_core::future::boxed(|| async {
            session_row::Entity::find()
                .filter(session_row::Column::ConversationId.eq(&params.conversation_id))
                .filter(session_row::Column::ActorId.eq(&params.actor_id))
                .filter(session_row::Column::DeviceId.eq(&params.device_id))
                .one(&txn)
                .await
        })
        .await?
        .ok_or_else(invalid)?;
        let mut session =
            PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
        if session.agent_role.binding().is_none()
            || session.turn_state.is_active()
            || session.version != row.version
            || session.lease_token as i64 != row.lease_token
            || session.scope_snapshot != params.current_pdp_scope
            || session.automation_turns_used >= 3
            || !desk_diagnose_core::future::boxed(|| async {
                child_resume_admitted_on(&txn, &session, now.timestamp_millis()).await
            })
            .await?
        {
            return Ok(None);
        }
        desk_diagnose_core::future::send_phase(|| async {
            let (creation, _) = desk_diagnose_core::future::boxed(|| async {
                task_context_on(&txn, &session).await
            })
            .await?
            .ok_or_else(invalid)?;
            let binding = session.agent_role.binding().ok_or_else(invalid)?;
            desk_diagnose_core::future::boxed(|| async {
                parent_on(
                    &txn,
                    &binding.root_conversation_id,
                    &session.actor_id,
                    &session.device_id,
                )
                .await
            })
            .await?;
            if creation.source.model_destination != *destination
                || !session.pending_auto_triggers.iter().any(|pending| {
                    pending.event_id == event_id
                        && pending.kind == WorkKind::AgentExec
                        && pending.chain_id == session.chain_id
                        && pending.resolution_org_id.is_none()
                })
            {
                return Ok(None);
            }
            desk_diagnose_core::assistant_policy::validate_claim(
                session.surface,
                Some(session.policy_revision),
                params,
            )
            .map_err(|_| invalid())?;
            session
                .begin_turn(
                    &params.turn_id,
                    params.request_id.clone(),
                    None,
                    params.policy_revision,
                    params.current_pdp_scope.clone(),
                    &params.now,
                )
                .map_err(|_| invalid())?;
            session.adopt_trigger(params.trigger_origin, &params.turn_id);
            desk_diagnose_core::future::boxed(|| async {
                record_child_resume_on(&txn, &session, now.timestamp_millis()).await
            })
            .await?;
            session.version = row.version.checked_add(1).ok_or_else(invalid)?;
            let changed = desk_diagnose_core::future::boxed(|| async {
                session_row::Entity::update_many()
                .set(session_row::ActiveModel {
                    state_json: Set(session.encode_json_for_storage().map_err(|_| invalid())?),
                    version: Set(session.version),
                    lease_token: Set(i64::try_from(session.lease_token).map_err(|_| invalid())?),
                    lease_deadline: Set(Some(now + chrono::Duration::seconds(90))),
                    updated_at: Set(now),

                    ..Default::default()
                })
                .filter(session_row::Column::Id.eq(row.id))
                .filter(session_row::Column::Version.eq(row.version))
                .filter(session_row::Column::LeaseToken.eq(row.lease_token))
                .exec(&txn)
                .await
            })
            .await?;
            if changed.rows_affected != 1 {
                return Err(invalid());
            }
            desk_diagnose_core::future::boxed(|| async { txn.commit().await }).await?;
            Ok(Some(session))
        })
        .await
    }
}
