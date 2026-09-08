//! Reconstruct control results only; never resolve a directory or repeat a file action.
use super::source_graph::TaskSourceError;
use crate::{
    chat::{ChatMessage, ChatRole, ToolCall},
    file_scope::{DirectoryConsentSource, DirectoryConsentState},
    session::{PersistedAgentSession, TriggerOrigin},
};
use sha2::{Digest, Sha256};

/// Hosts must authenticate each returned request's original immutable directory
/// receipt before applying any returned message to the fenced session.
pub fn missing_results(
    session: &PersistedAgentSession,
) -> Result<Vec<(String, ChatMessage)>, TaskSourceError> {
    if session.trigger_origin != TriggerOrigin::ScheduledTask {
        return Err(TaskSourceError::InvalidRoot);
    }
    let mut recovered = Vec::new();
    for id in session.unclosed_tool_call_ids() {
        let calls: Vec<_> = session
            .conversation
            .iter()
            .filter(|message| message.role == ChatRole::Assistant)
            .flat_map(|message| {
                message
                    .tool_calls
                    .iter()
                    .filter(|call| call.id == id)
                    .map(move |call| (message, call))
            })
            .collect();
        if calls.len() != 1 {
            return Err(TaskSourceError::ConflictingNode);
        }
        let (parent, call) = calls[0];
        if call.name != crate::directory_tools::REQUEST_DIRECTORY {
            continue;
        }
        if parent.turn_id != session.current_turn_id {
            return Err(TaskSourceError::ConflictingNode);
        }
        let input = crate::directory_tools::parse(&ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        })
        .map_err(|_| TaskSourceError::InvalidNode)?;
        let request_id = format!(
            "directory-proposal-{:x}",
            Sha256::digest(
                format!(
                    "{}:{}:{}",
                    session.conversation_id, session.input_revision, call.id
                )
                .as_bytes()
            )
        );
        let records: Vec<_> = session
            .file_scope
            .records()
            .iter()
            .filter(|record| record.proposal.request_id == request_id)
            .collect();
        if records.is_empty() {
            continue;
        }
        if records.len() != 1 {
            return Err(TaskSourceError::ConflictingNode);
        }
        let record = records[0];
        if record.proposal.source != DirectoryConsentSource::TaskContract {
            continue;
        }
        if record.state != DirectoryConsentState::Approved
            || record.proposal.requested_path != input.path
            || record.proposal.canonical_path != input.path
            || record.proposal.purpose != input.purpose
        {
            return Err(TaskSourceError::ConflictingNode);
        }
        let content = crate::directory_tools::task_approved_result(&request_id).to_string();
        let envelope = crate::model_message_labels::internal_tool_result_envelope(
            parent.data_envelope.as_ref(),
            &call.id,
            &content,
            "task_directory_resolved",
        )
        .map_err(|_| TaskSourceError::InvalidNode)?
        .ok_or(TaskSourceError::MissingSource)?;
        let message_id = format!(
            "directory-recovery-{:x}",
            Sha256::digest(request_id.as_bytes())
        );
        if session
            .conversation
            .iter()
            .any(|message| message.message_id == message_id)
        {
            return Err(TaskSourceError::ConflictingNode);
        }
        let mut message = ChatMessage::tool_result(message_id, &call.id, content);
        message.data_envelope = Some(envelope);
        recovered.push((request_id, message));
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        file_scope::{
            DirectoryProposal,
            transaction::{FileScopeMutation, FileScopeUpdate},
        },
        session::{AgentSessionSurface, TurnState},
    };
    use desk_agent_protocol::{
        AgentScope, ExecutionMode,
        computer_use::{ObjectKind, ObjectRef},
        data_lineage::DestinationIdentity,
    };

    #[test]
    fn recovery_requires_original_task_consent_and_does_not_recover_twice() {
        let mut session = PersistedAgentSession::new(
            "run",
            "owner",
            "device",
            0,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-09-07T00:00:00Z",
        );
        session.adopt_client_metadata(Some("client"), AgentSessionSurface::DeviceAssistant);
        session.trigger_origin = TriggerOrigin::ScheduledTask;
        session.turn_state = TurnState::Running;
        session.current_turn_id = Some("turn".into());
        session.input_revision = 1;
        let tool = ToolCall {
            id: "directory-call".into(),
            name: crate::directory_tools::REQUEST_DIRECTORY.into(),
            arguments_json: serde_json::json!({"path":"/reports", "purpose":"Write report"})
                .to_string(),
        };
        let mut parent = crate::model_message_labels::model_bound_user_message(
            "proposal".into(),
            "Directory proposal".into(),
            DestinationIdentity::Model {
                connection_id: "connection".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        parent.role = ChatRole::Assistant;
        parent.turn_id = Some("turn".into());
        parent.tool_calls.push(tool.to_ref());
        session.conversation.push(parent);
        let request_id = format!(
            "directory-proposal-{:x}",
            Sha256::digest(b"run:1:directory-call")
        );
        let proposal = DirectoryProposal {
            request_id: request_id.clone(),
            requested_path: "/reports".into(),
            canonical_path: "/reports".into(),
            directory: ObjectRef {
                token: "fresh-token".into(),
                snapshot_id: "snapshot".into(),
                object_kind: ObjectKind::Directory,
                expires_at: "2030-01-01T00:00:00Z".into(),
            },
            purpose: "Write report".into(),
            source: DirectoryConsentSource::TaskContract,
        };
        let update = FileScopeUpdate {
            subject: session
                .file_scope_subject("owner", "device", "run")
                .unwrap(),
            client_conversation_id: "client".into(),
            client_request_id: request_id.clone(),
            expected_revision: 0,
            mutation: FileScopeMutation::Select { proposal },
        };
        assert!(missing_results(&session).unwrap().is_empty());
        let (mut approved, _) =
            crate::file_scope::transaction::prepare(&session, &update, 1).unwrap();
        let recovered = missing_results(&approved).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].0, request_id);
        assert!(approved.scope_snapshot.granted.is_empty());
        let mut duplicate = approved.clone();
        duplicate
            .conversation
            .push(approved.conversation[0].clone());
        assert!(missing_results(&duplicate).is_err());
        let mut changed = approved.clone();
        changed.conversation[0].tool_calls[0].arguments_json =
            serde_json::json!({"path":"/other", "purpose":"Write report"}).to_string();
        assert!(missing_results(&changed).is_err());
        approved.conversation.push(recovered[0].1.clone());
        assert!(missing_results(&approved).unwrap().is_empty());
    }
}
