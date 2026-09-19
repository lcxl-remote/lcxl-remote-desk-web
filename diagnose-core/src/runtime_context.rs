//! Ephemeral server state, separate from stable instructions and durable history.
use crate::chat::{ChatMessage, ChatRole};

pub const MESSAGE_ID: &str = "assistant-runtime-context";

pub fn is_runtime(message: &ChatMessage) -> bool {
    message.role == ChatRole::SystemEvent && message.message_id == MESSAGE_ID
}

/// Return the initial server snapshot for fresh construction on every request.
pub fn take(system: &mut ChatMessage) -> ChatMessage {
    system
        .runtime_context
        .take()
        .map(|value| *value)
        .unwrap_or_else(|| ChatMessage::system_event(MESSAGE_ID, ""))
}

/// Assistant state stays at the request tail; diagnostic-only prompts retain
/// their dedicated system contract.
pub fn append_instruction(system: &mut ChatMessage, text: &str) {
    if let Some(runtime) = system.runtime_context.as_mut() {
        runtime.text.push_str(text);
    } else {
        system.text.push_str(text);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn static_instructions_ignore_locale_and_authorization() {
        let mut first = crate::ai_assistant::build_ai_assistant_system_message_with_catalog(
            Some("zh-CN"),
            "grant A",
        );
        let mut second = crate::ai_assistant::build_ai_assistant_system_message_with_catalog(
            Some("en-US"),
            "grant B",
        );
        assert_eq!(first.text, second.text);
        assert_ne!(super::take(&mut first).text, super::take(&mut second).text);
        assert!(first.runtime_context.is_none());
        assert!(!serde_json::to_string(&second).unwrap().contains("grant B"));
    }
    #[test]
    fn exact_authorization_tail_retains_destination_expiry_and_checks_egress() {
        use crate::{chat::ChatRole, prompt::ResponseFormatSpec, seam::ModelRequest};
        use desk_agent_protocol::data_lineage::DestinationIdentity;
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let mut system = crate::permission_resume::bind_exact_authorization_system_message(
            crate::ai_assistant::build_ai_assistant_system_message_with_catalog(
                None,
                "exact private input",
            ),
            destination.clone(),
            100_000,
        )
        .unwrap();
        assert!(system.data_envelope.is_none());
        assert!(!system.text.contains("exact private input"));
        let runtime = super::take(&mut system);
        assert_eq!(runtime.role, ChatRole::SystemEvent);
        assert_eq!(
            runtime
                .data_envelope
                .as_ref()
                .unwrap()
                .retention
                .expires_at_unix_ms,
            Some(100_000)
        );
        let runtime =
            crate::permission_resume::rebind_exact_authorization_system_message(runtime).unwrap();
        let policy = crate::model_egress::ModelEgressPolicy {
            destination,
            selected_source_tools: Default::default(),
            export_authorization_id: "test".into(),
            now_unix_ms: 1,
            byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: false,
        };
        policy
            .authorize_request(ModelRequest::text_only(
                vec![system, runtime],
                ResponseFormatSpec::None,
            ))
            .unwrap();
    }
}
