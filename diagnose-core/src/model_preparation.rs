//! Pure measurements of the actual outgoing protocol body and its source set.
use crate::seam::ModelRequest;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRequestPreparation {
    pub context_bytes: usize,
    pub total_bytes: usize,
    pub maximum_context_bytes: usize,
    pub system_prompt_bytes: usize,
    pub tool_definitions_bytes: usize,
    pub body_sha256: String,
    pub source_message_ids: Vec<String>,
}

impl ModelRequestPreparation {
    pub fn measure(
        body: &Value,
        request: &ModelRequest,
        maximum_context_bytes: usize,
    ) -> Result<Self, AgentError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|_| invalid("The model request could not be encoded"))?;
        Ok(Self {
            context_bytes: crate::prompt_cache::wire_context_bytes(body),
            total_bytes: bytes.len(),
            maximum_context_bytes,
            system_prompt_bytes: body.get("system").map_or_else(
                || {
                    body["messages"].as_array().map_or(0, |messages| {
                        messages
                            .iter()
                            .filter(|message| message["role"] == "system")
                            .map(|message| {
                                serde_json::to_vec(message).map_or(0, |bytes| bytes.len())
                            })
                            .sum()
                    })
                },
                |system| serde_json::to_vec(system).map_or(0, |bytes| bytes.len()),
            ),
            tool_definitions_bytes: body.get("tools").map_or(0, |tools| {
                serde_json::to_vec(tools).map_or(0, |bytes| bytes.len())
            }),
            body_sha256: format!("{:x}", Sha256::digest(&bytes)),
            source_message_ids: request
                .messages
                .iter()
                .map(|message| message.message_id.clone())
                .collect(),
        })
    }

    pub fn require_fit(&self) -> Result<(), AgentError> {
        if self.context_bytes > self.maximum_context_bytes {
            return Err(invalid(format!(
                "The prepared model request requires {} bytes, exceeding its {} byte context budget. Required pending calls, unread results and authorization recovery cannot be discarded; no model request was sent.",
                self.context_bytes, self.maximum_context_bytes
            )));
        }
        Ok(())
    }

    pub fn verify_send(
        body: &Value,
        request: &ModelRequest,
        maximum: usize,
    ) -> Result<(), AgentError> {
        let actual = Self::measure(body, request, maximum)?;
        actual.verify_request(request)
    }

    pub fn verify_request(&self, request: &ModelRequest) -> Result<(), AgentError> {
        self.require_fit()?;
        if request
            .preparation
            .as_deref()
            .is_some_and(|expected| expected != self)
        {
            let mut error = invalid(
                "The prepared model request changed before dispatch. Its sources, profile or protocol projection must be prepared again; no model request was sent.",
            );
            error.error_code = Some(desk_utils::error::DeskErrorCode::PRECONDITION_FAILED.code());
            return Err(error);
        }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: Some(desk_utils::error::DeskErrorCode::AI_CONTEXT_ITEM_TOO_LARGE.code()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::{ChatMessage, ChatRole},
        prompt::ResponseFormatSpec,
    };

    #[test]
    fn over_budget_is_measured_before_the_fit_gate_and_never_silently_sent() {
        let request = ModelRequest::text_only(
            vec![ChatMessage::text("source", ChatRole::User, "中\"文")],
            ResponseFormatSpec::None,
        );
        let body = serde_json::json!({"messages":[{"role":"user","content":"中\"文"}],"tools":[{"function":{"name":"f","parameters":{}}}]});
        let actual = serde_json::to_vec(&body).unwrap().len();
        let prepared = ModelRequestPreparation::measure(&body, &request, actual - 1).unwrap();
        assert_eq!(prepared.context_bytes, actual);
        assert_eq!(prepared.source_message_ids, ["source"]);
        assert!(prepared.require_fit().is_err());
        assert!(ModelRequestPreparation::verify_send(&body, &request, actual - 1).is_err());
        assert!(
            ModelRequestPreparation::measure(&body, &request, actual)
                .unwrap()
                .require_fit()
                .is_ok()
        );
    }

    #[test]
    fn prepared_body_and_actual_sources_must_match_even_when_both_fit() {
        let mut request = ModelRequest::text_only(
            vec![ChatMessage::text("source", ChatRole::User, "hello")],
            ResponseFormatSpec::None,
        );
        let body =
            serde_json::json!({"messages":[{"role":"user","content":"hello"}],"max_tokens":65536});
        request.preparation = Some(Box::new(
            ModelRequestPreparation::measure(&body, &request, 4096).unwrap(),
        ));
        ModelRequestPreparation::verify_send(&body, &request, 4096).unwrap();
        let mut changed = body.clone();
        changed["max_tokens"] = 1024.into();
        assert!(ModelRequestPreparation::verify_send(&changed, &request, 4096).is_err());
        request.messages.clear();
        assert!(ModelRequestPreparation::verify_send(&body, &request, 4096).is_err());
    }
}
