//! Continue a claimed occurrence without accepting another user input.
use super::*;
use crate::{agent_run_event_store::ReadContextSelection, schedule_store::ClaimedContinuation};
use desk_diagnose_core::session::PersistedAgentSession;
use desk_signal_facade::model::{auth_context::AuthKind, signal::RemoteDeskTypeEnum};
use sea_orm::TransactionTrait;

pub(super) struct PreparedResume {
    pub claimed: ClaimedContinuation,
    pub original: Option<ReadContextSelection>,
    pub lease_seconds: u32,
    pub permission_request_id: Option<String>,
    pub fresh: Option<super::fresh::FreshContext>,
}

/// The dispatcher retains responsibility for durable outcome settlement. A
/// returned error never means an in-flight action is proven to have failed.
pub async fn resume_scheduled_turn(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    gate: &crate::device_assistant_gate::DeviceAssistantGate,
    target_connection_id: String,
    claimed: ClaimedContinuation,
    lease_seconds: u32,
) -> Result<LoopOutcome, AgentError> {
    if !gate.is_enabled() {
        return Err(transport_error("Device Assistant is disabled"));
    }
    let prepared = prepare(&db, claimed, lease_seconds).await?;
    let session = &prepared.claimed.session;
    {
        let map = connections.read().await;
        let mut targets = map.values().filter(|target| {
            target.auth_context.auth_kind == AuthKind::TokenAuth
                && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                && target.model.version_info.client_id.as_deref()
                    == Some(session.device_id.as_str())
        });
        if targets
            .next()
            .is_none_or(|target| target.model.connection_id != target_connection_id)
            || targets.next().is_some()
        {
            return Err(transport_error(
                "scheduled target is unavailable or ambiguous",
            ));
        }
    }
    let ask = DeviceAssistantAsk {
        question: desk_diagnose_core::permission_resume::latest_user_requirement(
            &session.conversation,
        )
        .ok_or_else(|| transport_error("original scheduled input missing"))?
        .text
        .clone(),
        conversation_id: session.client_conversation_id.clone(),
        client_message_id: prepared.claimed.run.run_id.clone(),
        ..Default::default()
    };
    compose_turn(
        connections,
        db,
        prepared.claimed.run.run_id.clone(),
        String::new(),
        target_connection_id,
        prepared.claimed.run.owner_user_id,
        session.device_id.clone(),
        ask,
        None,
        Some(prepared),
    )
    .await?
    .ok_or_else(|| transport_error("scheduled continuation did not pass runtime preflight"))
}

pub(super) async fn prepare(
    db: &DatabaseConnection,
    mut claimed: ClaimedContinuation,
    lease_seconds: u32,
) -> Result<PreparedResume, AgentError> {
    if claimed.run.owner_user_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID
        || !(30..=300).contains(&lease_seconds)
    {
        return Err(transport_error(
            "invalid scheduled continuation owner or lease",
        ));
    }
    let txn = db
        .begin()
        .await
        .map_err(|_| transport_error("scheduled input storage unavailable"))?;
    let row = crate::schedule_store::lock_action_session(&txn, &claimed.run.conversation_id)
        .await
        .map_err(|_| transport_error("scheduled continuation is no longer current"))?
        .ok_or_else(|| transport_error("scheduled session missing"))?;
    let current = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| transport_error("invalid scheduled session"))?;
    if current.turn_state != desk_diagnose_core::session::TurnState::Running
        || current.trigger_origin != TriggerOrigin::ScheduledContinuation
        || current.current_request_id.as_deref() != Some(claimed.run.run_id.as_str())
        || current.current_turn_id.as_deref() != Some(claimed.run.turn_id.as_str())
        || current.actor_id != claimed.run.owner_user_id.to_string()
        || current.policy_revision != PERSONAL_ASSISTANT_POLICY_REVISION
        || current.version != claimed.session.version
        || current.lease_token != claimed.session.lease_token
        || current.input_revision != claimed.session.input_revision
        || current.conversation_id != claimed.session.conversation_id
        || current.actor_id != claimed.session.actor_id
        || current.device_id != claimed.session.device_id
    {
        return Err(transport_error(
            "scheduled claim no longer matches original input",
        ));
    }
    let original = crate::agent_run_event_store::input_context::original_on(&txn, &current)
        .await?
        .ok_or_else(|| transport_error("original scheduled read selection missing"))?;
    let permission_request_id =
        crate::agent_session_store::permission_resume::scheduled_decision_on(&txn, &current)
            .await?;
    txn.commit()
        .await
        .map_err(|_| transport_error("scheduled input storage unavailable"))?;
    claimed.session = current;
    Ok(PreparedResume {
        claimed,
        original: Some(original),
        lease_seconds,
        permission_request_id,
        fresh: None,
    })
}
