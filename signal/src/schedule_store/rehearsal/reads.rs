//! Reviewable successful reads from one frozen rehearsal; never a publication permit.
use super::*;
use crate::capability_grant_store::*;
use crate::entity::{
    agent_action_item as work_row, agent_capability_dispatch_outbox as outbox_row,
    agent_capability_grant as grant_row, agent_grant_reservation as reservation_row, agent_session,
};
use desk_agent_protocol::capability_grant::CapabilityGrantIssuer;
use desk_diagnose_core::{chat::ChatRole, session::PersistedAgentSession};
use sea_orm::{QueryOrder, QuerySelect};
use std::collections::BTreeSet;

pub use desk_diagnose_core::schedule::rehearsal::{ObservedRehearsalRead, RehearsalReadReport};

impl ScheduleStore {
    pub async fn read_rehearsal_reads(
        &self,
        owner: i32,
        rehearsal_id: &str,
    ) -> Result<RehearsalReadReport, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let report = Self::read_rehearsal_reads_on(&txn, owner, rehearsal_id).await?;
        txn.commit().await?;
        Ok(report)
    }

    /// Join the publication transaction; never acquire a second connection or commit it.
    pub(crate) async fn read_rehearsal_reads_on(
        txn: &sea_orm::DatabaseTransaction,
        owner: i32,
        rehearsal_id: &str,
    ) -> Result<RehearsalReadReport, ScheduleStoreError> {
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
        let works = work_row::Entity::find()
            .filter(work_row::Column::ConversationId.eq(&source.conversation_id))
            .filter(work_row::Column::Kind.eq(CAPABILITY_WORK_KIND))
            .order_by_asc(work_row::Column::Id)
            .all(txn)
            .await?;
        let mut report = RehearsalReadReport {
            rehearsal_id: source.rehearsal_id,
            session_sha256: snapshot,
            reads: Vec::new(),
            unconfirmed_read_call_ids: Vec::new(),
            other_tool_call_ids: Vec::new(),
        };
        let mut matched = BTreeSet::new();
        for work in works {
            let prepared: PreparedCapabilityPayload = serde_json::from_str(&work.payload_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
            let authority = &prepared.observed_authority;
            if !matches!(
                authority.effect,
                desk_agent_protocol::capability_provider::CapabilityEffect::ReadDevice
                    | desk_agent_protocol::capability_provider::CapabilityEffect::ReadFile
                    | desk_agent_protocol::capability_provider::CapabilityEffect::ReadExternal
                    | desk_agent_protocol::capability_provider::CapabilityEffect::CaptureScreen
            ) {
                continue;
            }
            let proposals: Vec<_> = session
                .conversation
                .iter()
                .filter(|message| {
                    message.role == ChatRole::Assistant
                        && message.turn_id.as_deref() == Some(work.turn_id.as_str())
                })
                .flat_map(|message| &message.tool_calls)
                .filter(|call| {
                    format!(
                        "capability-call-{}",
                        digest(&format!(
                            "{}:{}:{}",
                            session.conversation_id, work.turn_id, call.id
                        ))
                    ) == prepared.call_id
                })
                .collect();
            if proposals.len() != 1 {
                return Err(ScheduleStoreError::Conflict);
            }
            let call = proposals[0];
            let canonical =
                desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
                    &call.name,
                    serde_json::from_str(&call.arguments_json)
                        .map_err(|_| ScheduleStoreError::Invalid)?,
                )
                .map_err(|_| ScheduleStoreError::Invalid)?;
            if !matched.insert(call.id.clone())
                || work.actor_id != owner.to_string()
                || work.target_device_id != source.target_device_id
                || work.payload_schema_version != 1
                || work.manual_resolved_at.is_some()
                || work.created_at.timestamp_millis() < started
                || work.updated_at.timestamp_millis() > finished
                || work.is_side_effecting
                || work.action_request_id != prepared.call_id
                || work.tool_call_id != prepared.call_id
                || prepared.input_revision != 1
                || prepared.input_watermark != 1
                || prepared.canonical_input_json != canonical
                || prepared.canonical_input_digest_sha256 != digest(&canonical)
                || authority.canonical_input_sha256 != prepared.canonical_input_digest_sha256
                || work.draft_hash != prepared.canonical_input_digest_sha256
                || authority.provider_id != prepared.provider_id
                || authority.capability_id != prepared.capability_id
                || authority.tool_name != prepared.tool_name
                || call.name != prepared.tool_name
            {
                return Err(ScheduleStoreError::Conflict);
            }
            if work.status != CAPABILITY_WORK_SUCCEEDED {
                report.unconfirmed_read_call_ids.push(call.id.clone());
                continue;
            }
            if work.result_schema_version == Some(2) {
                let outbox = outbox_row::Entity::find()
                    .filter(outbox_row::Column::WorkId.eq(work.id))
                    .one(txn)
                    .await?
                    .ok_or(ScheduleStoreError::Conflict)?;
                let Some(observed) = SignalCapabilityGrantStore::observe_completed_provider_on(
                    txn,
                    &outbox.dispatch_id,
                )
                .await?
                else {
                    report.unconfirmed_read_call_ids.push(call.id.clone());
                    continue;
                };
                if observed.origin.tool_call_id != call.id
                    || observed.authority != *authority
                    || observed.canonical_input_json != canonical
                    || observed.completed_at
                        > u64::try_from(finished).map_err(|_| ScheduleStoreError::Conflict)?
                {
                    return Err(ScheduleStoreError::Conflict);
                }
                report.reads.push(ObservedRehearsalRead {
                    call_id: prepared.call_id,
                    tool_call_id: call.id.clone(),
                    grant_id: observed.grant_id,
                    issued_by: observed.issued_by,
                    authority: observed.authority,
                    completed_at: i64::try_from(observed.completed_at)
                        .map_err(|_| ScheduleStoreError::Conflict)?,
                    output_sha256: observed.output_sha256,
                });
                continue;
            }
            if work.result_schema_version != Some(1) {
                report.unconfirmed_read_call_ids.push(call.id.clone());
                continue;
            }
            let completed: CapabilityDispatchCompletion = serde_json::from_str(
                work.result_json
                    .as_deref()
                    .ok_or(ScheduleStoreError::Conflict)?,
            )
            .map_err(|_| ScheduleStoreError::Invalid)?;
            let outbox = outbox_row::Entity::find()
                .filter(outbox_row::Column::WorkId.eq(work.id))
                .one(txn)
                .await?
                .ok_or(ScheduleStoreError::Conflict)?;
            let dispatch: CapabilityDispatchPayload = serde_json::from_str(&outbox.payload_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
            let reserved = reservation_row::Entity::find()
                .filter(reservation_row::Column::WorkId.eq(work.id))
                .one(txn)
                .await?
                .ok_or(ScheduleStoreError::Conflict)?;
            if outbox.state != DISPATCH_OUTBOX_COMPLETED
                || outbox.payload_schema_version != 1
                || outbox.call_id != prepared.call_id
                || outbox.reservation_id != prepared.reservation_id
                || u64::try_from(outbox.generation).ok() != Some(prepared.generation)
                || reserved.state != RESERVATION_STATUS_COMMITTED
                || reserved.reservation_id != prepared.reservation_id
                || reserved.call_id != prepared.call_id
                || reserved.grant_id != prepared.grant_id
                || reserved.run_id != source.conversation_id
                || reserved.generation != outbox.generation
                || reserved.canonical_input_digest_sha256 != prepared.canonical_input_digest_sha256
                || dispatch.work_id != work.id
                || dispatch.dispatch_id != outbox.dispatch_id
                || dispatch.call_id != prepared.call_id
                || dispatch.grant_id != prepared.grant_id
                || dispatch.reservation_id != prepared.reservation_id
                || dispatch.generation != prepared.generation
                || dispatch.input_revision != prepared.input_revision
                || dispatch.input_watermark != prepared.input_watermark
                || dispatch.canonical_input_json != canonical
                || dispatch.canonical_input_digest_sha256 != prepared.canonical_input_digest_sha256
                || dispatch.provider_id != prepared.provider_id
                || dispatch.capability_id != prepared.capability_id
                || dispatch.tool_name != prepared.tool_name
                || dispatch.observed_authority != *authority
                || dispatch.command_origin.is_some()
                || dispatch.command_receipt.is_some()
                || outbox.computer_binding_json.is_some()
                || outbox.computer_acceptance_json.is_some()
                || outbox.computer_background_json.is_some()
                || completed.dispatch_id != outbox.dispatch_id
                || completed.call_id != prepared.call_id
                || completed.generation != prepared.generation
                || completed.outcome != CapabilityDispatchOutcome::Succeeded
                || !super::super::publication::valid_digest(&completed.result_digest_sha256)
                || work
                    .dispatch_intent_at
                    .is_none_or(|at| at < work.created_at || at > work.updated_at)
                || outbox.created_at < work.created_at
                || outbox.updated_at != work.updated_at
            {
                return Err(ScheduleStoreError::Conflict);
            }
            let row = grant_row::Entity::find()
                .filter(grant_row::Column::GrantId.eq(&prepared.grant_id))
                .one(txn)
                .await?
                .ok_or(ScheduleStoreError::Conflict)?;
            let grant = crate::capability_grant_store::decode_grant(&row)?;
            if grant.actor_id != work.actor_id
                || grant.run_id != work.conversation_id
                || grant.target_device_id != work.target_device_id
                || grant.input_revision != prepared.input_revision
                || grant.surface
                    != desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner
                || grant.policy_revision != work.policy_revision
                || !authority.is_within_grant(&grant)
                || matches!(grant.issued_by, CapabilityGrantIssuer::TaskAuthorization(_))
                || u64::try_from(work.created_at.timestamp_millis())
                    .ok()
                    .is_none_or(|at| at < grant.issued_at_unix_ms || at >= grant.expires_at_unix_ms)
            {
                return Err(ScheduleStoreError::Conflict);
            }
            report.reads.push(ObservedRehearsalRead {
                call_id: prepared.call_id,
                tool_call_id: call.id.clone(),
                grant_id: prepared.grant_id,
                issued_by: grant.issued_by,
                authority: prepared.observed_authority,
                completed_at: work.updated_at.timestamp_millis(),
                output_sha256: completed.result_digest_sha256,
            });
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

#[cfg(test)]
mod tests;
