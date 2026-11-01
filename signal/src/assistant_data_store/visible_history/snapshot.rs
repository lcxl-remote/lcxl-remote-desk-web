//! Admit a read exactly once as content owned by the current conversation.
use super::*;
use desk_diagnose_core::ai_assistant::{
    data_access::{AssistantDataToolAccess, scope_sha256},
    read_projection::{ReadProjectionKind, request_sha256},
};

pub(super) fn prepare_on<'a>(
    db: &'a DatabaseTransaction,
    session: &'a PersistedAgentSession,
    message: &'a ChatMessage,
    policy: &'a AssistantDataPolicy,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Draft, AssistantStoreError>> + Send + 'a>,
> {
    desk_diagnose_core::future::send_phase(|| async move {
        let label = message
            .data_envelope
            .as_ref()
            .ok_or(AssistantStoreError::Inaccessible)?;
        let id = label
            .provenance
            .source_object_id
            .as_deref()
            .ok_or(AssistantStoreError::Inaccessible)?;
        let stored = super::super::projection::load_on(db, &session.actor_id, id)
            .await?
            .ok_or(AssistantStoreError::Inaccessible)?;
        let proof = &stored.proof;
        let now = unsigned(chrono::Utc::now().timestamp_millis())?;
        let data = session
            .assistant_data
            .as_ref()
            .ok_or(AssistantStoreError::Inaccessible)?;
        proof
            .validate_cached_result(&message.text)
            .map_err(AssistantStoreError::Invalid)?;
        if stored.row.revoked_at_ms.is_some()
            || !proof.preparation_available(now, None)
            || proof.kind != ReadProjectionKind::Tool
            || message.role != ChatRole::Tool
            || message.tool_ok != Some(true)
            || proof.actor_id != session.actor_id
            || proof.root_conversation_id != session.conversation_id
            || proof.source_device_ref != data.source_device_ref
            || proof.actor_input_seq != data.current_actor_input_seq
            || proof.input_revision != session.input_revision
            || proof.task_id != data.retrieval_task_id
            || proof.usage.retrieval_episode_id != data.retrieval_budget.episode_id
            || proof.usage.scope_sha256
                != scope_sha256(session).map_err(AssistantStoreError::Invalid)?
            || proof.tool_call_id.as_deref() != message.tool_call_id.as_deref()
            || proof
                .snapshot_envelope()
                .map_err(AssistantStoreError::Invalid)?
                != *label
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let producer = session
            .conversation
            .iter()
            .find(|source| Some(source.message_id.as_str()) == proof.producer_message_id.as_deref())
            .ok_or(AssistantStoreError::Inaccessible)?;
        let output = producer
            .data_envelope
            .as_ref()
            .ok_or(AssistantStoreError::Inaccessible)?;
        let call = producer
            .tool_calls
            .iter()
            .find(|call| Some(call.id.as_str()) == proof.tool_call_id.as_deref())
            .ok_or(AssistantStoreError::Inaccessible)?;
        if Some(output.envelope_id.as_str()) != proof.producer_output_envelope_id.as_deref()
            || call.name != proof.tool_name
            || request_sha256(&call.name, &call.arguments_json)
                .map_err(AssistantStoreError::Invalid)?
                != proof.request_sha256
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        crate::model_egress_store::SignalModelEgressStore::read_context_output_on(
            db,
            &session.conversation_id,
            output,
        )
        .await?;
        use crate::config::ConfigConnection;
        let state = super::super::reconcile_settings_on(db, &session.actor_id).await?;
        let settings = db.config_read().await.assistant_data_settings.clone();

        let access = AssistantDataToolAccess {
            settings,
            policy_revision: policy.revision,
            memory_state_generation: unsigned(state.memory_state_generation)?,
            control_generation: unsigned(state.control_generation)?,
        };
        let [destination] = label.allowed_destinations.as_slice() else {
            return Err(AssistantStoreError::Inaccessible);
        };
        super::super::model_sources::validate_on(
            db,
            &session.actor_id,
            &data.source_device_ref,
            &proof.usage,
            &access,
            data.history_access_scope.as_deref(),
            destination,
            policy,
            now,
        )
        .await?;
        let view = desk_diagnose_core::ai_assistant::retained_copy::OwnerHistoryReadValidation::for_prepared_projection(proof)
        .map_err(AssistantStoreError::Invalid)?;
        let copy = super::super::owner_copy::inspect_on(db, &view, now)
            .await?
            .ok_or(AssistantStoreError::Inaccessible)?;
        if Some(&copy.policy) != proof.snapshot_policy.as_ref() {
            return Err(AssistantStoreError::Inaccessible);
        }
        super::super::native_export::consume_projection_on(db, &stored, now).await?;
        Ok(Draft {
            native_id: message.message_id.clone(),
            content: message.text.clone(),
            envelope: label.clone(),
            policy: copy.policy,
            dependencies: Vec::new(),
            recorded_at: proof.created_at_unix_ms,
            input_seq: data.current_actor_input_seq,
            timezone: data.current_input_timezone.clone(),
            omit_policy: false,
            citations: Default::default(),
            role: "tool",
            // Read observations carry no native action-completion authority.
            kind: desk_agent_protocol::ai_assistant::source::AssistantSourceKind::AssistantMessage,
        })
    })
}
