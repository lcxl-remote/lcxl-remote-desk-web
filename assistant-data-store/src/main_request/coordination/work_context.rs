//! Text already retained by the native work, scoped to its own model and devices.

use super::*;
use desk_agent_protocol::data_lineage::DataEnvelope;
use desk_diagnose_core::{
    model_egress::message_content_bytes,
    sink_authorizer::{DefaultSinkAuthorizer, SinkAuthorizer, SinkInput},
};

#[derive(Debug, Clone, Copy)]
pub enum ContextSelector<'a> {
    Evidence(&'a str),
    WorkHistory,
}

impl ContextSelector<'_> {
    pub fn conversation(self, scope: &ReportReadScope) -> &str {
        match self {
            Self::Evidence(_) => &scope.child_conversation_id,
            Self::WorkHistory => &scope.binding.root_conversation_id,
        }
    }

    pub fn matches_session(self, scope: &ReportReadScope, session: &PersistedAgentSession) -> bool {
        use desk_diagnose_core::session::AgentSessionSurface;
        session.surface == AgentSessionSurface::AiAssistant
            && session.conversation_id == self.conversation(scope)
            && match self {
                Self::Evidence(_) => session.agent_role.binding() == Some(&scope.binding),
                Self::WorkHistory => {
                    session.agent_role.workbench_request_id().is_some()
                        && session.agent_role.workbench_id() == scope.binding.source.workbench_id()
                        && session.delegation_group_id.as_deref() == Some(&scope.binding.group_id)
                        && session.version == scope.work_version
                }
            }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ContextRead {
    pub(super) payload: serde_json::Value,
    pub(super) labels: Vec<DataEnvelope>,
}

/// The hosts validate the session row and its current devices before projecting.
/// Reuse the sink's retention, destination and exact byte checks for each item.
pub fn project_work_context(
    session: &PersistedAgentSession,
    selector: ContextSelector<'_>,
    destination: &DestinationIdentity,
    now_ms: i64,
    byte_limit: usize,
) -> Option<ContextRead> {
    let now = u64::try_from(now_ms).ok()?;
    let item = |message: &ChatMessage| {
        let label = message.data_envelope.as_ref()?;
        if message.image_data_url.is_some()
            || !message.tool_calls.is_empty()
            || message.raw_result.is_some()
            || message.text.len() > byte_limit
        {
            return None;
        }
        let bytes = message_content_bytes(message).ok()?;
        DefaultSinkAuthorizer
            .authorize(
                destination,
                &[SinkInput {
                    envelope: label,
                    bytes: &bytes,
                }],
                now,
                byte_limit,
            )
            .ok()?;
        Some((
            serde_json::json!({
                "message_id": message.message_id,
                "role": message.role,
                "text": message.text,
            }),
            label.clone(),
        ))
    };
    match selector {
        ContextSelector::Evidence(reference) => {
            let mut matches = session.conversation.iter().filter(|message| {
                matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
                    && message
                        .data_envelope
                        .as_ref()
                        .is_some_and(|label| label.envelope_id == reference)
            });
            let message = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            let (payload, label) = item(message)?;
            Some(ContextRead {
                payload,
                labels: vec![label],
            })
        }
        ContextSelector::WorkHistory => {
            const MAX_MESSAGES: usize = 8;
            let mut labels = Vec::new();
            let mut entries = Vec::new();
            let mut bytes = 2_usize;
            let mut omitted = false;
            for message in session.conversation.iter().rev().filter(|message| {
                matches!(message.role, ChatRole::User | ChatRole::Assistant)
                    && message.tool_calls.is_empty()
            }) {
                let Some((entry, label)) = item(message) else {
                    omitted = true;
                    continue;
                };
                let size = entry.to_string().len() + 1;
                if entries.len() == MAX_MESSAGES || bytes.saturating_add(size) > byte_limit {
                    omitted = true;
                    break;
                }
                bytes += size;
                entries.push(entry);
                labels.push(label);
            }
            entries.reverse();
            Some(ContextRead {
                payload: serde_json::json!({"messages": entries, "omitted": omitted}),
                labels,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentScope, ExecutionMode, data_lineage::Sensitivity};

    #[test]
    fn context_keeps_only_retained_text_with_exact_model_authority() {
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let mut session = PersistedAgentSession::new(
            "work",
            "owner",
            "",
            1,
            AgentScope {
                granted: Vec::new(),
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-08-25T00:00:00Z",
        );
        let owner = model_bound_user_message(
            "owner-input".into(),
            "Compare both systems".into(),
            destination.clone(),
        )
        .unwrap();
        let mut evidence = model_bound_user_message(
            "evidence".into(),
            "The original observation".into(),
            destination.clone(),
        )
        .unwrap();
        evidence.role = ChatRole::Tool;
        let reference = evidence.data_envelope.as_ref().unwrap().envelope_id.clone();
        session.conversation = vec![owner, evidence];
        let selected = ContextSelector::Evidence(&reference);
        let project =
            |session: &PersistedAgentSession, selector: ContextSelector<'_>, now, size| {
                project_work_context(session, selector, &destination, now, size)
            };
        let read = project(&session, selected, 100, 4096).unwrap();
        assert_eq!(read.payload["text"], "The original observation");
        assert_eq!(read.labels[0].envelope_id, reference);
        let history = project(&session, ContextSelector::WorkHistory, 100, 4096).unwrap();
        assert_eq!(history.payload["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            history.payload["messages"][0]["text"],
            "Compare both systems"
        );
        assert!(project(&session, selected, 100, 2).is_none());
        assert!(
            project(
                &session,
                ContextSelector::Evidence("outside-request"),
                100,
                4096
            )
            .is_none()
        );
        let original = session.conversation[1].clone();
        session.conversation[1].text.push_str(" (changed)");
        assert!(project(&session, selected, 100, 4096).is_none());
        session.conversation[1] = original.clone();
        session.conversation[1]
            .data_envelope
            .as_mut()
            .unwrap()
            .retention
            .expires_at_unix_ms = Some(100);
        assert!(project(&session, selected, 100, 4096).is_none());
        session.conversation[1] = original.clone();
        session.conversation[1]
            .data_envelope
            .as_mut()
            .unwrap()
            .sensitivity = Sensitivity::Secret;
        assert!(project(&session, selected, 100, 4096).is_none());
        session.conversation[1] = original;
        session.conversation[1]
            .data_envelope
            .as_mut()
            .unwrap()
            .allowed_destinations
            .clear();
        assert!(project(&session, selected, 100, 4096).is_none());
    }
}
