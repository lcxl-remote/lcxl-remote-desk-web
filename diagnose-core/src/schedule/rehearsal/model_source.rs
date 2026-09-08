//! Select a model evidence scope from an authenticated frozen rehearsal.
use crate::{
    chat::{ChatMessage, ChatRole},
    session::PersistedAgentSession,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RehearsalModelSource<'a> {
    Input(&'a str),
    Turn(&'a str),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidRehearsalModelSource;

/// Callers authenticate and freeze the session before using this selector.
/// This selects historical identity only; the original successful model receipt
/// must still verify the complete output and all of its input lineage.
pub fn rehearsal_model_source<'a>(
    session: &'a PersistedAgentSession,
    original_input_id: &'a str,
    message: &'a ChatMessage,
) -> Result<RehearsalModelSource<'a>, InvalidRehearsalModelSource> {
    if message.role != ChatRole::Assistant
        || session
            .conversation
            .iter()
            .filter(|item| item.message_id == message.message_id)
            .count()
            != 1
        || !session.conversation.contains(message)
    {
        return Err(InvalidRehearsalModelSource);
    }
    rehearsal_model_turn_source(
        session,
        original_input_id,
        message
            .turn_id
            .as_deref()
            .ok_or(InvalidRehearsalModelSource)?,
    )
}

/// Select a retained compression call's scope without inventing a transcript message.
/// The caller verifies that the compression trace belongs to this frozen session.
pub fn rehearsal_model_turn_source<'a>(
    session: &'a PersistedAgentSession,
    original_input_id: &'a str,
    output_turn: &'a str,
) -> Result<RehearsalModelSource<'a>, InvalidRehearsalModelSource> {
    let inputs: Vec<_> = session
        .conversation
        .iter()
        .filter(|item| {
            item.role == ChatRole::User
                && !crate::permission_resume::is_resume_control_message(item)
        })
        .collect();
    if inputs.len() != 1
        || inputs[0].message_id != original_input_id
        || session.actor_id.trim().is_empty()
        || session.device_id.trim().is_empty()
        || session.conversation_id.trim().is_empty()
        || output_turn.trim().is_empty()
        || !session
            .conversation
            .iter()
            .any(|message| message.turn_id.as_deref() == Some(output_turn))
        || session
            .conversation
            .iter()
            .filter(|message| message.message_id == original_input_id)
            .count()
            != 1
    {
        return Err(InvalidRehearsalModelSource);
    }
    let input_turn = inputs[0]
        .turn_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .ok_or(InvalidRehearsalModelSource)?;
    Ok(if input_turn == output_turn {
        RehearsalModelSource::Input(original_input_id)
    } else {
        RehearsalModelSource::Turn(output_turn)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selects_input_or_continuation_without_trusting_transport_state() {
        let mut session = PersistedAgentSession::new(
            "conversation",
            "1",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-09-07T00:00:00Z",
        );
        let input = ChatMessage::text("input", ChatRole::User, "request").with_turn_id("first");
        let answer =
            ChatMessage::text("answer", ChatRole::Assistant, "response").with_turn_id("first");
        let continued =
            ChatMessage::text("continued", ChatRole::Assistant, "next").with_turn_id("next-turn");
        session.conversation = vec![input.clone(), answer.clone(), continued.clone()];
        session.current_request_id = Some("new-transport-id".into());
        assert_eq!(
            rehearsal_model_source(&session, "input", &answer),
            Ok(RehearsalModelSource::Input("input"))
        );
        assert_eq!(
            rehearsal_model_source(&session, "input", &continued),
            Ok(RehearsalModelSource::Turn("next-turn"))
        );
        assert_eq!(
            rehearsal_model_turn_source(&session, "input", "first"),
            Ok(RehearsalModelSource::Input("input"))
        );
        assert_eq!(
            rehearsal_model_turn_source(&session, "input", "next-turn"),
            Ok(RehearsalModelSource::Turn("next-turn"))
        );
        assert!(rehearsal_model_turn_source(&session, "input", "missing-turn").is_err());
        assert!(rehearsal_model_turn_source(&session, "input", "").is_err());
        assert!(rehearsal_model_source(&session, "other-input", &answer).is_err());
        assert!(rehearsal_model_source(&session, "input", &input).is_err());
        let mut changed = answer.clone();
        changed.text.push_str("tampered");
        assert!(rehearsal_model_source(&session, "input", &changed).is_err());
        session.conversation.push(answer.clone());
        assert!(rehearsal_model_source(&session, "input", &answer).is_err());
        session.conversation.pop();
        session
            .conversation
            .push(ChatMessage::text("other-input", ChatRole::User, "other"));
        assert!(rehearsal_model_source(&session, "input", &answer).is_err());
    }
}
