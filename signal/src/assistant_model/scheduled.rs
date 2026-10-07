//! Fresh-task admission on the audited model dispatch path.
use super::*;
use crate::{
    ai_assistant_gate::AiAssistantGate,
    control_authorizer::SINGLE_ACCOUNT_USER_ID,
    entity::agent_session,
    schedule_store::{ScheduleStore, TaskBudgetKind, TaskBudgetRequest},
};
use desk_diagnose_core::schedule::model_admission::ModelAdmissionError;
use desk_diagnose_core::{model_egress::AuthorizedModelRequest, session::PersistedAgentSession};
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter};
use std::sync::Arc;

pub(crate) struct FreshTaskModelContext {
    pub run_id: String,
    pub device_id: String,
    pub node_id: String,
    pub run_epoch: i64,
    pub session_token: u64,
    pub target_connection_id: String,
    pub connections: actix_web::web::Data<SharedConnectionMap>,
    pub gate: Arc<AiAssistantGate>,
}

fn denied() -> DbErr {
    DbErr::Custom("scheduled model dispatch is no longer authorized".into())
}

impl FreshTaskModelContext {
    async fn validate_target(&self) -> Result<(), DbErr> {
        let peers = self.connections.read().await;
        let mut matches = peers.values().filter(|peer| {
            peer.auth_context.auth_kind == AuthKind::TokenAuth
                && peer.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                && peer.model.version_info.client_id.as_deref() == Some(self.device_id.as_str())
        });
        if matches
            .next()
            .is_none_or(|peer| peer.model.connection_id != self.target_connection_id)
            || matches.next().is_some()
        {
            return Err(denied());
        }
        Ok(())
    }
}

impl MeteredModel {
    pub(super) async fn record_dispatch(
        &self,
        authorized: &AuthorizedModelRequest,
        ordinal: u64,
        ordinary_id: String,
    ) -> Result<String, ModelAdmissionError<DbErr>> {
        if let Some(reservation) = authorized.request.delegation_call.as_ref() {
            let (_, units) = self
                .inner
                .task_request_budget(&authorized.request)
                .map_err(|_| denied())?;
            if !reservation.permits_rendered_tokens(units) {
                return Err(ModelAdmissionError::BudgetExceeded);
            }
        }
        let Some(task) = &self.fresh_task else {
            let gate = crate::ai_assistant_gate::global_ai_assistant_gate();
            let settings = gate.snapshot();
            if self.inner.dispatch_cancelled() || !settings.enabled {
                return Err(denied().into());
            }
            let txn =
                crate::db::begin_write(&self.db, crate::entity::agent_session::Entity).await?;
            self.inner
                .validate_current_on(&txn)
                .await
                .map_err(|_| denied())?;
            if let Some(reservation) = authorized.request.delegation_call.as_ref() {
                crate::agent_subagent_store::SubAgentStore::link_model_receipt_on(
                    &txn,
                    reservation,
                    &ordinary_id,
                    chrono::Utc::now().timestamp_millis(),
                )
                .await?;
            }
            crate::model_egress_store::SignalModelEgressStore::record_dispatch_intent_on(
                &txn,
                ordinary_id.clone(),
                self.export_authorization_id.clone(),
                ordinal,
                &authorized.audit,
                &authorized.input_envelopes,
            )
            .await?;
            if self.inner.dispatch_cancelled() || gate.snapshot() != settings {
                return Err(denied().into());
            }
            txn.commit().await?;
            return Ok(ordinary_id);
        };
        let settings = task.gate.snapshot();
        if !settings.enabled || task.run_epoch <= 0 || task.session_token == 0 {
            return Err(denied().into());
        }
        task.validate_target().await?;
        let (digest, units) = self
            .inner
            .task_request_budget(&authorized.request)
            .map_err(|_| denied())?;
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_session::Entity).await?;
        ScheduleStore::lock_run_authority(
            &txn,
            SINGLE_ACCOUNT_USER_ID,
            &task.device_id,
            &task.run_id,
            &task.node_id,
            task.run_epoch,
        )
        .await
        .map_err(|_| denied())?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&task.run_id))
            .filter(agent_session::Column::ActorId.eq(SINGLE_ACCOUNT_USER_ID.to_string()))
            .filter(agent_session::Column::DeviceId.eq(&task.device_id))
            .one(&txn)
            .await?
            .ok_or_else(denied)?;
        let held = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| denied())?;
        if held.lease_token != task.session_token
            || self.export_authorization_id
                != model_export_id(
                    &held.actor_id,
                    &held.device_id,
                    &held.conversation_id,
                    ModelExportSource::Turn(held.current_turn_id.as_deref().ok_or_else(denied)?),
                )
        {
            return Err(denied().into());
        }
        // The task write fence holds SQLite's writer lock, so local model edits
        // cannot commit between this configuration check and dispatch commit.
        self.inner
            .validate_current_on(&txn)
            .await
            .map_err(|_| denied())?;
        let receipt_id = if let Some(reservation) = authorized.request.delegation_call.as_ref() {
            // The group and original occurrence quotas were reserved together.
            crate::agent_subagent_store::SubAgentStore::link_model_receipt_on(
                &txn,
                reservation,
                &ordinary_id,
                chrono::Utc::now().timestamp_millis(),
            )
            .await?;
            crate::model_egress_store::SignalModelEgressStore::record_dispatch_intent_on(
                &txn,
                ordinary_id.clone(),
                self.export_authorization_id.clone(),
                ordinal,
                &authorized.audit,
                &authorized.input_envelopes,
            )
            .await?;
            ordinary_id
        } else {
            ScheduleStore::reserve_fresh_model_dispatch_on(
                &txn,
                &held,
                &TaskBudgetRequest {
                    owner: SINGLE_ACCOUNT_USER_ID,
                    device: &task.device_id,
                    run_id: &task.run_id,
                    node: &task.node_id,
                    lease_epoch: task.run_epoch,
                    kind: TaskBudgetKind::ModelTokens,
                    rule_id: None,
                    logical_key: &format!("model-step-{ordinal}"),
                    input_sha256: &digest,
                    units,
                },
                &self.export_authorization_id,
                ordinal,
                &authorized.audit,
                &authorized.input_envelopes,
            )
            .await
            .map_err(|error| match error {
                crate::schedule_store::ScheduleStoreError::BudgetExceeded => {
                    ModelAdmissionError::BudgetExceeded
                }
                _ => ModelAdmissionError::Backend(denied()),
            })?
            .receipt
            .receipt_id
        };
        task.validate_target().await?;
        if self.inner.dispatch_cancelled() || task.gate.snapshot() != settings {
            return Err(denied().into());
        }
        txn.commit().await?;
        Ok(receipt_id)
    }

    pub(super) async fn settle_task_dispatch(
        &self,
        ordinal: u64,
        usage: &desk_diagnose_core::chat::TokenUsage,
        delegated: bool,
    ) -> Result<(), AgentError> {
        if delegated {
            return Ok(());
        }
        let Some(task) = &self.fresh_task else {
            return Ok(());
        };
        // Missing primary usage is durable but not permission to refund quota.
        if desk_diagnose_core::schedule::model_usage::terminal_token_units(usage).is_none() {
            return Ok(());
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_session::Entity)
            .await
            .map_err(|_| transport_error("task accounting unavailable"))?;
        let reservation = ScheduleStore::settle_fresh_model_dispatch_on(
            &txn,
            SINGLE_ACCOUNT_USER_ID,
            &task.run_id,
            u32::try_from(ordinal).map_err(|_| transport_error("invalid task model ordinal"))?,
        )
        .await
        .map_err(|_| transport_error("task accounting unavailable"))?;
        txn.commit()
            .await
            .map_err(|_| transport_error("task accounting unavailable"))?;
        if reservation.state == "overrun" {
            return Err(transport_error("task model budget exceeded"));
        }
        Ok(())
    }
}
