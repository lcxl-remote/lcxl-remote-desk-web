//! Generate and verify a reviewable definition; this creates no authorization.
use super::*;
use crate::entity::agent_session;
use desk_agent_protocol::schedule::contract::{TaskContract, TaskExceptionMode, TaskStepBinding};
use desk_diagnose_core::{
    chat::ChatRole,
    schedule::contract::{draft::observed_rule, validate_contract},
    session::PersistedAgentSession,
};

impl ScheduleStore {
    pub async fn generate_task_contract(
        &self,
        owner: i32,
        schedule_id: &str,
        expected_revision: i64,
    ) -> Result<TaskContract, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.revision != expected_revision
            || task.kind != "fresh_task"
            || task.active_run_id.is_some()
            || !matches!(
                task.status.as_str(),
                "draft" | "awaiting_authorization" | "paused"
            )
        {
            return Err(ScheduleStoreError::Conflict);
        }
        use sea_orm::QueryOrder;
        let source = rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::ScheduleId.eq(schedule_id))
            .order_by_desc(rehearsal::Column::Id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if source.status != "completed" || source.task_revision != task.task_revision {
            return Err(ScheduleStoreError::Conflict);
        }
        let reads = Self::read_rehearsal_reads_on(&txn, owner, &source.rehearsal_id).await?;
        let actions = Self::read_rehearsal_actions_on(&txn, owner, &source.rehearsal_id).await?;
        if !reads.unconfirmed_read_call_ids.is_empty()
            || !actions.unconfirmed_tool_call_ids.is_empty()
            || reads.session_sha256 != actions.session_sha256
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let session_row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&source.conversation_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if digest(&session_row.state_json) != actions.session_sha256 {
            return Err(ScheduleStoreError::Conflict);
        }
        let session = PersistedAgentSession::decode_json(&session_row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        // Permission proposals do not execute a provider action. Actual grants and
        // receipts are verified by the collectors; no proposal becomes a task grant.
        for id in reads
            .other_tool_call_ids
            .iter()
            .filter(|id| actions.other_tool_call_ids.contains(id))
        {
            let calls: Vec<_> = session
                .conversation
                .iter()
                .flat_map(|message| &message.tool_calls)
                .filter(|call| &call.id == id)
                .collect();
            if calls.len() != 1 || calls[0].name != "request_capability_grants" {
                return Err(ScheduleStoreError::Conflict);
            }
        }
        let mut contract = TaskContract {
            schema_version: 1,
            schedule_id: task.schedule_id.clone(),
            task_revision: task.task_revision as u64,
            contract_revision: 1,
            target_device_id: task.target_device_id.clone(),
            prompt_sha256: digest(&task.prompt),
            permissions: vec![],
            steps: vec![],
            exception_mode: TaskExceptionMode::Deny,
            budget: crate::schedule_budget_policy::read(&txn).await?.maximum,
        };
        let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
        for read in &reads.reads {
            observed_rule(&mut contract, &read.authority, None, None, &registry)
                .map_err(|_| ScheduleStoreError::Conflict)?;
        }
        let mut generated = Vec::new();
        for action in &actions.actions {
            let proposals: Vec<_> = session
                .conversation
                .iter()
                .filter(|message| {
                    message.role == ChatRole::Assistant
                        && message.turn_id.as_deref()
                            == Some(action.origin.turn_fence.turn_id.as_str())
                })
                .flat_map(|message| {
                    message
                        .tool_calls
                        .iter()
                        .filter(|call| call.id == action.origin.tool_call_id)
                        .map(move |call| (message, call))
                })
                .collect();
            if proposals.len() != 1 {
                return Err(ScheduleStoreError::Conflict);
            }
            let (message, call) = proposals[0];
            let canonical =
                desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
                    &call.name,
                    serde_json::from_str(&call.arguments_json)
                        .map_err(|_| ScheduleStoreError::Invalid)?,
                )
                .map_err(|_| ScheduleStoreError::Invalid)?;
            observed_rule(
                &mut contract,
                &action.authority,
                Some(&canonical),
                action.sent_message.as_ref().map(desk_diagnose_core::schedule::contract::draft::ObservedMessage::Sent)
                    .or_else(|| action.prepared_message.as_ref().map(desk_diagnose_core::schedule::contract::draft::ObservedMessage::Prepared)),
                &registry,
            )
            .map_err(|_| ScheduleStoreError::Conflict)?;
            let text_artifact = action
                .created_artifact
                .as_ref()
                .filter(|_| call.name == "create_text_artifact_in_selected_directory");
            if let Some(output) = text_artifact {
                let original = desk_diagnose_core::chat::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments_json: call.arguments_json.clone(),
                };
                desk_diagnose_core::schedule::contract::draft::generalize_text_artifact(
                    &mut contract,
                    &session,
                    &original,
                    &action.authority,
                    output,
                    action.completed_at,
                )
                .map_err(|_| ScheduleStoreError::Conflict)?;
            }
            if action.sent_message.is_some()
                || action.prepared_message.is_some()
                || text_artifact.is_some()
            {
                generated.push((contract.steps.len() - 1, message.message_id.clone()));
            }
        }
        for (index, message) in generated {
            let provisional =
                validate_contract(&contract).map_err(|_| ScheduleStoreError::Conflict)?;
            let sources = Self::read_rehearsal_model_sources_on(
                &txn,
                owner,
                &source.rehearsal_id,
                &provisional,
                &message,
            )
            .await?;
            let allowed_source_scopes = match &mut contract.steps[index].binding {
                TaskStepBinding::SendMessage {
                    allowed_source_scopes,
                    ..
                }
                | TaskStepBinding::ProduceTextArtifact {
                    allowed_source_scopes,
                    ..
                } => allowed_source_scopes,
                _ => return Err(ScheduleStoreError::Invalid),
            };
            *allowed_source_scopes = sources.scopes;
        }
        let validated = validate_contract(&contract).map_err(|_| ScheduleStoreError::Conflict)?;
        Self::publication_contract_scope_evidence_on(&txn, &task, &validated, &source.rehearsal_id)
            .await?;
        txn.commit().await?;
        Ok(validated.contract().clone())
    }
}
