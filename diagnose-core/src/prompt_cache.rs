//! Explicit cache hints on final protocol blocks; never edits history or replay.
use crate::{chat::ChatRole, seam::ModelRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    #[default]
    ProviderDefault,
    AnthropicExplicit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheOptions {
    #[serde(default)]
    pub mode: CacheMode,
    #[serde(default)]
    pub cache_history: bool,
}

/// String and block encodings carry identical text. Never annotate thinking.
fn mark_last(content: &mut Value) -> bool {
    if let Some(text) = content.as_str() {
        if text.is_empty() {
            return false;
        }
        *content = json!([{"type":"text", "text":text}]);
    }
    let Some(blocks) = content.as_array_mut() else {
        return false;
    };
    for block in blocks.iter_mut().rev() {
        if matches!(
            block["type"].as_str(),
            Some("text" | "image" | "tool_use" | "tool_result")
        ) {
            if block["type"] == "text" && block["text"].as_str().is_none_or(str::is_empty) {
                continue;
            }
            block["cache_control"] = json!({"type":"ephemeral"});
            return true;
        }
    }
    false
}

pub fn apply_anthropic(body: &mut Value, request: &ModelRequest, options: &Value) {
    let Ok(options) = serde_json::from_value::<CacheOptions>(
        options.get("prompt_cache").cloned().unwrap_or(json!({})),
    ) else {
        return;
    };
    if options.mode != CacheMode::AnthropicExplicit {
        return;
    }
    if let Some(system) = body.get_mut("system") {
        mark_last(system);
    }
    if !options.cache_history {
        return;
    }
    // Each neutral non-system message has one wire message at this point.
    // Mark before runtime state, even when the API subsequently merges user runs.
    let Some(boundary) = request
        .messages
        .iter()
        .filter(|m| m.role != ChatRole::System)
        .position(crate::runtime_context::is_runtime)
    else {
        return;
    };
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages[..boundary].iter_mut().rev() {
            if let Some(content) = message.get_mut("content") {
                if mark_last(content) {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatMessage;
    use crate::prompt::ResponseFormatSpec;
    #[test]
    fn hints_stop_before_runtime_and_preserve_signed_thinking() {
        let request = ModelRequest::text_only(
            vec![
                ChatMessage::text("u", ChatRole::User, "question"),
                ChatMessage::text("a", ChatRole::Assistant, "answer"),
                ChatMessage::system_event(crate::runtime_context::MESSAGE_ID, "state"),
            ],
            ResponseFormatSpec::None,
        );
        let original = json!({"system":"rules","messages":[{"role":"user","content":"question"},{"role":"assistant","content":[{"type":"thinking","thinking":"private","signature":"signed"},{"type":"text","text":"answer"}]},{"role":"user","content":"state"}]});
        let mut body = original.clone();
        apply_anthropic(&mut body, &request, &json!({}));
        assert_eq!(body, original);
        apply_anthropic(
            &mut body,
            &request,
            &json!({"prompt_cache":{"mode":"anthropic_explicit","cache_history":true}}),
        );
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(
            body["messages"][1]["content"][1]
                .get("cache_control")
                .is_some()
        );
        assert_eq!(
            body["messages"][1]["content"][0],
            original["messages"][1]["content"][0]
        );
        assert_eq!(body["messages"][2], original["messages"][2]);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeReason {
    Initial,
    ConnectionOrModel,
    RequestOptions,
    ToolDefinitions,
    StaticInstructions,
    HistoryProjection,
    UnknownProjectionChange,
    RuntimeOrAppend,
}

/// Content-free, replaceable metadata; the configured gateway secret is the MAC
/// key, so restarts and Manager instances agree without storing another secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireObservation {
    pub binding: String,
    pub source_binding: String,
    pub options_binding: String,
    pub change_reason: ChangeReason,
    pub blocks: Vec<String>,
    pub anchor: Option<usize>,
    pub common_prefix_blocks: usize,
    pub response_digest: Option<String>,
}

fn mac(key: &[u8], bytes: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let mut value = Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    value.update(b"assistant-cache-layout-v1\0");
    value.update(bytes);
    format!("{:x}", value.finalize().into_bytes())
}

fn plain_block(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("cache_control");
    }
    value
}

/// Compare the final wire projection, including model/options, schema, roles,
/// replay and inline images. Never use message IDs as content identity.
pub fn observe(
    body: &mut Value,
    previous: Option<&WireObservation>,
    key: &[u8],
    source: &crate::replay::SourceContextKey,
) -> Option<WireObservation> {
    if key.is_empty() {
        return None;
    }
    let mut header = body.clone();
    let object = header.as_object_mut()?;
    object.remove("messages");
    object.remove("system");
    object.remove("tools");
    let source_binding = mac(key, &serde_json::to_vec(source).ok()?);
    let options_binding = mac(key, &serde_json::to_vec(&header).ok()?);
    let binding = mac(key, &serde_json::to_vec(&(source, header)).ok()?);
    let mut blocks = Vec::new();
    let mut positions = Vec::new();
    let mut chain = binding.clone();
    let mut logical_position = 0usize;
    let mut anchor = None;
    let mut append = |value: Value, position: Option<(usize, usize, usize)>| {
        chain = mac(
            key,
            &serde_json::to_vec(&(&chain, value)).expect("JSON is serializable"),
        );
        blocks.push(chain.clone());
        positions.push(position);
        blocks.len() - 1
    };
    append(body.get("tools").cloned().unwrap_or(Value::Null), None);
    append(
        body.get("system")
            .map(|value| {
                if let Some(items) = value.as_array() {
                    Value::Array(items.iter().map(plain_block).collect())
                } else {
                    value.clone()
                }
            })
            .unwrap_or(Value::Null),
        None,
    );
    for (message_index, message) in body["messages"].as_array()?.iter().enumerate() {
        let role = &message["role"];
        if let Some(items) = message["content"].as_array() {
            let mut previous_kind = "";
            for (block_index, block) in items.iter().enumerate() {
                let kind = block["type"].as_str().unwrap_or("");
                if !matches!(kind, "tool_use" | "tool_result") || kind != previous_kind {
                    logical_position += 1;
                }
                previous_kind = kind;
                let index = append(
                    json!([role, plain_block(block)]),
                    Some((message_index, block_index, logical_position)),
                );
                if block.get("cache_control").is_some() {
                    anchor = Some(index);
                }
            }
        } else {
            logical_position += 1;
            append(message.clone(), None);
        }
    }
    // Metadata is expendable; unexpectedly large inputs do not grow durable state.
    if blocks.len() > 8192 {
        return None;
    }
    let common_prefix_blocks = previous
        .filter(|old| old.binding == binding)
        .map_or(0, |old| {
            blocks
                .iter()
                .zip(&old.blocks)
                .take_while(|(left, right)| left == right)
                .count()
        });
    if let Some((old_anchor, latest_anchor)) = previous.and_then(|old| old.anchor).zip(anchor) {
        if old_anchor < common_prefix_blocks && old_anchor < latest_anchor {
            if let (Some((mi, bi, old_position)), Some((_, _, new_position))) = (
                positions.get(old_anchor).copied().flatten(),
                positions.get(latest_anchor).copied().flatten(),
            ) {
                if new_position.saturating_sub(old_position) > 20 {
                    body["messages"][mi]["content"][bi]["cache_control"] =
                        json!({"type":"ephemeral"});
                }
            }
        }
    }
    let change_reason = match previous {
        None => ChangeReason::Initial,
        Some(old) if old.source_binding != source_binding => ChangeReason::ConnectionOrModel,
        Some(old) if old.options_binding != options_binding => ChangeReason::RequestOptions,
        Some(_) if common_prefix_blocks == 0 => ChangeReason::ToolDefinitions,
        Some(_) if common_prefix_blocks == 1 => ChangeReason::StaticInstructions,
        Some(old)
            if old
                .anchor
                .is_some_and(|anchor| common_prefix_blocks <= anchor) =>
        {
            ChangeReason::HistoryProjection
        }
        Some(old) if old.anchor.is_none() && common_prefix_blocks < old.blocks.len() => {
            ChangeReason::UnknownProjectionChange
        }
        Some(_) => ChangeReason::RuntimeOrAppend,
    };
    Some(WireObservation {
        binding,
        source_binding,
        options_binding,
        change_reason,
        blocks,
        anchor,
        common_prefix_blocks,
        response_digest: None,
    })
}

pub fn record_response(
    observation: &mut Option<WireObservation>,
    turn: &crate::chat::ModelTurn,
    key: &[u8],
) {
    if let Some(observation) = observation {
        observation.response_digest = serde_json::to_vec(turn).ok().map(|bytes| mac(key, &bytes));
    }
}

/// Validate the encoded request after role/schema/cache projection. Cached input
/// occupies the same context budget as uncached input.
pub fn validate_wire_budget(
    body: &Value,
    maximum: usize,
) -> Result<(), crate::model_profile::ProfileError> {
    let actual = serde_json::to_vec(body).map_or(usize::MAX, |bytes| bytes.len());
    if actual > maximum {
        return Err(crate::model_profile::ProfileError::InvalidRequestOption(
            format!("encoded model request exceeds context byte budget ({actual} > {maximum})"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    fn source() -> crate::replay::SourceContextKey {
        crate::replay::SourceContextKey::derive(
            crate::model_profile::WireProtocol::AnthropicMessages,
            "gateway",
            "model-id",
            "model",
        )
    }
    fn body(count: usize) -> Value {
        let mut messages = (0..count)
            .map(|i| json!({"role":"user","content":[{"type":"text","text":format!("fact {i}")}]}))
            .collect::<Vec<_>>();
        messages.last_mut().unwrap()["content"][0]["cache_control"] = json!({"type":"ephemeral"});
        json!({"model":"test","system":[{"type":"text","text":"rules","cache_control":{"type":"ephemeral"}}],"messages":messages})
    }
    #[test]
    fn changed_prefix_and_rotated_key_never_restore_an_old_anchor() {
        let first = observe(&mut body(2), None, b"secret", &source()).unwrap();
        let mut unchanged = body(25);
        let next = observe(&mut unchanged, Some(&first), b"secret", &source()).unwrap();
        assert_eq!(next.common_prefix_blocks, first.blocks.len());
        assert!(
            unchanged["messages"][1]["content"][0]
                .get("cache_control")
                .is_some()
        );
        let mut changed = body(25);
        changed["messages"][0]["content"][0]["text"] = "attachment replaced".into();
        let next = observe(&mut changed, Some(&first), b"secret", &source()).unwrap();
        assert_eq!(next.common_prefix_blocks, 2);
        assert_eq!(next.change_reason, ChangeReason::HistoryProjection);
        let mut unanchored = first.clone();
        unanchored.anchor = None;
        let unknown = observe(&mut changed, Some(&unanchored), b"secret", &source()).unwrap();
        assert_eq!(unknown.change_reason, ChangeReason::UnknownProjectionChange);
        assert!(
            changed["messages"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
        let next = observe(&mut body(25), Some(&first), b"new secret", &source()).unwrap();
        assert_eq!(next.common_prefix_blocks, 0);
        assert!(!serde_json::to_string(&first).unwrap().contains("fact"));
    }
    #[test]
    fn configuration_reset_preserves_thinking_and_metadata_corruption_is_disposable() {
        let mut options = json!({"thinking":{"type":"adaptive"},"prompt_cache":{"mode":"anthropic_explicit","cache_history":true}});
        assert!(reset_history(&mut options));
        assert_eq!(
            without_cache(&options),
            json!({"thinking":{"type":"adaptive"}})
        );
        assert_eq!(options["prompt_cache"]["cache_history"], false);
        assert!(!reset_history(&mut options));
        #[derive(Deserialize)]
        struct Stored {
            #[serde(deserialize_with = "deserialize_observation")]
            cache: Option<WireObservation>,
        }
        assert!(
            serde_json::from_value::<Stored>(json!({"cache":{"bad":"metadata"}}))
                .unwrap()
                .cache
                .is_none()
        );
    }

    #[test]
    fn consecutive_parallel_tool_results_use_one_lookback_position() {
        let first = observe(&mut body(2), None, b"secret", &source()).unwrap();
        let mut next = body(3);
        next["messages"][2]["content"] = Value::Array((0..30).map(|i| json!({"type":"tool_result","tool_use_id":format!("id{i}"),"content":"result"})).collect());
        next["messages"][2]["content"][29]["cache_control"] = json!({"type":"ephemeral"});
        observe(&mut next, Some(&first), b"secret", &source()).unwrap();
        assert!(
            next["messages"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }
}

pub fn without_cache(options: &Value) -> Value {
    let mut options = options.clone();
    if let Some(object) = options.as_object_mut() {
        object.remove("prompt_cache");
    }
    options
}

/// Changing the target/configuration requires a new explicit history opt-in.
pub fn reset_history(options: &mut Value) -> bool {
    if let Some(history) = options.pointer_mut("/prompt_cache/cache_history") {
        let changed = *history != Value::Bool(false);
        *history = Value::Bool(false);
        changed
    } else {
        false
    }
}

pub fn deserialize_observation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<WireObservation>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<WireObservation>(value)
        .ok()
        .filter(|value| {
            value.blocks.len() <= 8192
                && value.binding.len() == 64
                && value.blocks.iter().all(|digest| {
                    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                && value.anchor.is_none_or(|index| index < value.blocks.len())
        }))
}

/// A different connection is an unverified gateway, including for static hints.
pub fn reset_connection(options: &mut Value) -> bool {
    options
        .as_object_mut()
        .is_some_and(|object| object.remove("prompt_cache").is_some())
}
