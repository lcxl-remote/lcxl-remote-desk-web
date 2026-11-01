//! Exact visible-message binding. This proves content identity only; the host
//! authenticates the accepted archive and all current source controls separately.

use super::ArchivedAssistantOutput;
use crate::{
    chat::{ChatMessage, ChatRole, ModelTurn, ToolCall},
    model_egress::model_turn_content_bytes,
};
use desk_agent_protocol::{
    ai_assistant::source::{SourceUsePolicy, validate_source_id},
    data_lineage::ContentRef,
};
use sha2::{Digest, Sha256};

/// Retention policy validated against a real message and its original native output.
pub struct VisibleAssistantMessage {
    policy: Option<SourceUsePolicy>,
}

impl ArchivedAssistantOutput {
    pub fn bind_visible_message(
        &self,
        message: &ChatMessage,
    ) -> Result<VisibleAssistantMessage, &'static str> {
        self.input_policy_graph
            .validate_native_closure(&self.sources.input_snapshot)?;
        validate_source_id(&message.message_id)?;
        if message.role != ChatRole::Assistant
            || message.data_envelope.as_ref() != Some(&self.output)
            || message.tool_call_id.is_some()
            || message.image_data_url.is_some()
            || message.background_task_id.is_some()
            || message.attachment_read.is_some()
            || message.raw_result.is_some()
            || message.pending_delivery_format.is_some()
        {
            return Err("visible assistant message is not its original native output");
        }
        // Tool references retain the original wire fields. Reasoning/replay and
        // display-only control summaries are not added to the archived body.
        let turn = ModelTurn {
            text: message.text.clone(),
            tool_calls: message
                .tool_calls
                .iter()
                .map(|call| ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments_json: call.arguments_json.clone(),
                })
                .collect(),
            ..Default::default()
        };
        let bytes = model_turn_content_bytes(&turn)
            .map_err(|_| "invalid original visible model payload")?;
        let ContentRef::ImmutableBlob {
            sha256, size_bytes, ..
        } = &self.output.content
        else {
            return Err("visible native output is not immutable");
        };
        let actual = format!("{:x}", Sha256::digest(&bytes));
        if actual != self.output.digest_sha256
            || sha256 != &actual
            || *size_bytes != bytes.len() as u64
        {
            return Err("visible assistant message changed its native text or tool calls");
        }
        let policy = self.retained_policy.clone();
        Ok(VisibleAssistantMessage { policy })
    }
}

impl VisibleAssistantMessage {
    pub fn history_policy(&self) -> Option<&SourceUsePolicy> {
        self.policy.as_ref()
    }
}
