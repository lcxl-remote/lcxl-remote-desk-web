//! Bind completed runtime observations to the task being considered for publication.
use super::super::publication::TaskRehearsalEvidence;
use super::*;
use sea_orm::DatabaseTransaction;
use std::collections::BTreeSet;

impl ScheduleStore {
    /// This proves observed execution only. Contract coverage and current policy
    /// must still be verified before creating any continuing authorization.
    pub async fn publication_rehearsal_evidence_on(
        txn: &DatabaseTransaction,
        task: &entity::Model,
        rehearsal_id: &str,
    ) -> Result<TaskRehearsalEvidence, ScheduleStoreError> {
        let source = rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(task.owner_user_id))
            .filter(rehearsal::Column::ScheduleId.eq(&task.schedule_id))
            .filter(rehearsal::Column::RehearsalId.eq(rehearsal_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if source.status != "completed"
            || source.task_revision != task.task_revision
            || source.target_device_id != task.target_device_id
            || source.prompt != task.prompt
            || source.prompt_sha256 != digest(&task.prompt)
            || source.locale != task.locale
            || source.model_id != task.model_id
            || source.answer_message_id.is_none()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let reads = Self::read_rehearsal_reads_on(txn, task.owner_user_id, rehearsal_id).await?;
        if !reads.unconfirmed_read_call_ids.is_empty() {
            return Err(ScheduleStoreError::Conflict);
        }
        let mut confirmed = BTreeSet::new();
        for read in &reads.reads {
            if !confirmed.insert(read.tool_call_id.clone()) {
                return Err(ScheduleStoreError::Conflict);
            }
        }
        let mut unclassified: BTreeSet<_> = reads.other_tool_call_ids.iter().cloned().collect();

        let actions =
            Self::read_rehearsal_actions_on(txn, task.owner_user_id, rehearsal_id).await?;
        if actions.session_sha256 != reads.session_sha256
            || !actions.unconfirmed_tool_call_ids.is_empty()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        for action in &actions.actions {
            if !confirmed.insert(action.origin.tool_call_id.clone()) {
                return Err(ScheduleStoreError::Conflict);
            }
        }
        unclassified.extend(actions.other_tool_call_ids.iter().cloned());
        unclassified.retain(|id| !confirmed.contains(id));
        if !unclassified.is_empty() {
            return Err(ScheduleStoreError::Conflict);
        }
        if rehearsal::Entity::find_by_id(source.id)
            .one(txn)
            .await?
            .as_ref()
            != Some(&source)
            || entity::Entity::find_by_id(task.id).one(txn).await?.as_ref() != Some(task)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let finished_at = source.finished_at.ok_or(ScheduleStoreError::Conflict)?;
        if finished_at > super::super::authority::authority_now(txn).await? {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(TaskRehearsalEvidence {
            rehearsal_run_id: source.rehearsal_id,
            conversation_id: source.conversation_id,
            input_revision: 1,
            evidence_sha256: digest(&json(&(
                &task.schedule_id,
                task.task_revision,
                &source.prompt_sha256,
                &reads,
                &actions,
            ))?),
            finished_at,
        })
    }
}

impl ScheduleStore {
    /// Historical coverage combines successful reads, exact actions and sealed
    /// sends with verified model sources. Current policy and budgets remain mandatory.
    pub async fn publication_contract_scope_evidence_on(
        txn: &DatabaseTransaction,
        task: &entity::Model,
        contract: &desk_diagnose_core::schedule::contract::ValidatedTaskContract,
        rehearsal_id: &str,
    ) -> Result<TaskRehearsalEvidence, ScheduleStoreError> {
        use desk_agent_protocol::schedule::contract::TaskInputConstraint;
        let definition = contract.contract();
        if definition.schedule_id != task.schedule_id
            || i64::try_from(definition.task_revision).ok() != Some(task.task_revision)
            || definition.target_device_id != task.target_device_id
            || definition.prompt_sha256 != digest(&task.prompt)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let has_generated = definition.permissions.iter().any(|rule| {
            matches!(
                rule.input,
                TaskInputConstraint::GeneratedMessage { .. }
                    | TaskInputConstraint::GeneratedTextArtifact { .. }
            )
        });
        let mut generated_sources = Vec::new();
        let mut evidence = Self::publication_rehearsal_evidence_on(txn, task, rehearsal_id).await?;
        let reads = Self::read_rehearsal_reads_on(txn, task.owner_user_id, rehearsal_id).await?;
        let mut covered = BTreeSet::new();
        for read in &reads.reads {
            if let Some(rule) = contract
                .observed_exact_rule(&task.target_device_id, &read.authority)
                .or_else(|| {
                    contract.observed_scoped_read_rule(&task.target_device_id, &read.authority)
                })
            {
                covered.insert(rule.rule_id.clone());
            }
        }
        let actions =
            Self::read_rehearsal_actions_on(txn, task.owner_user_id, rehearsal_id).await?;
        for action in &actions.actions {
            if let Some(rule) =
                contract.observed_exact_rule(&task.target_device_id, &action.authority)
            {
                covered.insert(rule.rule_id.clone());
            }
            if has_generated
                && (action.sent_message.is_some()
                    || action.prepared_message.is_some()
                    || (action.created_artifact.is_some()
                        && action.authority.tool_name
                            == "create_text_artifact_in_selected_directory"))
            {
                use crate::entity::agent_session;
                use desk_diagnose_core::{
                    chat::ChatRole, schedule::contract::TaskMessageTarget,
                    session::PersistedAgentSession,
                };
                let row = agent_session::Entity::find()
                    .filter(
                        agent_session::Column::ConversationId
                            .eq(&action.origin.turn_fence.conversation_id),
                    )
                    .one(txn)
                    .await?
                    .ok_or(ScheduleStoreError::Conflict)?;
                if digest(&row.state_json) != actions.session_sha256 {
                    return Err(ScheduleStoreError::Conflict);
                }
                let session = PersistedAgentSession::decode_json(&row.state_json)
                    .map_err(|_| ScheduleStoreError::Invalid)?;
                let proposals: Vec<_> = session
                    .conversation
                    .iter()
                    .filter(|message| {
                        message.role == ChatRole::Assistant
                            && message.turn_id.as_deref()
                                == Some(action.origin.turn_fence.turn_id.as_str())
                            && message.tool_calls.iter().any(|call| {
                                call.id == action.origin.tool_call_id
                                    && call.name == action.origin.tool_name
                            })
                    })
                    .collect();
                if proposals.len() != 1 {
                    return Err(ScheduleStoreError::Conflict);
                }
                let (mut sources, graph) = Self::read_rehearsal_model_sources_with_graph_on(
                    txn,
                    task.owner_user_id,
                    rehearsal_id,
                    contract,
                    &proposals[0].message_id,
                )
                .await?;
                if let Some(output) = action.created_artifact.as_ref().filter(|_| {
                    action.authority.tool_name == "create_text_artifact_in_selected_directory"
                }) {
                    let originals: Vec<_> = proposals[0]
                        .tool_calls
                        .iter()
                        .filter(|call| {
                            call.id == action.origin.tool_call_id
                                && call.name == action.origin.tool_name
                        })
                        .collect();
                    if originals.len() != 1 {
                        return Err(ScheduleStoreError::Conflict);
                    }
                    let original = desk_diagnose_core::chat::ToolCall {
                        id: originals[0].id.clone(),
                        name: originals[0].name.clone(),
                        arguments_json: originals[0].arguments_json.clone(),
                    };
                    if let Some(step) = contract.observed_generated_text_step(
                        &session,
                        &original,
                        &action.authority,
                        output,
                        action.completed_at,
                        &sources,
                    ) {
                        covered.insert(step.rule_id.clone());
                        generated_sources.push((
                            step.step_id.clone(),
                            action.origin.tool_call_id.clone(),
                            sources.scopes,
                            sources.root_envelope_ids,
                        ));
                    }
                    continue;
                }
                let snapshot = action
                    .sent_message
                    .as_ref()
                    .map(|sent| &sent.snapshot)
                    .or_else(|| {
                        action
                            .prepared_message
                            .as_ref()
                            .map(|prepared| &prepared.snapshot)
                    })
                    .ok_or(ScheduleStoreError::Conflict)?;
                let candidates: Vec<_> = contract
                    .contract()
                    .steps
                    .iter()
                    .filter(|step| {
                        contract.contract().permissions.iter().any(|rule| {
                            rule.rule_id == step.rule_id
                                && rule.provider_id == action.authority.provider_id
                                && rule.tool_name == action.authority.tool_name
                                && rule.effect == action.authority.effect
                        })
                    })
                    .collect();
                if candidates.is_empty() {
                    continue;
                }
                if candidates.len() != 1 {
                    return Err(ScheduleStoreError::Conflict);
                }
                let mut attachment_receipts = Vec::new();
                for attachment in &snapshot.payload.attachments {
                    let matching: Vec<_> = actions
                        .actions
                        .iter()
                        .filter(|original| {
                            original.origin.turn_fence.conversation_id == session.conversation_id
                                && original.completed_at <= snapshot.sealed_at_unix_ms
                                && original.created_artifact.as_ref().is_some_and(|artifact| {
                                    artifact.content == attachment.content
                                        && artifact.file_name == attachment.file_name
                                        && artifact.media_type == attachment.media_type
                                        && artifact.size_bytes == attachment.size_bytes
                                        && artifact.digest_sha256 == attachment.digest_sha256
                                })
                        })
                        .collect();
                    if matching.len() != 1 {
                        return Err(ScheduleStoreError::Conflict);
                    }
                    let envelope = &matching[0].receipt.envelope;
                    attachment_receipts.push(desk_diagnose_core::schedule::source_graph::attachment::TaskAttachmentReceipt {
                        run_id: &session.conversation_id, envelope_id: &envelope.envelope_id,
                        receipt_digest_sha256: &envelope.digest_sha256, attachment,
                    });
                }
                let attachment_check = contract
                    .evaluate_step_attachments(
                        &session.conversation_id,
                        &candidates[0].step_id,
                        &snapshot.payload.attachments,
                        &attachment_receipts,
                        &graph.nodes,
                        &graph.bindings,
                    )
                    .map_err(|_| ScheduleStoreError::Conflict)?;
                if attachment_check.decision == desk_diagnose_core::schedule::contract::attachment::TaskAttachmentDecision::Denied {
                    return Err(ScheduleStoreError::Conflict);
                }
                sources
                    .scopes
                    .extend(attachment_check.sources.scopes.iter().cloned());
                sources.scopes.sort();
                sources.scopes.dedup();
                sources
                    .root_envelope_ids
                    .extend(attachment_check.sources.root_envelope_ids.iter().cloned());
                sources.root_envelope_ids.sort();
                sources.root_envelope_ids.dedup();
                let step = if let Some(sent) = &action.sent_message {
                    let target = TaskMessageTarget {
                        task_device_id: &task.target_device_id,
                        provider_device_id: &sent.snapshot.payload.surface.device_id,
                    };
                    contract.observed_generated_message_step_with_attachments(
                        &target,
                        &session.conversation_id,
                        &action.authority,
                        sent,
                        &sources,
                        &attachment_check,
                    )
                } else if let Some(prepared) = &action.prepared_message {
                    let target = TaskMessageTarget {
                        task_device_id: &task.target_device_id,
                        provider_device_id: &prepared.snapshot.payload.surface.device_id,
                    };
                    contract.observed_generated_draft_step_with_attachments(
                        &target,
                        &session.conversation_id,
                        &action.authority,
                        prepared,
                        &sources,
                        &attachment_check,
                    )
                } else {
                    None
                };
                if let Some(step) = step {
                    covered.insert(step.rule_id.clone());
                    generated_sources.push((
                        step.step_id.clone(),
                        action.origin.tool_call_id.clone(),
                        sources.scopes,
                        sources.root_envelope_ids,
                    ));
                }
            }
        }
        if definition
            .permissions
            .iter()
            .any(|rule| !covered.contains(&rule.rule_id))
        {
            return Err(ScheduleStoreError::Conflict);
        }
        // Omitted exploratory observations do not add rules to the reviewed contract.
        evidence.evidence_sha256 = digest(&json(&(
            evidence.evidence_sha256,
            contract.digest(),
            generated_sources,
        ))?);
        Ok(evidence)
    }
}
