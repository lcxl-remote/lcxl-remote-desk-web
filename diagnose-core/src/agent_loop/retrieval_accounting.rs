//! One durable read/search account shared by legacy and cross-conversation tools.

use super::*;
use crate::ai_assistant::history::{
    ReservationAdmission, RetrievalCallKind, serialized_tool_response_bytes,
};
use crate::chat::ToolCall;

pub(super) struct RetrievalCall {
    pub id: Option<String>,
    pub upper_bytes: u32,
}

pub(super) enum Admission {
    New(RetrievalCall),
    Replay(Box<ChatMessage>),
}

pub(super) struct LegacyToolResult {
    pub message: Box<ChatMessage>,
    pub ok: bool,
    pub replayed: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_legacy(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    call: &ToolCall,
    producer_message_id: &str,
    result_message_id: String,
    parent: Option<&DataEnvelope>,
    attachment_read_budget: &mut usize,
    observation: &crate::model_observability::tool::ToolObservation,
) -> Result<LegacyToolResult, AgentError> {
    let reservation = match reserve(deps, session, producer_message_id, call).await {
        Ok(Admission::New(reservation)) => reservation,
        Ok(Admission::Replay(message)) => {
            let ok = message.tool_ok.unwrap_or(true);
            return Ok(LegacyToolResult {
                message,
                ok,
                replayed: true,
            });
        }
        Err(error) if error.safe_for_model => {
            return Ok(LegacyToolResult {
                message: Box::new(legacy_error(
                    session,
                    call,
                    &result_message_id,
                    parent,
                    &error.message,
                )?),
                ok: false,
                replayed: false,
            });
        }
        Err(error) => return Err(error),
    };
    let result = if call.name == crate::conversation_attachment::READ_ATTACHMENT_TOOL {
        let maximum = reservation.id.as_ref().map(|_| reservation.upper_bytes);
        crate::future::boxed(|| {
            read_conversation_attachment(
                deps,
                session,
                call,
                parent,
                attachment_read_budget,
                maximum,
                observation,
            )
        })
        .await
    } else {
        crate::conversation_history::load_history_page(
            call,
            &session.conversation,
            deps.model.model_egress_policy()?.as_ref(),
            current_unix_ms(deps.clock)?,
        )
        .and_then(|result| {
            session
                .focus_epoch
                .record_history_result(result.content.len())
                .map_err(invalid)?;
            let mut message =
                ChatMessage::tool_result(&result_message_id, &call.id, result.content);
            message.data_envelope =
                crate::model_message_labels::conversation_history_result_envelope(
                    parent,
                    &result.source_messages,
                    &call.id,
                    &message.text,
                )?;
            Ok(message)
        })
    };
    observation.preflight_result(&result);
    let (mut message, mut ok) = match result {
        Ok(mut message) => {
            message.message_id = result_message_id.clone();
            message.tool_call_id = Some(call.id.clone());
            (message, true)
        }
        Err(error) if error.safe_for_model => (
            legacy_error(session, call, &result_message_id, parent, &error.message)?,
            false,
        ),
        Err(error) => return Err(error),
    };
    message.turn_id = session.current_turn_id.clone();
    if !fits(&reservation, &message) {
        message = legacy_error(
            session,
            call,
            &result_message_id,
            parent,
            "Response exceeds the framed byte limit; narrow the source window. No source content was returned.",
        )?;
        ok = false;
    }
    settle(session, &reservation, &message)?;
    message.tool_ok = Some(ok);
    Ok(LegacyToolResult {
        message: Box::new(message),
        ok,
        replayed: false,
    })
}

fn legacy_error(
    session: &PersistedAgentSession,
    call: &ToolCall,
    message_id: &str,
    parent: Option<&DataEnvelope>,
    reason: &str,
) -> Result<ChatMessage, AgentError> {
    let mut message = ChatMessage::tool_result(
        message_id,
        &call.id,
        crate::model_input::describe_error(&call.name, reason),
    );
    message.tool_ok = Some(false);
    message.data_envelope = crate::model_message_labels::conversation_history_result_envelope(
        parent,
        &[],
        &call.id,
        &message.text,
    )?;
    message.turn_id = session.current_turn_id.clone();
    Ok(message)
}

pub(super) fn kind(call: &ToolCall) -> Option<RetrievalCallKind> {
    crate::ai_assistant::data_tools::retrieval_call_kind(call).or({
        match call.name.as_str() {
            crate::conversation_history::LOAD_CONVERSATION_HISTORY_TOOL_NAME
            | crate::conversation_attachment::READ_ATTACHMENT_TOOL => Some(RetrievalCallKind::Read),
            _ => None,
        }
    })
}

pub(super) async fn reserve(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    producer_message_id: &str,
    call: &ToolCall,
) -> Result<Admission, AgentError> {
    let kind = kind(call).ok_or_else(|| invalid("tool has no retrieval accounting class"))?;
    let upper_bytes = session
        .assistant_data
        .as_ref()
        .map(|data| data.retrieval_budget.limits.response_bytes)
        .unwrap_or_else(|| {
            desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits::default()
                .response_bytes
        });
    if session.surface != AgentSessionSurface::AiAssistant || !session.agent_role.is_main() {
        return Ok(Admission::New(RetrievalCall {
            id: None,
            upper_bytes,
        }));
    }
    desk_agent_protocol::ai_assistant::source::validate_source_id(&call.id).map_err(invalid)?;

    let now = current_unix_ms(deps.clock)?;
    let data = session
        .assistant_data
        .as_mut()
        .ok_or_else(|| invalid("assistant retrieval accounting is unavailable"))?;
    let id = crate::native_ids::stable(
        crate::native_ids::Kind::AssistantReadAuthorization,
        &(
            &session.actor_id,
            &session.conversation_id,
            &data.retrieval_budget.episode_id,
            producer_message_id,
            &call.id,
        ),
    );
    let hash = format!(
        "{:x}",
        sha2::Sha256::digest(
            serde_json::to_vec(&(&call.name, &call.arguments_json))
                .map_err(|_| invalid("invalid retrieval input"))?
        )
    );
    let upper_bytes = if let Some(previous) = data
        .retrieval_budget
        .reservations
        .iter()
        .find(|entry| entry.logical_call_id == id)
    {
        previous.reserved_bytes
    } else if kind == RetrievalCallKind::Read {
        let remaining = data
            .retrieval_budget
            .limits
            .read_bytes
            .saturating_sub(data.retrieval_budget.read_bytes);
        if remaining == 0 {
            return Err(invalid(
                "history read byte budget exhausted; report the unexpanded scope as partial",
            ));
        }
        upper_bytes.min(u32::try_from(remaining).unwrap_or(u32::MAX))
    } else {
        upper_bytes
    };
    let admission = data
        .retrieval_budget
        .reserve(&id, &hash, kind, upper_bytes, now)
        .map_err(invalid)?;
    if admission == ReservationAdmission::Settled {
        let index = session
            .conversation
            .iter()
            .position(|message| message.message_id == producer_message_id)
            .ok_or_else(|| invalid("retrieval producer is unavailable"))?;
        let result = session.conversation[index + 1..]
            .iter()
            .take_while(|message| message.role == ChatRole::Tool)
            .find(|message| message.tool_call_id.as_deref() == Some(call.id.as_str()))
            .ok_or_else(|| invalid("settled retrieval has no committed response"))?;
        return Ok(Admission::Replay(Box::new(result.clone())));
    }
    if admission == ReservationAdmission::New {
        deps.session_seam.save(session).await?;
    }
    Ok(Admission::New(RetrievalCall {
        id: Some(id),
        upper_bytes,
    }))
}

pub(super) fn settle(
    session: &mut PersistedAgentSession,
    reservation: &RetrievalCall,
    message: &ChatMessage,
) -> Result<(), AgentError> {
    let Some(id) = &reservation.id else {
        return Ok(());
    };
    let actual = serialized_tool_response_bytes(
        message.tool_call_id.as_deref().unwrap_or_default(),
        &message.text,
        message.image_data_url.as_deref(),
    )
    .map_err(invalid)?;
    if actual > reservation.upper_bytes {
        return Err(invalid(
            "retrieval response exceeds its reserved framed byte limit",
        ));
    }
    session
        .assistant_data
        .as_mut()
        .ok_or_else(|| invalid("assistant retrieval accounting is unavailable"))?
        .retrieval_budget
        .settle(id, actual)
        .map_err(invalid)
}

pub(super) fn fits(reservation: &RetrievalCall, message: &ChatMessage) -> bool {
    if reservation.id.is_none() {
        return true;
    }
    serialized_tool_response_bytes(
        message.tool_call_id.as_deref().unwrap_or_default(),
        &message.text,
        message.image_data_url.as_deref(),
    )
    .is_ok_and(|bytes| bytes <= reservation.upper_bytes)
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}
