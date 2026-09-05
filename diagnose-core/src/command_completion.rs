//! Bounded interpretation of the output of one owner-confirmed exact command.

use desk_agent_protocol::{AgentError, AgentErrorKind, data_lineage::DestinationIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{chat::ChatRole, seam::ModelRequest, session::PersistedAgentSession};

pub const COMPLETION_GRACE_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandCompletionContext {
    pub destination: DestinationIdentity,
    pub captured_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub context_sha256: String,
}

pub fn context_digest(session: &PersistedAgentSession) -> Result<String, AgentError> {
    let bytes = serde_json::to_vec(&(
        &session.context_attachments,
        session.policy_revision,
        &session.scope_snapshot,
    ))
    .map_err(|_| denied())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

impl CommandCompletionContext {
    pub fn capture(
        session: &PersistedAgentSession,
        destination: DestinationIdentity,
        now: u64,
        approved_runtime_ms: u32,
    ) -> Result<Self, AgentError> {
        if now == 0 || approved_runtime_ms == 0 {
            return Err(denied());
        }
        let mut expires = now
            .checked_add(u64::from(approved_runtime_ms))
            .and_then(|deadline| deadline.checked_add(COMPLETION_GRACE_MS))
            .ok_or_else(denied)?;
        if let Some(scope_expiry) = &session.scope_snapshot.expires_at {
            let expiry =
                chrono::DateTime::parse_from_rfc3339(scope_expiry).map_err(|_| denied())?;
            expires = expires.min(u64::try_from(expiry.timestamp_millis()).map_err(|_| denied())?);
        }
        let context = Self {
            destination,
            captured_at_unix_ms: now,
            expires_at_unix_ms: expires,
            context_sha256: context_digest(session)?,
        };
        context.validate()?;
        Ok(context)
    }

    pub fn validate(&self) -> Result<(), AgentError> {
        self.destination.validate().map_err(|_| denied())?;
        if !matches!(self.destination, DestinationIdentity::Model { .. })
            || self.captured_at_unix_ms == 0
            || self.expires_at_unix_ms <= self.captured_at_unix_ms
            || self.context_sha256.len() != 64
            || !self
                .context_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(denied());
        }
        Ok(())
    }

    pub fn check(
        &self,
        session: &PersistedAgentSession,
        destination: &DestinationIdentity,
        now: u64,
    ) -> Result<(), AgentError> {
        self.validate()?;
        if &self.destination != destination
            || now < self.captured_at_unix_ms
            || now >= self.expires_at_unix_ms
            || self.context_sha256 != context_digest(session)?
        {
            return Err(denied());
        }
        Ok(())
    }
}

/// The caller must first validate the immutable receipt and its pending trigger.
/// Old tool-call groups and observations are not inputs to result interpretation.
pub fn project_request(
    mut request: ModelRequest,
    session: &PersistedAgentSession,
    event_id: &str,
) -> Result<ModelRequest, AgentError> {
    if !request.tools.is_empty() {
        return Err(denied());
    }
    let result = session
        .conversation
        .iter()
        .find(|message| message.message_id == event_id)
        .filter(|message| {
            message.role == ChatRole::UntrustedOutput && message.data_envelope.is_some()
        })
        .ok_or_else(denied)?;
    let requirement = crate::permission_resume::latest_user_requirement(&session.conversation)
        .filter(|message| message.data_envelope.is_some())
        .ok_or_else(denied)?;
    request
        .messages
        .retain(|message| message.role == ChatRole::System);
    request.messages.push(requirement.clone());
    request.messages.push(result.clone());
    Ok(request)
}

fn denied() -> AgentError {
    AgentError { kind: AgentErrorKind::PermissionDenied,
        message: "The completed result is saved, but its original model export authorization is no longer available.".into(),
        retryable: false, safe_for_model: true, error_code: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chat::ChatMessage, prompt::ResponseFormatSpec};

    fn destination() -> DestinationIdentity {
        DestinationIdentity::Model {
            connection_id: "provider".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        }
    }
    fn session() -> PersistedAgentSession {
        PersistedAgentSession::new(
            "run",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ConfirmEachAction,
                expires_at: None,
                policy_name: None,
            },
            "now",
        )
    }

    #[test]
    fn approved_runtime_plus_grace_survives_five_minutes_but_is_bounded_and_pinned() {
        let original = session();
        let now = 1000;
        let frozen =
            CommandCompletionContext::capture(&original, destination(), now, 600_000).unwrap();
        assert_eq!(frozen.expires_at_unix_ms, 901_000);
        frozen
            .check(&original, &destination(), now + 320_000)
            .unwrap();
        frozen.check(&original, &destination(), 900_999).unwrap();
        assert!(frozen.check(&original, &destination(), 901_000).is_err());
        assert!(frozen.check(&original, &destination(), now - 1).is_err());
        let mut changed = destination();
        if let DestinationIdentity::Model { model_id, .. } = &mut changed {
            *model_id = "other".into();
        }
        assert!(frozen.check(&original, &changed, now + 320_000).is_err());
        let mut changed = original.clone();
        changed.policy_revision += 1;
        assert!(
            frozen
                .check(&changed, &destination(), now + 320_000)
                .is_err()
        );
        let encoded = serde_json::to_string(&frozen).unwrap();
        assert_eq!(
            serde_json::from_str::<CommandCompletionContext>(&encoded).unwrap(),
            frozen
        );
        assert!(CommandCompletionContext::capture(&original, destination(), now, 0).is_err());
        assert!(CommandCompletionContext::capture(&original, destination(), u64::MAX, 1).is_err());
        let mut scoped = original;
        scoped.scope_snapshot.expires_at = Some("1970-01-01T00:07:00Z".into());
        let limited =
            CommandCompletionContext::capture(&scoped, destination(), now, 600_000).unwrap();
        assert_eq!(limited.expires_at_unix_ms, 420_000);
        assert!(limited.check(&scoped, &destination(), 420_000).is_err());
    }

    #[test]
    fn projection_keeps_only_original_requirement_and_exact_result_without_renewing_labels() {
        let mut session = session();
        let user = crate::model_message_labels::model_bound_user_message(
            "user".into(),
            "Find large directories".into(),
            destination(),
        )
        .unwrap();
        let mut result = ChatMessage::text("event", ChatRole::UntrustedOutput, "du completed");
        result.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            user.data_envelope.as_ref(),
            "call",
            &result.text,
            "execute_confirmed_command",
        )
        .unwrap();
        result.tool_call_id = Some("call".into());
        session.conversation = vec![
            user.clone(),
            ChatMessage::text("old", ChatRole::Assistant, "expired old observation"),
            result.clone(),
        ];
        let request = ModelRequest::text_only(
            vec![ChatMessage::text(
                "system",
                ChatRole::System,
                "trusted prompt",
            )],
            ResponseFormatSpec::None,
        );
        let projected = project_request(request.clone(), &session, "event").unwrap();
        assert_eq!(projected.messages.len(), 3);
        assert_eq!(projected.messages[1], user);
        assert_eq!(projected.messages[2], result);
        assert!(projected.tools.is_empty());
        assert!(project_request(request.clone(), &session, "missing").is_err());
        session.conversation.last_mut().unwrap().data_envelope = None;
        assert!(project_request(request, &session, "event").is_err());
    }
}
