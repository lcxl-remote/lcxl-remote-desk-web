//! Resolve a generated artifact's model proposal through original run evidence.
use super::*;
use desk_diagnose_core::schedule::rehearsal::sources::{
    RehearsalCompressionCall, build_rehearsal_source_graph, retained_rehearsal_compressions,
};

pub(super) async fn sources(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    authority: &schedule_store::CurrentTaskAuthority,
    tool_id: &str,
) -> Result<Vec<String>, DbErr> {
    let works = agent_action_item::Entity::find()
        .filter(agent_action_item::Column::ConversationId.eq(&session.conversation_id))
        .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
        .all(txn)
        .await?;
    let mut reads = Vec::new();
    for work in works {
        if work.status != CAPABILITY_WORK_SUCCEEDED {
            continue;
        }
        let dispatch = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(work.id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        if dispatch.computer_binding_json.is_none() {
            continue;
        }
        let Some(observed) = SignalCapabilityGrantStore::observe_completed_task_provider_on(
            txn,
            &dispatch.dispatch_id,
            authority.provenance(),
        )
        .await?
        else {
            continue;
        };
        reads.extend(
            desk_diagnose_core::schedule::rehearsal::read_sources::verified_task_action_sources(
                session,
                authority.contract(),
                &observed.authority,
                &observed.origin,
                &observed.receipt,
                observed.created_artifact.as_ref(),
                observed.completed_at,
            )
            .map_err(|_| invalid())?,
        );
    }
    let mut calls = Vec::new();
    let mut proposal = None;
    let export_id = crate::assistant_model::model_export_id(
        &session.actor_id,
        &session.device_id,
        &session.conversation_id,
        crate::assistant_model::ModelExportSource::Turn(
            session.current_turn_id.as_deref().ok_or_else(invalid)?,
        ),
    );
    for message in &session.conversation {
        if message.tool_calls.iter().any(|call| call.id == tool_id) {
            if proposal.is_some() {
                return Err(invalid());
            }
            proposal = Some(message.message_id.clone());
        }
        if message.data_envelope.as_ref().is_some_and(|envelope| {
            envelope.provenance.source_provider_id == "external-model"
                && envelope.provenance.source_tool_name == "model-response"
        }) {
            calls.push(
                crate::model_egress_store::SignalModelEgressStore::find_output_evidence_on(
                    txn, &export_id, message,
                )
                .await?,
            );
        }
    }
    // Authenticate retained summaries against completed model calls before
    // allowing their derived content to participate in a generated message.
    let mut compressions = Vec::new();
    for trace in retained_rehearsal_compressions(session).map_err(|_| invalid())? {
        let inputs = crate::model_egress_store::SignalModelEgressStore::read_compression_inputs_on(
            txn, &export_id, &trace,
        )
        .await?;
        compressions.push(RehearsalCompressionCall { trace, inputs });
    }
    let graph = build_rehearsal_source_graph(
        session,
        &format!("{}:input", session.conversation_id),
        authority.contract(),
        &reads,
        &calls,
        &compressions,
    )
    .map_err(|_| invalid())?;
    Ok(graph
        .resolve_model_output(session, proposal.as_deref().ok_or_else(invalid)?)
        .map_err(|_| invalid())?
        .scopes)
}
