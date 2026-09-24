//! Resume a queued goal through the same live provider and authorization
//! composition as an interactive AI Assistant turn.

use super::*;
use desk_diagnose_core::goal::GoalRun;
use desk_signal_facade::model::{auth_context::AuthKind, signal::RemoteDeskTypeEnum};

pub async fn resume_queued_goal(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    gate: &crate::ai_assistant_gate::AiAssistantGate,
    goal: GoalRun,
) -> Result<LoopOutcome, AgentError> {
    if !gate.is_enabled()
        || goal.owner_id.parse::<i32>().ok()
            != Some(crate::control_authorizer::SINGLE_ACCOUNT_USER_ID)
        || goal.state != desk_diagnose_core::goal::GoalState::Queued
    {
        return Err(transport_error("goal is not eligible to continue"));
    }
    let target = {
        let map = connections.read().await;
        let mut targets = map.values().filter(|target| {
            target.auth_context.auth_kind == AuthKind::TokenAuth
                && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                && target.model.version_info.client_id.as_deref() == Some(goal.device_id.as_str())
        });
        let first = targets
            .next()
            .map(|target| target.model.connection_id.clone());
        if targets.next().is_some() {
            None
        } else {
            first
        }
    }
    .ok_or_else(|| transport_error("goal device is unavailable or ambiguous"))?;
    let request_id = format!(
        "goal-{}-slice-{}",
        goal.goal_id,
        goal.slice_seq.saturating_add(1)
    );
    let ask = AiAssistantAsk {
        question: goal.goal_text.clone(),
        client_message_id: goal.source_message_id.clone(),
        ..Default::default()
    };
    compose_turn(
        connections,
        db,
        request_id,
        String::new(),
        target,
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        goal.device_id.clone(),
        ask,
        None,
        None,
        Some(goal),
    )
    .await?
    .ok_or_else(|| transport_error("goal continuation did not pass runtime preflight"))
}
