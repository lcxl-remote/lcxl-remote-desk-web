//! Join verified read receipts to the original content in the frozen rehearsal.
use super::*;
use crate::entity::agent_session;
pub use desk_diagnose_core::schedule::rehearsal::read_sources::RehearsalToolSource;
use desk_diagnose_core::{
    schedule::rehearsal::read_sources::{ReadSourceDigest, verified_read_source},
    session::PersistedAgentSession,
};

impl ScheduleStore {
    /// Historical source evidence only. The caller must verify all upstream
    /// model/transform nodes and current policy before publishing any contract.
    pub async fn read_rehearsal_read_sources_on(
        txn: &sea_orm::DatabaseTransaction,
        owner: i32,
        rehearsal_id: &str,
    ) -> Result<Vec<RehearsalToolSource>, ScheduleStoreError> {
        // This locks and verifies the owner-bound frozen session and each receipt.
        let report = Self::read_rehearsal_reads_on(txn, owner, rehearsal_id).await?;
        if !report.unconfirmed_read_call_ids.is_empty() {
            return Err(ScheduleStoreError::Conflict);
        }
        let source = rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::RehearsalId.eq(rehearsal_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&source.conversation_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if digest(&row.state_json) != report.session_sha256 {
            return Err(ScheduleStoreError::Conflict);
        }
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let mut sources = Vec::new();
        for read in &report.reads {
            use crate::entity::agent_capability_dispatch_outbox as outbox;
            let dispatch = outbox::Entity::find()
                .filter(outbox::Column::CallId.eq(&read.call_id))
                .one(txn)
                .await?
                .ok_or(ScheduleStoreError::Conflict)?;
            let source = if dispatch.computer_binding_json.is_some() {
                let observed = crate::capability_grant_store::SignalCapabilityGrantStore::observe_completed_provider_on(
                    txn, &dispatch.dispatch_id,
                ).await?.ok_or(ScheduleStoreError::Conflict)?;
                sources.extend(desk_diagnose_core::schedule::rehearsal::read_sources::verified_action_status_sources(
                    &session.conversation, &observed.authority, &observed.origin, &observed.receipt,
                ).map_err(|_| ScheduleStoreError::Conflict)?);
                desk_diagnose_core::schedule::rehearsal::read_sources::verified_action_source(
                    &session.conversation, &observed.authority, &observed.origin, &observed.receipt,
                )
            } else {
                verified_read_source(&session.conversation, read, ReadSourceDigest::ModelPayload)
            }.map_err(|_| ScheduleStoreError::Conflict)?;
            sources.push(source);
        }
        Ok(sources)
    }
}

impl ScheduleStore {
    /// Resolve historical model sources inside the caller's publication transaction.
    /// This does not approve a destination, issue grants, or replace current policy.
    pub async fn read_rehearsal_model_sources_on(
        txn: &sea_orm::DatabaseTransaction,
        owner: i32,
        rehearsal_id: &str,
        contract: &desk_diagnose_core::schedule::contract::ValidatedTaskContract,
        output_message_id: &str,
    ) -> Result<desk_diagnose_core::schedule::source_graph::ResolvedTaskSources, ScheduleStoreError>
    {
        Self::read_rehearsal_model_sources_with_graph_on(
            txn,
            owner,
            rehearsal_id,
            contract,
            output_message_id,
        )
        .await
        .map(|(sources, _)| sources)
    }

    pub async fn read_rehearsal_model_sources_with_graph_on(
        txn: &sea_orm::DatabaseTransaction,
        owner: i32,
        rehearsal_id: &str,
        contract: &desk_diagnose_core::schedule::contract::ValidatedTaskContract,
        output_message_id: &str,
    ) -> Result<
        (
            desk_diagnose_core::schedule::source_graph::ResolvedTaskSources,
            desk_diagnose_core::schedule::rehearsal::sources::RehearsalSourceGraph,
        ),
        ScheduleStoreError,
    > {
        use desk_diagnose_core::schedule::rehearsal::sources::build_rehearsal_source_graph;
        // The read collector locks the task and frozen session and verifies every read.
        let mut reads = Self::read_rehearsal_read_sources_on(txn, owner, rehearsal_id).await?;
        let source = rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::RehearsalId.eq(rehearsal_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(&source.schedule_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let definition = contract.contract();
        if definition.schedule_id != source.schedule_id
            || i64::try_from(definition.task_revision).ok() != Some(source.task_revision)
            || task.task_revision != source.task_revision
            || definition.target_device_id != source.target_device_id
            || task.target_device_id != source.target_device_id
            || definition.prompt_sha256 != source.prompt_sha256
            || digest(&source.prompt) != source.prompt_sha256
            || task.prompt != source.prompt
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&source.conversation_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if source.completed_session_sha256.as_deref() != Some(digest(&row.state_json).as_str())
            || source.completed_session_version != Some(row.version)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let actions = Self::read_rehearsal_actions_on(txn, owner, rehearsal_id).await?;
        if actions.session_sha256 != digest(&row.state_json)
            || !actions.unconfirmed_tool_call_ids.is_empty()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        for action in &actions.actions {
            reads.extend(desk_diagnose_core::schedule::rehearsal::read_sources::verified_task_action_sources(
                &session, contract, &action.authority, &action.origin, &action.receipt,
                action.created_artifact.as_ref(), action.completed_at,
            ).map_err(|_| ScheduleStoreError::Conflict)?);
        }
        let input_id = format!("rehearsal:{}:input", source.rehearsal_id);
        let mut calls = Vec::new();
        for message in &session.conversation {
            if message.data_envelope.as_ref().is_some_and(|envelope| {
                envelope.provenance.source_provider_id == "external-model"
                    && envelope.provenance.source_tool_name == "model-response"
            }) {
                calls.push(crate::model_egress_store::SignalModelEgressStore::find_rehearsal_output_evidence_on(txn, &session, &input_id, message).await?);
            }
        }
        use desk_diagnose_core::schedule::rehearsal::{
            model_source::{RehearsalModelSource, rehearsal_model_turn_source},
            sources::{RehearsalCompressionCall, retained_rehearsal_compressions},
        };
        let mut compressions = Vec::new();
        for trace in
            retained_rehearsal_compressions(&session).map_err(|_| ScheduleStoreError::Conflict)?
        {
            let origin =
                rehearsal_model_turn_source(&session, &input_id, &trace.compressor.created_turn_id)
                    .map_err(|_| ScheduleStoreError::Conflict)?;
            use crate::assistant_model::{ModelExportSource, model_export_id};
            let origin = match origin {
                RehearsalModelSource::Input(id) => ModelExportSource::Input(id),
                RehearsalModelSource::Turn(id) => ModelExportSource::Turn(id),
            };
            let export_id = model_export_id(
                &session.actor_id,
                &session.device_id,
                &session.conversation_id,
                origin,
            );
            let inputs =
                crate::model_egress_store::SignalModelEgressStore::read_compression_inputs_on(
                    txn, &export_id, &trace,
                )
                .await?;
            compressions.push(RehearsalCompressionCall { trace, inputs });
        }
        let graph = build_rehearsal_source_graph(
            &session,
            &input_id,
            contract,
            &reads,
            &calls,
            &compressions,
        )
        .map_err(|_| ScheduleStoreError::Conflict)?;
        let sources = graph
            .resolve_model_output(&session, output_message_id)
            .map_err(|_| ScheduleStoreError::Conflict)?;
        Ok((sources, graph))
    }
}
