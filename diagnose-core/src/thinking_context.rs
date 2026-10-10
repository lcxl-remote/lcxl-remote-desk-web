//! Content-free observations and explicit, source-bound thinking projection.
use crate::{
    chat::{ChatMessage, ChatRole, TokenUsage},
    replay::{ReplayCodec, ReplayDisposition, SourceContextKey},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderContextObservation {
    pub call_id: String,
    pub conversation_id: Option<String>,
    pub stale: bool,
    pub observed_at_unix_ms: i64,
    pub source_context_key: Option<SourceContextKey>,
    pub profile_revision: i64,
    pub input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cleared_thinking_turns: Option<u64>,
    pub cleared_input_tokens: Option<u64>,
    pub request_bytes: Option<u64>,
}

impl ProviderContextObservation {
    pub fn record_edits(&mut self, value: &Value) {
        let Some(edits) = value
            .pointer("/context_management/applied_edits")
            .and_then(Value::as_array)
        else {
            return;
        };
        let thinking: Vec<_> = edits
            .iter()
            .filter(|edit| edit["type"] == "clear_thinking_20251015")
            .collect();
        if thinking.is_empty() {
            return;
        }
        self.cleared_thinking_turns = thinking.iter().try_fold(0u64, |sum, edit| {
            sum.checked_add(edit["cleared_thinking_turns"].as_u64()?)
        });
        self.cleared_input_tokens = thinking.iter().try_fold(0u64, |sum, edit| {
            sum.checked_add(edit["cleared_input_tokens"].as_u64()?)
        });
    }

    pub fn with_usage(mut self, usage: TokenUsage, cache_write_applicable: bool) -> Self {
        self.input_tokens = normalized_input_tokens(usage, cache_write_applicable);
        self
    }
}

pub fn normalized_input_tokens(usage: TokenUsage, cache_write_applicable: bool) -> Option<u64> {
    let input = u64::try_from(usage.input_tokens?).ok()?;
    let read = u64::try_from(usage.cache_read_tokens?).ok()?;
    let write = if cache_write_applicable {
        u64::try_from(usage.cache_write_tokens?).ok()?
    } else {
        0
    };
    input.checked_add(read)?.checked_add(write)
}

pub fn project_replay(
    message: &mut ChatMessage,
    contract: crate::model_profile::ReasoningContract,
) {
    if contract == crate::model_profile::ReasoningContract::OpenaiChat
        && let Some(ReplayDisposition::Present { envelope }) = &message.replay_disposition
        && envelope.codec == ReplayCodec::OpenAiReasoningContent
    {
        message.replay_disposition = Some(ReplayDisposition::NotRequired {
            source_context_key: envelope.source_context_key.clone(),
        });
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingPrefixState {
    pub bindings: BTreeMap<String, String>,
    pub invalidated: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingPrefixObservation {
    pub request_prefix: String,
    pub invalidated: BTreeSet<String>,
}

fn digest(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}
fn without_cache(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| key.as_str() != "cache_control")
                .map(|(key, value)| (key.clone(), without_cache(value)))
                .collect(),
        ),
        Value::Array(array) => Value::Array(array.iter().map(without_cache).collect()),
        _ => value.clone(),
    }
}

/// Compare the actual Messages layout, preserving valid earlier blocks and
/// removing only invalid completed thinking. Protected chains fail explicitly.
pub fn project_anthropic_prefix(
    body: &mut Value,
    request: &crate::seam::ModelRequest,
    source: &SourceContextKey,
) -> Result<ThinkingPrefixObservation, crate::model_profile::ProfileError> {
    let state = request
        .thinking_prefix
        .as_ref()
        .cloned()
        .unwrap_or_default();
    let mut invalidated = state.invalidated.clone();
    let mut prefix = json!([
        source,
        without_cache(&body["system"]),
        without_cache(&body["tools"]),
        []
    ]);
    let ids: Vec<_> = request
        .messages
        .iter()
        .filter(|m| m.role != ChatRole::System)
        .collect();
    if let Some(messages) = body["messages"].as_array_mut() {
        for (index, message) in messages.iter_mut().enumerate() {
            let Some(original) = ids.get(index) else {
                continue;
            };
            let thinking = message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        matches!(
                            block["type"].as_str(),
                            Some("thinking" | "redacted_thinking")
                        )
                    })
                });
            let originally_thinking = matches!(&original.replay_disposition,
                Some(ReplayDisposition::Present { envelope }) if envelope.codec == ReplayCodec::AnthropicContentBlocks
                    && envelope.payload.as_array().is_some_and(|blocks| blocks.iter().any(|block| matches!(block["type"].as_str(),Some("thinking"|"redacted_thinking")))));
            if (thinking || originally_thinking)
                && (invalidated.contains(&original.message_id)
                    || state.bindings.get(&original.message_id) != Some(&digest(&prefix)))
            {
                if request
                    .protected_replay_message_ids
                    .contains(&original.message_id)
                {
                    return Err(crate::model_profile::ProfileError::InvalidRequestOption(
                        "Anthropic thinking prefix changed inside a protected tool chain. Complete the chain with unchanged system/tools/runtime context before compressing or changing the request layout.".into()));
                }
                invalidated.insert(original.message_id.clone());
                if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                    blocks.retain(|block| {
                        !matches!(
                            block["type"].as_str(),
                            Some("thinking" | "redacted_thinking")
                        )
                    });
                }
            }
            prefix[3]
                .as_array_mut()
                .unwrap()
                .push(without_cache(message));
        }
    }
    Ok(ThinkingPrefixObservation {
        request_prefix: digest(&prefix),
        invalidated,
    })
}

/// Translate raw-history protections to assistant messages, including tool groups.
pub fn protected_replay_ids(session: &crate::session::PersistedAgentSession) -> BTreeSet<String> {
    let protection = session.context_protection_set();
    let mut ids = protection.protected_message_ids.clone();
    let mut calls = protection.protected_tool_call_ids;
    for message in &session.conversation {
        if ids.contains(&message.message_id)
            && let Some(call) = &message.tool_call_id
        {
            calls.insert(call.clone());
        }
    }
    for message in &session.conversation {
        if message.role == ChatRole::Assistant
            && (message
                .turn_id
                .as_ref()
                .is_some_and(|id| Some(id) == protection.current_turn_id.as_ref())
                || message
                    .tool_calls
                    .iter()
                    .any(|call| calls.contains(&call.id)))
        {
            ids.insert(message.message_id.clone());
        }
    }
    ids
}

/// Known invalid completed blocks are omitted only from the prepared copy.
pub fn project_known_invalid(message: &mut ChatMessage, state: &ThinkingPrefixState) {
    if state.invalidated.contains(&message.message_id)
        && let Some(ReplayDisposition::Present { envelope }) = &mut message.replay_disposition
        && envelope.codec == ReplayCodec::AnthropicContentBlocks
        && let Some(blocks) = envelope.payload.as_array_mut()
    {
        blocks.retain(|block| {
            !matches!(
                block["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_includes_cache_once_and_missing_counters_are_unknown() {
        let usage = TokenUsage {
            input_tokens: Some(10),
            cache_read_tokens: Some(90),
            cache_write_tokens: Some(20),
            output_tokens: Some(5),
        };
        assert_eq!(normalized_input_tokens(usage, false), Some(100));
        assert_eq!(normalized_input_tokens(usage, true), Some(120));
        assert_eq!(
            normalized_input_tokens(
                TokenUsage {
                    cache_read_tokens: None,
                    ..usage
                },
                true
            ),
            None
        );
        assert_eq!(
            normalized_input_tokens(
                TokenUsage {
                    input_tokens: Some(-1),
                    ..usage
                },
                false
            ),
            None
        );
    }
    #[test]
    fn clearing_is_an_observation_not_a_second_usage_subtraction() {
        let mut observation = ProviderContextObservation::default();
        observation.record_edits(&json!({"context_management":{"applied_edits":[{"type":"clear_thinking_20251015","cleared_thinking_turns":2,"cleared_input_tokens":500}]}}));
        assert_eq!(observation.cleared_input_tokens, Some(500));
        let observation = observation.with_usage(
            TokenUsage {
                input_tokens: Some(10),
                cache_read_tokens: Some(90),
                ..Default::default()
            },
            false,
        );
        assert_eq!(observation.input_tokens, Some(100));
    }

    fn source() -> SourceContextKey {
        SourceContextKey::derive(
            crate::model_profile::WireProtocol::AnthropicMessages,
            "connection",
            "model",
            "test",
        )
    }
    fn bound_message(id: &str, source: &SourceContextKey) -> ChatMessage {
        let mut message = ChatMessage::text(id, ChatRole::Assistant, "done");
        message.replay_disposition = Some(ReplayDisposition::Present {
            envelope: crate::replay::ProviderReplayEnvelope::new(
                ReplayCodec::AnthropicContentBlocks,
                source.clone(),
                json!([
                {"type":"thinking","thinking":"opaque reasoning","signature":"signed"},
                {"type":"text","text":"done"}]),
            ),
        });
        message
    }
    fn body(messages: Vec<Value>) -> Value {
        json!({"system":"stable","tools":[],"messages":messages})
    }
    #[test]
    fn prefix_preserves_early_blocks_and_drops_only_invalid_completed_blocks() {
        let source = source();
        let user = ChatMessage::text("u", ChatRole::User, "question");
        let first = crate::seam::ModelRequest::text_only(
            vec![user.clone()],
            crate::prompt::ResponseFormatSpec::None,
        );
        let user_wire = json!({"role":"user","content":"question"});
        let binding =
            project_anthropic_prefix(&mut body(vec![user_wire.clone()]), &first, &source).unwrap();
        let assistant = bound_message("a", &source);
        let replay = match &assistant.replay_disposition {
            Some(ReplayDisposition::Present { envelope }) => envelope.payload.clone(),
            _ => unreachable!(),
        };
        let mut request = crate::seam::ModelRequest::text_only(
            vec![user, assistant.clone()],
            crate::prompt::ResponseFormatSpec::None,
        );
        request.thinking_prefix = Some(ThinkingPrefixState {
            bindings: BTreeMap::from([("a".into(), binding.request_prefix)]),
            invalidated: BTreeSet::new(),
        });
        let wire = body(vec![
            user_wire,
            json!({"role":"assistant","content":replay}),
        ]);
        let mut valid = wire.clone();
        assert!(
            project_anthropic_prefix(&mut valid, &request, &source)
                .unwrap()
                .invalidated
                .is_empty()
        );
        assert_eq!(valid, wire);
        let mut changed = wire.clone();
        changed["system"] = json!("changed");
        let invalid = project_anthropic_prefix(&mut changed, &request, &source).unwrap();
        assert!(invalid.invalidated.contains("a"));
        assert_eq!(
            changed["messages"][1]["content"],
            json!([{"type":"text","text":"done"}])
        );
        // A second assembly pass retains the invalidation even after dropping blocks.
        assert!(
            project_anthropic_prefix(&mut changed, &request, &source)
                .unwrap()
                .invalidated
                .contains("a")
        );
        request.protected_replay_message_ids.insert("a".into());
        let mut changed = wire;
        changed["system"] = json!("changed");
        assert!(project_anthropic_prefix(&mut changed, &request, &source).is_err());
        assert_eq!(
            request.messages[1], assistant,
            "opaque history must stay intact"
        );
    }
    #[test]
    fn replay_contract_and_cost_are_based_on_the_prepared_copy() {
        let source = source();
        let original = bound_message("a", &source);
        let mut prepared = original.clone();
        let mut state = ThinkingPrefixState::default();
        state.invalidated.insert("a".into());
        project_known_invalid(&mut prepared, &state);
        assert!(
            crate::trim::model_context_cost(&prepared) < crate::trim::model_context_cost(&original)
        );
        let expected = json!({"role":"assistant","content":[{"type":"text","text":"done"}]})
            .to_string()
            .len();
        assert_eq!(crate::trim::model_context_cost(&prepared), expected);
        let mut openai = original.clone();
        openai.replay_disposition = Some(ReplayDisposition::Present {
            envelope: crate::replay::ProviderReplayEnvelope::new(
                ReplayCodec::OpenAiReasoningContent,
                source.clone(),
                json!("reasoning"),
            ),
        });
        let mut standard = openai.clone();
        project_replay(
            &mut standard,
            crate::model_profile::ReasoningContract::OpenaiChat,
        );
        assert!(matches!(
            standard.replay_disposition,
            Some(ReplayDisposition::NotRequired { .. })
        ));
        project_replay(
            &mut openai,
            crate::model_profile::ReasoningContract::DeepseekChat,
        );
        assert!(matches!(
            openai.replay_disposition,
            Some(ReplayDisposition::Present { .. })
        ));
    }
}
