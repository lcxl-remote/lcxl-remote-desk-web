//! Immutable publication artifacts. They carry provenance, never current authority.

use crate::{
    external_tools::IssueAccess,
    model_egress::{NativeModelOutputSources, authorize_issue_delivery},
    native_ids::{self, Kind},
};
use desk_agent_protocol::{AgentError, data_lineage::DataEnvelope};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PREPARE_TOOL_NAME: &str = "prepare_issue_reply";

mod model_call;
pub use model_call::ReplyModelCall;
mod tools;
pub use tools::{
    ReplyGeneration, ReplyGenerationInput, ReplyPublication, ReplyToolReceipt, resolve, tool,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyOrigin {
    pub actor_id: String,
    pub root_conversation_id: String,
    pub request_id: String,
    pub execution_conversation_id: String,
    pub tool_call_id: String,
    pub input_revision: u64,
    pub control_revision: u64,
    pub authorization_id: String,
    pub authorization_revision: u64,
}

impl ReplyOrigin {
    pub(crate) fn validate(&self) -> Result<(), AgentError> {
        if [
            &self.actor_id,
            &self.root_conversation_id,
            &self.request_id,
            &self.execution_conversation_id,
            &self.authorization_id,
            &self.tool_call_id,
        ]
        .iter()
        .any(|id| id.is_empty() || id.len() > 512)
            || self.input_revision == 0
            || self.authorization_revision == 0
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenIssueReply {
    pub schema_version: u16,
    pub artifact_id: String,
    pub origin: ReplyOrigin,
    pub access: IssueAccess,
    pub body: String,
    pub output: DataEnvelope,
    pub native_sources: NativeModelOutputSources,
    pub comment_tool_signature: String,
    pub marker: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueReplyReceipt {
    pub comment_id: u64,
    pub account_id: u64,
    pub body: String,
}

/// Enough to recognize an already sent comment after local result data expires.
/// It contains no comment, prompt, model proof or device evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueReplyReceiptKey {
    pub access: IssueAccess,
    pub marker: String,
    pub wire_sha256: String,
}

impl IssueReplyReceiptKey {
    pub fn validate(&self) -> Result<(), AgentError> {
        self.access.validate()?;
        if self.marker.len() != 32
            || !self.marker.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.wire_sha256.len() != 64
            || !self
                .wire_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub fn matches_receipt(&self, account_id: u64, body: &str) -> bool {
        account_id == self.access.target.account_id
            && format!("{:x}", Sha256::digest(body.as_bytes())) == self.wire_sha256
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyTransportError {
    Unavailable,
    InvalidBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyReconciliationPage {
    pub receipt: Option<IssueReplyReceipt>,
    pub next_page: Option<std::num::NonZeroU32>,
}

/// Preparation is read-only. Sending consumes an immutable prepared request,
/// after the host has committed the original action's dispatch generation.
#[async_trait::async_trait(?Send)]
pub trait ReplyTransport {
    type Prepared;
    async fn prepare(
        &self,
        artifact: &FrozenIssueReply,
    ) -> Result<Self::Prepared, ReplyTransportError>;
    async fn send(
        &self,
        prepared: Self::Prepared,
    ) -> Result<IssueReplyReceipt, ReplyTransportError>;
    async fn reconcile(
        &self,
        key: &IssueReplyReceiptKey,
        page: std::num::NonZeroU32,
    ) -> Result<ReplyReconciliationPage, ReplyTransportError>;
}

impl FrozenIssueReply {
    /// The host has loaded the actual successful native receipt and checked the
    /// original request before handing its complete proof to this constructor.
    pub fn freeze(
        origin: ReplyOrigin,
        access: IssueAccess,
        body: String,
        output: DataEnvelope,
        native_sources: NativeModelOutputSources,
        comment_tool_signature: String,
        random_marker: &[u8; 16],
    ) -> Result<Self, AgentError> {
        let marker = random_marker
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let artifact_id = artifact_id(
            &origin.actor_id,
            &origin.execution_conversation_id,
            origin.input_revision,
            &origin.tool_call_id,
        );
        let reply = Self {
            schema_version: 1,
            artifact_id,
            origin,
            access,
            body,
            output,
            native_sources,
            comment_tool_signature,
            marker,
        };
        reply.validate()?;
        Ok(reply)
    }

    pub fn validate(&self) -> Result<(), AgentError> {
        self.origin.validate()?;
        self.access.validate()?;
        self.native_sources
            .validate(&self.output)
            .map_err(|_| invalid())?;
        if self.schema_version != 1
            || self.artifact_id
                != artifact_id(
                    &self.origin.actor_id,
                    &self.origin.execution_conversation_id,
                    self.origin.input_revision,
                    &self.origin.tool_call_id,
                )
            || self.native_sources.evidence.context_conversation_id
                != self.origin.execution_conversation_id
            || self.native_sources.evidence.model_export_authorization_id
                != publication_export_id(&self.origin, &self.access)
            || self.body.trim().is_empty()
            || self.wire_body().len() > 64 * 1024
            || self.output.content.size_bytes() != self.body.len() as u64
            || self.comment_tool_signature.len() != 64
            || !self
                .comment_tool_signature
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.marker.len() != 32
            || !self.marker.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub fn authorize_delivery(&self, now_ms: u64) -> Result<(), AgentError> {
        self.validate()?;
        authorize_issue_delivery(
            &self
                .access
                .target
                .destination(self.access.connection_ref.clone()),
            &self.native_sources,
            &self.output,
            &self.body,
            now_ms,
        )
        .map_err(|error| error.agent_error())
    }

    pub fn content_expires_at_unix_ms(&self) -> Option<u64> {
        std::iter::once(&self.output)
            .chain(self.native_sources.input_snapshot.envelopes.iter())
            .flat_map(|envelope| {
                let observation = match &envelope.content {
                    desk_agent_protocol::data_lineage::ContentRef::EphemeralObservation {
                        expires_at_unix_ms,
                        ..
                    } => Some(*expires_at_unix_ms),
                    _ => None,
                };
                [envelope.retention.expires_at_unix_ms, observation]
                    .into_iter()
                    .flatten()
            })
            .min()
    }

    pub fn wire_body(&self) -> String {
        format!("{}\n\n<!-- assistant-reply:{} -->", self.body, self.marker)
    }

    pub fn receipt_key(&self) -> IssueReplyReceiptKey {
        IssueReplyReceiptKey {
            access: self.access.clone(),
            marker: self.marker.clone(),
            wire_sha256: format!("{:x}", Sha256::digest(self.wire_body().as_bytes())),
        }
    }

    pub fn matches_receipt(&self, account_id: u64, body: &str) -> bool {
        account_id == self.access.target.account_id && body == self.wire_body()
    }
}

fn invalid() -> AgentError {
    crate::subagent::invalid("Frozen issue publication is invalid")
}

/// The actual physical model receipt must bind this original call and target.
pub fn publication_export_id(origin: &ReplyOrigin, access: &IssueAccess) -> String {
    native_ids::stable(Kind::Lineage, &("issue-publication", origin, access))
}

/// Control and policy changes fence dispatch; they cannot turn a replay of the
/// original tool call into a second external action with a new artifact ID.
pub fn artifact_id(actor: &str, conversation: &str, input_revision: u64, call: &str) -> String {
    native_ids::stable(
        Kind::IssueReply,
        &("issue-tool-call", actor, conversation, input_revision, call),
    )
}
