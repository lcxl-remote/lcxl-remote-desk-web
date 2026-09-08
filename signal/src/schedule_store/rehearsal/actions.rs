//! Review successful provider actions from an immutable completed rehearsal.
use super::*;
use crate::capability_grant_store::{
    SignalCapabilityGrantStore, computer_completion::observation::ObservedProviderAction,
};
use crate::entity::{
    agent_action_item as work_item, agent_capability_dispatch_outbox as outbox, agent_session,
};
use desk_diagnose_core::{chat::ChatRole, session::PersistedAgentSession};
use sea_orm::{QueryOrder, QuerySelect};
use std::collections::BTreeSet;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct RehearsalActionReport {
    pub rehearsal_id: String,
    pub session_sha256: String,
    pub actions: Vec<ObservedProviderAction>,
    pub unconfirmed_tool_call_ids: Vec<String>,
    pub other_tool_call_ids: Vec<String>,
}

impl ScheduleStore {
    /// Join the publication transaction; never acquire a second connection or commit it.
    pub(crate) async fn read_rehearsal_actions_on(
        txn: &sea_orm::DatabaseTransaction,
        owner: i32,
        rehearsal_id: &str,
    ) -> Result<RehearsalActionReport, ScheduleStoreError> {
        let source = rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::RehearsalId.eq(rehearsal_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                sea_orm::sea_query::Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(&source.schedule_id))
            .exec(txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::NotFound);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&source.conversation_id))
            .lock_exclusive()
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let source = rehearsal::Entity::find_by_id(source.id)
            .lock_exclusive()
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let snapshot = digest(&row.state_json);
        if source.status != "completed"
            || source.completed_session_sha256.as_deref() != Some(snapshot.as_str())
            || source.completed_session_version != Some(row.version)
            || session.version != row.version
            || row.actor_id != owner.to_string()
            || session.actor_id != row.actor_id
            || row.device_id != source.target_device_id
            || session.device_id != row.device_id
            || session.conversation_id != source.conversation_id
            || session.client_conversation_id.as_deref()
                != Some(source.client_conversation_id.as_str())
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let started = source.started_at.ok_or(ScheduleStoreError::Conflict)?;
        let finished = source.finished_at.ok_or(ScheduleStoreError::Conflict)?;
        if started < 0 || finished < started {
            return Err(ScheduleStoreError::Conflict);
        }
        let works = work_item::Entity::find()
            .filter(work_item::Column::ConversationId.eq(&source.conversation_id))
            .filter(work_item::Column::Kind.eq("capability_provider"))
            .order_by_asc(work_item::Column::Id)
            .all(txn)
            .await?;
        let mut report = RehearsalActionReport {
            rehearsal_id: source.rehearsal_id,
            session_sha256: snapshot,
            actions: Vec::new(),
            unconfirmed_tool_call_ids: Vec::new(),
            other_tool_call_ids: Vec::new(),
        };
        let mut matched = BTreeSet::new();
        for work in works {
            let prepared: crate::capability_grant_store::PreparedCapabilityPayload =
                serde_json::from_str(&work.payload_json)
                    .map_err(|_| ScheduleStoreError::Invalid)?;
            use desk_agent_protocol::capability_provider::CapabilityEffect;
            if matches!(
                prepared.observed_authority.effect,
                CapabilityEffect::ReadDevice
                    | CapabilityEffect::ReadFile
                    | CapabilityEffect::ReadExternal
                    | CapabilityEffect::CaptureScreen
            ) {
                // All read transports belong to the same read report, including native receipts.
                continue;
            }
            let Some(dispatch) = outbox::Entity::find()
                .filter(outbox::Column::WorkId.eq(work.id))
                .one(txn)
                .await?
            else {
                // Prepared work has no dispatch receipt yet.
                continue;
            };
            if dispatch.computer_binding_json.is_none() {
                // Inline reads are classified by the read report. Other unmatched
                // calls remain unclassified and cannot pass publication.
                continue;
            }
            let observed = SignalCapabilityGrantStore::observe_completed_provider_on(
                txn,
                &dispatch.dispatch_id,
            )
            .await?;
            let Some(observed) = observed else {
                // Keep unresolved actions unclassified; they cannot prove publication coverage.
                let binding: crate::capability_grant_store::computer_binding::ComputerBinding =
                    serde_json::from_str(
                        dispatch
                            .computer_binding_json
                            .as_deref()
                            .ok_or(ScheduleStoreError::Conflict)?,
                    )
                    .map_err(|_| ScheduleStoreError::Invalid)?;
                report
                    .unconfirmed_tool_call_ids
                    .push(binding.origin.tool_call_id);
                continue;
            };
            let proposals: Vec<_> = session
                .conversation
                .iter()
                .filter(|message| {
                    message.role == ChatRole::Assistant
                        && message.turn_id.as_deref() == Some(work.turn_id.as_str())
                })
                .flat_map(|message| &message.tool_calls)
                .filter(|call| call.id == observed.origin.tool_call_id)
                .collect();
            if proposals.len() != 1
                || !matched.insert(observed.origin.tool_call_id.clone())
                || work.actor_id != session.actor_id
                || work.target_device_id != source.target_device_id
                || work.created_at.timestamp_millis() < started
                || observed.origin.turn_fence.input_revision != 1
                || observed.origin.tool_name != proposals[0].name
                || observed.completed_at
                    > u64::try_from(finished).map_err(|_| ScheduleStoreError::Conflict)?
            {
                return Err(ScheduleStoreError::Conflict);
            }
            let proposal = proposals[0];
            let canonical =
                desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
                    &proposal.name,
                    serde_json::from_str(&proposal.arguments_json)
                        .map_err(|_| ScheduleStoreError::Invalid)?,
                )
                .map_err(|_| ScheduleStoreError::Invalid)?;
            if canonical != observed.canonical_input_json {
                return Err(ScheduleStoreError::Conflict);
            }
            report.actions.push(observed);
        }
        let directory_controls =
            super::super::directory_receipt::verified_control_calls(txn, &session).await?;
        report.other_tool_call_ids = session
            .conversation
            .iter()
            .flat_map(|message| &message.tool_calls)
            .filter(|call| !matched.contains(&call.id) && !directory_controls.contains(&call.id))
            .map(|call| call.id.clone())
            .collect();
        Ok(report)
    }
}
