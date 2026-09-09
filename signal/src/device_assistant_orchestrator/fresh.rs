//! Isolated task entry using the original atomically claimed session.
use super::*;
use desk_diagnose_core::{
    schedule::contract::ValidatedTaskContract, session::PersistedAgentSession,
};

pub(super) struct FreshContext {
    pub contract: ValidatedTaskContract,
    pub approval_reference: Option<String>,
    pub gate: std::sync::Arc<crate::device_assistant_gate::DeviceAssistantGate>,
}

pub(crate) fn contains_tool(
    contract: &ValidatedTaskContract,
    providers: &desk_diagnose_core::provider_registry::ProviderRegistry,
    name: &str,
) -> bool {
    providers
        .capability_for_tool(name)
        .is_some_and(|capability| {
            providers
                .provider_for_capability(&capability.wire.capability_id)
                .is_some_and(|provider| {
                    contract.contract().permissions.iter().any(|rule| {
                        rule.tool_name == name
                            && rule.provider_id == provider.wire.provider_id
                            && rule.capability_id == capability.wire.capability_id
                            && rule.tool_schema_version == capability.wire.input_schema_version
                            && rule.effect == capability.wire.effect
                    })
                })
        })
}

#[allow(clippy::too_many_arguments)]
pub async fn resume_fresh_task(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    gate: std::sync::Arc<crate::device_assistant_gate::DeviceAssistantGate>,
    target_connection_id: String,
    node_id: &str,
    claimed: PersistedAgentSession,
    lease_seconds: u32,
) -> Result<LoopOutcome, AgentError> {
    if !gate.is_enabled()
        || claimed.conversation.is_empty()
        || !(30..=300).contains(&lease_seconds)
        || claimed.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
        || claimed.trigger_origin != desk_diagnose_core::session::TriggerOrigin::ScheduledTask
    {
        return Err(transport_error("invalid fresh task claim"));
    }
    let txn = crate::db::begin_write(&db, crate::entity::agent_session::Entity)
        .await
        .map_err(|_| transport_error("task storage unavailable"))?;
    let row = crate::schedule_store::lock_action_session(&txn, &claimed.conversation_id)
        .await
        .map_err(|_| transport_error("task lease changed"))?
        .ok_or_else(|| transport_error("task session missing"))?;
    let current = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| transport_error("invalid task session"))?;
    if current != claimed {
        return Err(transport_error("task claim changed"));
    }
    let authority = crate::schedule_store::fresh_action_authority_on(&txn, &current)
        .await
        .map_err(|_| transport_error("task authorization changed"))?;
    if authority.run().lease_owner.as_deref() != Some(node_id) {
        return Err(transport_error("task executor changed"));
    }
    let approval_reference = if current.version == 1 {
        if current.conversation.len() != 1 || authority.run().result_ref.is_some() {
            return Err(transport_error("invalid initial task context"));
        }
        None
    } else {
        Some(
            authority
                .run()
                .result_ref
                .as_deref()
                .filter(|value| value.starts_with("permission:") || value.starts_with("directory:"))
                .filter(|id| !id.is_empty())
                .ok_or_else(|| transport_error("missing task approval claim"))?
                .to_owned(),
        )
    };
    let prepared = super::scheduled::PreparedResume {
        claimed: crate::schedule_store::ClaimedContinuation {
            run: authority.run().clone(),
            session: current,
        },
        original: None,
        lease_seconds,
        permission_request_id: None,
        fresh: Some(FreshContext {
            contract: authority.contract().clone(),
            approval_reference,
            gate,
        }),
    };
    txn.commit()
        .await
        .map_err(|_| transport_error("task storage unavailable"))?;
    let ask = DeviceAssistantAsk {
        question: claimed.conversation[0].text.clone(),
        client_message_id: claimed.conversation_id.clone(),
        locale: claimed.response_locale.clone(),
        ..Default::default()
    };
    compose_turn(
        connections,
        db,
        claimed.conversation_id,
        String::new(),
        target_connection_id,
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        claimed.device_id,
        ask,
        None,
        Some(prepared),
    )
    .await?
    .ok_or_else(|| transport_error("fresh task preflight did not complete"))
}
