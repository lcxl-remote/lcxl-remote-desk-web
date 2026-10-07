//! Delegated turns reuse the interactive capability, model and tool composition.
use super::*;
use desk_diagnose_core::subagent::runtime::RuntimeTurn;
use desk_signal_facade::model::{auth_context::AuthKind, signal::RemoteDeskTypeEnum};

pub async fn resume_subagent_turn(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    gate: &crate::ai_assistant_gate::AiAssistantGate,
    runtime: RuntimeTurn,
) -> Result<LoopOutcome, AgentError> {
    runtime.validate()?;
    let session = runtime.session();
    if !gate.is_enabled()
        || session.actor_id.parse::<i32>().ok()
            != Some(crate::control_authorizer::SINGLE_ACCOUNT_USER_ID)
    {
        return Err(transport_error(
            "delegated turn is not eligible to continue",
        ));
    }
    let target = {
        let map = connections.read().await;
        let mut targets = map.values().filter(|target| {
            target.auth_context.auth_kind == AuthKind::TokenAuth
                && target.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                && target.model.version_info.client_id.as_deref()
                    == Some(session.device_id.as_str())
        });
        let first = targets
            .next()
            .map(|target| target.model.connection_id.clone());
        if targets.next().is_some() {
            None
        } else {
            first
        }
    };
    let target = if matches!(&runtime, RuntimeTurn::ParentCompletion { .. }) {
        target.unwrap_or_default()
    } else {
        target.ok_or_else(|| transport_error("delegated device is unavailable or ambiguous"))?
    };
    let ask = AiAssistantAsk {
        question: runtime.source().owner_requirement.text.clone(),
        locale: session.response_locale.clone(),
        ..Default::default()
    };
    let device = session.device_id.clone();
    compose_turn(
        connections,
        db,
        format!("subagent-turn:{}", uuid::Uuid::new_v4()),
        String::new(),
        target,
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        device,
        ask,
        None,
        None,
        None,
        Some(runtime),
    )
    .await?
    .ok_or_else(|| transport_error("delegated turn did not pass runtime preflight"))
}
