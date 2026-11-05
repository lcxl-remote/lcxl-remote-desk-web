//! Host-only publication purpose, bound to one original action and exact issue.

use super::{ReplyOrigin, publication_export_id};
use crate::{
    external_tools::IssueAccess,
    model_egress::{ModelEgressError, ModelEgressPolicy},
    seam::ModelRequest,
};

/// Provenance binding, not permission to publish. Native acceptance and delivery
/// independently use the original input and current host authority.
#[derive(Debug, Clone)]
pub struct ReplyModelCall {
    origin: ReplyOrigin,
    access: IssueAccess,
    native_fence: Option<ReplyModelFence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyModelFence {
    pub version: i64,
    pub lease_token: u64,
    pub turn_id: String,
}

impl ReplyModelCall {
    pub fn origin(&self) -> &ReplyOrigin {
        &self.origin
    }
    pub fn access(&self) -> &IssueAccess {
        &self.access
    }
    pub fn bind_native_input(
        &mut self,
        held: &crate::session::PersistedAgentSession,
    ) -> Result<(), ModelEgressError> {
        if held.actor_id != self.origin.actor_id
            || held.conversation_id != self.origin.execution_conversation_id
            || held.input_revision != self.origin.input_revision
            || held.control_revision != self.origin.control_revision
        {
            return Err(invalid());
        }
        self.native_fence = Some(ReplyModelFence {
            version: held.version,
            lease_token: held.lease_token,
            turn_id: held.current_turn_id.clone().ok_or_else(invalid)?,
        });
        Ok(())
    }
    pub fn native_input_matches(&self, held: &crate::session::PersistedAgentSession) -> bool {
        self.native_fence.as_ref().is_some_and(|fence| {
            fence.version == held.version
                && fence.lease_token == held.lease_token
                && Some(fence.turn_id.as_str()) == held.current_turn_id.as_deref()
        }) && self.origin.actor_id == held.actor_id
            && self.origin.execution_conversation_id == held.conversation_id
            && self.origin.input_revision == held.input_revision
            && self.origin.control_revision == held.control_revision
    }
}

impl ModelEgressPolicy {
    /// Build a fresh, tool-free request. General conversation, memory and Worker
    /// history are never copied here; every supplied material must allow this issue.
    pub fn prepare_issue_reply_call(
        &self,
        origin: ReplyOrigin,
        access: IssueAccess,
        materials: Vec<crate::chat::ChatMessage>,
    ) -> Result<ModelRequest, ModelEgressError> {
        origin.validate().map_err(|_| invalid())?;
        access.validate().map_err(|_| invalid())?;
        let mut policy = self.clone();
        policy.export_authorization_id = publication_export_id(&origin, &access);
        let mut request = policy
            .authorize_issue_reply_request(
                &access.target.destination(access.connection_ref.clone()),
                materials,
            )?
            .request;
        request.context_conversation_id = Some(origin.execution_conversation_id.clone());
        request.issue_reply = Some(Box::new(ReplyModelCall {
            origin,
            access,
            native_fence: None,
        }));
        Ok(request)
    }

    /// Resolve the call's purpose at the audited host boundary. This changes
    /// neither the pinned provider nor the host's ordinary export policy.
    pub fn for_request(mut self, request: &ModelRequest) -> Result<Self, ModelEgressError> {
        let Some(call) = &request.issue_reply else {
            return Ok(self);
        };
        if request.context_conversation_id.as_deref()
            != Some(&call.origin.execution_conversation_id)
            || request.use_case != crate::model_profile::ModelUseCase::Agent
            || !request.tools.is_empty()
            || request.assistant_data.is_some()
            || request.compression_identity.is_some()
            || request.previous_cache_projection.is_some()
            || request.thinking_prefix.is_some()
            || request.ui_references.is_some()
            || request.pinned_source_context_key.is_some()
            || !request.protected_replay_message_ids.is_empty()
        {
            return Err(invalid());
        }
        self.export_authorization_id = publication_export_id(&call.origin, &call.access);
        crate::model_egress::validate_issue_reply_messages(
            &request.messages,
            &call
                .access
                .target
                .destination(call.access.connection_ref.clone()),
            self.now_unix_ms,
            self.byte_cap,
        )?;
        Ok(self)
    }
}

fn invalid() -> ModelEgressError {
    ModelEgressError::InvalidPolicy("Invalid isolated issue publication call".into())
}
