//! Reconstruct calls skipped after a verified control pause in the same model batch.
use crate::schedule::source_graph::TaskSourceError;
use crate::{
    chat::{ChatMessage, ChatRole},
    model_egress::ModelInputLineage,
    session::PersistedAgentSession,
};

/// Each control pair was verified separately; returned request IDs preserve the
/// original receipt requirement for hosts classifying skipped calls.
type ControlLineage = (Vec<ModelInputLineage>, Vec<(String, String)>);

pub(super) fn collect(
    session: &PersistedAgentSession,
    controls: &[(String, String)],
    kind: &str,
    texts: &[&str],
) -> Result<ControlLineage, TaskSourceError> {
    let mut nodes = Vec::new();
    let mut calls = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for message in &session.conversation {
        if message.role != ChatRole::Tool || !texts.contains(&message.text.as_str()) {
            continue;
        }
        let Some(label) = &message.data_envelope else {
            return Err(TaskSourceError::MissingSource);
        };
        if label.provenance.source_provider_id != crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
            || label.provenance.source_tool_name != kind
        {
            continue;
        }
        let id = message
            .tool_call_id
            .as_deref()
            .ok_or(TaskSourceError::InvalidNode)?;
        if !seen.insert(id) {
            return Err(TaskSourceError::ConflictingNode);
        }
        let parents: Vec<&ChatMessage> = session
            .conversation
            .iter()
            .filter(|parent| {
                parent.role == ChatRole::Assistant
                    && parent.tool_calls.iter().any(|call| call.id == id)
            })
            .collect();
        if parents.len() != 1 {
            return Err(TaskSourceError::ConflictingNode);
        }
        let parent = parents[0];
        let index = parent
            .tool_calls
            .iter()
            .position(|call| call.id == id)
            .ok_or(TaskSourceError::MissingSource)?;
        if parent
            .tool_calls
            .iter()
            .filter(|call| call.id == id)
            .count()
            != 1
        {
            return Err(TaskSourceError::ConflictingNode);
        }
        let pauses: Vec<_> = parent.tool_calls[..index]
            .iter()
            .flat_map(|call| {
                controls
                    .iter()
                    .filter(move |(control, _)| control == &call.id)
            })
            .collect();
        if pauses.len() != 1
            || message.image_data_url.is_some()
            || message.background_task_id.is_some()
            || !message.tool_calls.is_empty()
        {
            return Err(TaskSourceError::ConflictingNode);
        }
        let expected = crate::model_message_labels::internal_tool_result_envelope(
            parent.data_envelope.as_ref(),
            id,
            &message.text,
            kind,
        )
        .map_err(|_| TaskSourceError::InvalidNode)?
        .ok_or(TaskSourceError::MissingSource)?;
        if &expected != label {
            return Err(TaskSourceError::ConflictingNode);
        }
        nodes.push(super::sources::source_node(label));
        calls.push((id.to_owned(), pauses[0].1.clone()));
    }
    Ok((nodes, calls))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ToolCall;
    use desk_agent_protocol::{AgentScope, ExecutionMode, data_lineage::DestinationIdentity};

    #[test]
    fn skipped_call_requires_one_prior_verified_pause_and_preserves_parent() {
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
        let mut parent = crate::model_message_labels::model_bound_user_message(
            "proposal".into(),
            "proposal".into(),
            DestinationIdentity::Model {
                connection_id: "connection".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        parent.role = ChatRole::Assistant;
        for id in ["earlier", "pause", "skipped"] {
            parent.tool_calls.push(
                ToolCall {
                    id: id.into(),
                    name: "read_system_info".into(),
                    arguments_json: "{}".into(),
                }
                .to_ref(),
            );
        }
        let text = "not executed: waiting for user permission decision";
        let kind = "permission_pause_tool_call";
        let mut output = ChatMessage::tool_result("skipped-result", "skipped", text);
        output.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            parent.data_envelope.as_ref(),
            "skipped",
            text,
            kind,
        )
        .unwrap();
        session.conversation = vec![parent, output];
        let controls = vec![("pause".into(), "request".into())];
        let (nodes, calls) = collect(&session, &controls, kind, &[text]).unwrap();
        assert_eq!(calls, vec![("skipped".into(), "request".into())]);
        assert_eq!(
            nodes[0].source_envelope_ids,
            vec![
                session.conversation[0]
                    .data_envelope
                    .as_ref()
                    .unwrap()
                    .envelope_id
                    .clone()
            ]
        );
        assert!(collect(&session, &[], kind, &[text]).is_err());
        assert!(
            collect(
                &session,
                &[("skipped".into(), "request".into())],
                kind,
                &[text]
            )
            .is_err()
        );
        assert!(
            collect(
                &session,
                &[("earlier".into(), "other".into()), controls[0].clone()],
                kind,
                &[text]
            )
            .is_err()
        );
        let mut changed = session.clone();
        changed.conversation.push(session.conversation[1].clone());
        assert!(collect(&changed, &controls, kind, &[text]).is_err());
        changed = session;
        changed.conversation[1]
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_envelope_ids
            .clear();
        assert!(collect(&changed, &controls, kind, &[text]).is_err());
    }
}
