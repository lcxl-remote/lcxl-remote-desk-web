//! Bind generated sends to this run's verified draft and original model inputs.
use super::*;
use desk_agent_protocol::{
    communication::{
        GmailWebDraftHandoffInput, GmailWebExactSendInput, SlackWebDraftHandoffInput,
        SlackWebExactSendInput,
    },
    schedule::contract::{TaskGeneratedMessage, TaskMessageDestination},
};
use desk_diagnose_core::chat::ToolCall;
use desk_diagnose_core::schedule::{
    contract::{TaskMessageTarget, task_message_resource_scope},
    rehearsal::sources::{
        RehearsalCompressionCall, build_rehearsal_source_graph, retained_rehearsal_compressions,
    },
};

pub(super) struct MessageBinding {
    pub input: serde_json::Value,
    pub destination: TaskMessageDestination,
    pub resources: Vec<String>,
    pub sources: Vec<String>,
    pub attachment_decision:
        desk_diagnose_core::schedule::contract::attachment::TaskAttachmentDecision,
}

pub(super) async fn bind(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    authority: &schedule_store::CurrentTaskAuthority,
    tool: &ToolCall,
    step_id: &str,
) -> Result<MessageBinding, DbErr> {
    let (send, draft, slack_send, slack_draft) = match tool.name.as_str() {
        "send_gmail_web_exact" => {
            let input: GmailWebExactSendInput =
                serde_json::from_str(&tool.arguments_json).map_err(|_| invalid())?;
            desk_diagnose_core::communication::verify_gmail_web_exact_send_input(&input)
                .map_err(|_| invalid())?;
            (Some(input), None, None, None)
        }
        "prepare_gmail_web_draft_handoff" => {
            let input: GmailWebDraftHandoffInput =
                serde_json::from_str(&tool.arguments_json).map_err(|_| invalid())?;
            input.validate().map_err(|_| invalid())?;
            (None, Some(input), None, None)
        }
        "send_slack_web_exact" => {
            let input: SlackWebExactSendInput =
                serde_json::from_str(&tool.arguments_json).map_err(|_| invalid())?;
            desk_diagnose_core::communication::verify_slack_web_exact_send_input(&input)
                .map_err(|_| invalid())?;
            (None, None, Some(input), None)
        }
        "prepare_slack_web_message_handoff" => {
            let input: SlackWebDraftHandoffInput =
                serde_json::from_str(&tool.arguments_json).map_err(|_| invalid())?;
            input.validate().map_err(|_| invalid())?;
            (None, None, None, Some(input))
        }
        _ => return Err(invalid()),
    };
    let snapshot = send
        .as_ref()
        .and_then(|input| input.handoff.send_payload_snapshot.as_ref())
        .or_else(|| {
            slack_send
                .as_ref()
                .and_then(|input| input.handoff.send_payload_snapshot.as_ref())
        });
    let message = if let Some(document) = send
        .as_ref()
        .map(|input| &input.draft)
        .or_else(|| draft.as_ref().map(|input| &input.draft))
    {
        TaskGeneratedMessage {
            subject: document.subject.clone(),
            body: document.body_plain_text.clone(),
        }
    } else {
        TaskGeneratedMessage {
            subject: String::new(),
            body: slack_send
                .as_ref()
                .map(|input| &input.body_plain_text)
                .or_else(|| slack_draft.as_ref().map(|input| &input.body_plain_text))
                .ok_or_else(invalid)?
                .clone(),
        }
    };
    let works = agent_action_item::Entity::find()
        .filter(agent_action_item::Column::ConversationId.eq(&session.conversation_id))
        .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
        .all(txn)
        .await?;
    let mut reads = Vec::new();
    let mut artifacts = Vec::new();
    let mut artifact_envelopes = Vec::new();
    let mut prepared = None;
    let mut verified_page = None;
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
        if let Some(artifact) = &observed.created_artifact {
            artifacts.push(artifact.clone());
            artifact_envelopes.push(observed.receipt.envelope.clone());
        }
        if let Some(draft) = &observed.prepared_message
            && snapshot == Some(&draft.snapshot)
        {
            if prepared.is_some() {
                return Err(invalid());
            }
            prepared = Some(draft.clone());
        }
        let draft_page = draft
            .as_ref()
            .map(|input| &input.page)
            .or_else(|| slack_draft.as_ref().map(|input| &input.page));
        if let (Some(input_page), Some(page)) = (draft_page, &observed.browser_page)
            && input_page == page
        {
            verified_page = Some(page.clone());
        }
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
    let target = TaskMessageTarget {
        task_device_id: &session.device_id,
        provider_device_id: &session.device_id,
    };
    let mut requested_attachments = Vec::new();
    // These objects came only from verified completed actions in this conversation.
    if let Some(attachment) = draft.as_ref().and_then(|input| input.attachment.as_ref()) {
        let requested = desk_agent_protocol::communication::ImmutableAttachmentSnapshot {
            content: attachment.artifact.content.clone(),
            file_name: attachment.artifact.file_name.clone(),
            media_type: attachment.artifact.media_type.clone(),
            size_bytes: attachment.artifact.size_bytes,
            digest_sha256: attachment.artifact.digest_sha256.clone(),
        };
        let original =
            desk_diagnose_core::schedule::source_graph::attachment::unique_attachment_artifact(
                &requested, &artifacts,
            )
            .map_err(|_| invalid())?;
        if original != &attachment.artifact {
            return Err(invalid());
        }
        requested_attachments.push(requested);
    }
    if let Some(snapshot) = snapshot {
        requested_attachments = snapshot.payload.attachments.clone();
        for attachment in &snapshot.payload.attachments {
            desk_diagnose_core::schedule::source_graph::attachment::unique_attachment_artifact(
                attachment, &artifacts,
            )
            .map_err(|_| invalid())?;
        }
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
        if message.tool_calls.iter().any(|call| call.id == tool.id) {
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
    let mut attachment_receipts = Vec::new();
    for attachment in &requested_attachments {
        let original =
            desk_diagnose_core::schedule::source_graph::attachment::unique_attachment_artifact(
                attachment, &artifacts,
            )
            .map_err(|_| invalid())?;
        let index = artifacts
            .iter()
            .position(|candidate| candidate == original)
            .ok_or_else(invalid)?;
        let envelope = artifact_envelopes.get(index).ok_or_else(invalid)?;
        attachment_receipts.push(
            desk_diagnose_core::schedule::source_graph::attachment::TaskAttachmentReceipt {
                run_id: &session.conversation_id,
                envelope_id: &envelope.envelope_id,
                receipt_digest_sha256: &envelope.digest_sha256,
                attachment,
            },
        );
    }
    let attachment_check = authority
        .contract()
        .evaluate_step_attachments(
            &session.conversation_id,
            step_id,
            &requested_attachments,
            &attachment_receipts,
            &graph.nodes,
            &graph.bindings,
        )
        .map_err(|_| invalid())?;
    if attachment_check.decision
        == desk_diagnose_core::schedule::contract::attachment::TaskAttachmentDecision::Denied
    {
        return Err(invalid());
    }
    let destination = if let Some(input) = &draft {
        authority
            .contract()
            .verify_generated_gmail_draft_with_attachments(
                &target,
                step_id,
                &session.conversation_id,
                &attachment_check,
                input,
                verified_page.as_ref().ok_or_else(invalid)?,
            )
            .map_err(|_| invalid())?
            .clone()
    } else if let Some(input) = &slack_draft {
        authority
            .contract()
            .verify_generated_slack_draft(
                &target,
                step_id,
                input,
                verified_page.as_ref().ok_or_else(invalid)?,
            )
            .map_err(|_| invalid())?
            .clone()
    } else {
        let prepared = prepared.ok_or_else(invalid)?;
        if prepared.subject != message.subject || prepared.body_plain_text != message.body {
            return Err(invalid());
        }
        authority
            .contract()
            .verify_generated_send_snapshot_with_attachments(
                &target,
                &session.conversation_id,
                step_id,
                snapshot.ok_or_else(invalid)?,
                &prepared.snapshot.payload.surface,
                &message,
                &attachment_check,
            )
            .map_err(|_| invalid())?
            .clone()
    };
    let mut sources = graph
        .resolve_model_output(session, proposal.as_deref().ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    sources.scopes.extend(attachment_check.sources.scopes);
    sources.scopes.sort();
    sources.scopes.dedup();
    Ok(MessageBinding {
        input: serde_json::to_value(message).map_err(|_| invalid())?,
        resources: task_message_resource_scope(&session.device_id, &destination)
            .map_err(|_| invalid())?,
        destination,
        sources: sources.scopes,
        attachment_decision: attachment_check.decision,
    })
}
