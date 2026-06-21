//! Anthropic Messages API adapter (streaming).
//!
//! Talks to the Anthropic `/v1/messages` API with server-sent-event streaming
//! (`stream: true`). This is a deliberately different wire shape from the
//! OpenAI-compatible adapter — it exercises the [`ModelAdapter`] abstraction
//! against a second, structurally distinct protocol:
//!
//! - `system` is a **top-level** field, not a message with `role: "system"`.
//! - `max_tokens` is **required**.
//! - the SSE events are typed (`message_start` / `content_block_delta` /
//!   `message_delta` / ...) and usage is split across `message_start`
//!   (`input_tokens`) and `message_delta` (`output_tokens`).
//! - images are `{"type":"image","source":{"type":"base64",media_type,data}}`.
//! - auth is `x-api-key` + `anthropic-version`, not `Authorization: Bearer`.
//!
//! Anthropic has no OpenAI-style `response_format`; the JSON contract is carried
//! by the system prompt and the parser degrades gracefully, so
//! [`ResponseFormatSpec`] is intentionally not mapped here.
//!
//! The SSE framing/parse is factored into [`AnthropicSseAccumulator`] (a pure
//! state machine) so it is unit-tested without a network; the HTTP send is a
//! thin wrapper over `awc`.

use std::collections::BTreeMap;

use async_trait::async_trait;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{
    ChatMessage, ChatRequest, ChatRole, ModelAdapter, ModelTurn, StopReason, TokenUsage, ToolCall,
    ToolChoice,
};

/// Upper bound on generated tokens. Anthropic requires `max_tokens`; the
/// diagnosis output is short, so a generous fixed cap is enough for now.
const MAX_TOKENS: u32 = 4096;

/// Anthropic API version header value (the dated, stable contract).
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Messages streaming adapter.
#[derive(Default)]
pub struct AnthropicAdapter;

impl AnthropicAdapter {
    pub fn new() -> Self {
        Self
    }
}

fn transport_error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::TransportError,
        message: message.into(),
        retryable: true,
        safe_for_model: true,
    }
}

/// Split a `data:` image URL into `(media_type, base64_payload)`. Returns `None`
/// for anything that is not a base64 data URL, so the caller can fall back to a
/// text-only message rather than sending a malformed image block.
fn split_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    if media_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((media_type, data))
}

/// Map one non-system [`ChatMessage`] to an Anthropic `messages[]` entry.
///
/// - Tool result → a **user** turn carrying a `tool_result` block (Anthropic has
///   no `tool` role; results are user-role content blocks).
/// - Assistant with tool calls → an assistant turn whose content is an optional
///   `text` block followed by `tool_use` blocks (`input` is the parsed arguments).
/// - Plain text → a string content, or a `text`+`image` block array with a
///   vision image.
fn message_to_json(m: &ChatMessage) -> Value {
    if m.role == ChatRole::Tool {
        return json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                "content": m.text,
            }],
        });
    }

    if m.role == ChatRole::Assistant && !m.tool_calls.is_empty() {
        let mut blocks: Vec<Value> = Vec::new();
        if !m.text.is_empty() {
            blocks.push(json!({"type": "text", "text": m.text}));
        }
        for c in &m.tool_calls {
            // The arguments were produced by the model as a JSON object; fall back
            // to an empty object if a replayed string is not parseable.
            let input: Value = serde_json::from_str(&c.arguments_json)
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
            blocks.push(json!({
                "type": "tool_use",
                "id": c.id,
                "name": c.name,
                "input": input,
            }));
        }
        return json!({"role": "assistant", "content": blocks});
    }

    let content = match m.image_data_url.as_deref().and_then(split_data_url) {
        Some((media_type, data)) => json!([
            {"type": "text", "text": m.text},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }},
        ]),
        None => json!(m.text),
    };
    json!({"role": m.role.as_str(), "content": content})
}

/// Build the Messages request body, mapping the generic [`ChatRequest`] to the
/// Anthropic shape: system text is hoisted to the top-level `system` field, the
/// remaining messages become the `messages` array, and any tools are advertised
/// with `input_schema`.
fn build_body(request: &ChatRequest) -> Value {
    // System messages are merged into the top-level `system` field; everything
    // else becomes a turn in `messages`.
    let mut system = String::new();
    let mut messages: Vec<Value> = Vec::new();
    for m in &request.messages {
        if m.role == ChatRole::System {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(&m.text);
            continue;
        }
        messages.push(message_to_json(m));
    }

    let mut body = json!({
        "model": request.model,
        "max_tokens": MAX_TOKENS,
        "stream": true,
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    // Anthropic has no `tool_choice:"none"`; that intent is expressed by omitting
    // the tools entirely. So advertise tools only when some are registered AND the
    // choice is not None; map Auto→`{type:"auto"}`, Required→`{type:"any"}`.
    if request.tool_choice != ToolChoice::None && !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters_schema,
                    })
                })
                .collect(),
        );
        body["tool_choice"] = match request.tool_choice {
            ToolChoice::Required => json!({"type": "any"}),
            // Auto (and the unreachable None) map to the model deciding.
            _ => json!({"type": "auto"}),
        };
    }
    body
}

/// Join `base_url` with the Messages path, tolerating a trailing slash. The
/// configured `base_url` is the host root (e.g. `https://api.anthropic.com`).
fn endpoint(base_url: &str) -> String {
    format!("{}/v1/messages", base_url.trim_end_matches('/'))
}

#[async_trait(?Send)]
impl ModelAdapter for AnthropicAdapter {
    async fn stream_chat(
        &self,
        request: ChatRequest,
        on_delta: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ModelTurn, AgentError> {
        // Build a TLS-capable client (see the OpenAI adapter for the rationale on
        // the rustls connector and pinning the `ring` crypto provider).
        let mut root_store = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = root_store.add(cert);
        }
        let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default TLS protocol versions")
        .with_root_certificates(std::sync::Arc::new(root_store))
        .with_no_client_auth();
        let client = awc::Client::builder()
            .connector(
                awc::Connector::new()
                    .timeout(std::time::Duration::from_secs(30))
                    .rustls_0_23(std::sync::Arc::new(tls)),
            )
            .finish();
        let body = build_body(&request);
        let mut response = client
            .post(endpoint(&request.base_url))
            // Generous headroom for slow first-token latency (see OpenAI adapter).
            .timeout(std::time::Duration::from_secs(180))
            .insert_header(("x-api-key", request.api_key.clone()))
            .insert_header(("anthropic-version", ANTHROPIC_VERSION))
            .insert_header(("Content-Type", "application/json"))
            .send_json(&body)
            .await
            .map_err(|e| transport_error(format!("model request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            // Read the gateway's error body (bounded) so the precise reason is
            // visible instead of a bare status. The body is the gateway's own
            // error text and never contains our api_key, so it is safe to log.
            let body = response.body().limit(16 * 1024).await.unwrap_or_default();
            let detail = super::gateway_error_detail(&body);
            log::warn!("anthropic gateway returned {status}: {detail}");
            return Err(transport_error(if detail.is_empty() {
                format!("model gateway returned status {status}")
            } else {
                format!("model gateway returned status {status}: {detail}")
            }));
        }

        let mut acc = AnthropicSseAccumulator::new();
        while let Some(chunk) = response.next().await {
            let bytes = chunk.map_err(|e| transport_error(format!("stream error: {e}")))?;
            acc.push_bytes(&bytes, on_delta);
        }
        Ok(acc.finish())
    }

    fn name(&self) -> &'static str {
        "lcxl-anthropic"
    }
}

/// A tool-use block assembled incrementally from an Anthropic stream. The block's
/// `id` / `name` arrive on `content_block_start`; its JSON `input` arrives as a
/// sequence of `input_json_delta.partial_json` fragments concatenated in order.
#[derive(Default)]
struct ToolUseBuilder {
    id: String,
    name: String,
    arguments: String,
}

/// Incremental parser for an Anthropic Messages SSE stream. Bytes are fed in as
/// they arrive; complete `data:` lines are parsed by their `type` for text
/// deltas, tool-use blocks, the stop reason, and usage. The `event:` lines are
/// ignored — the `data` payload carries the discriminating `type`, mirroring the
/// OpenAI accumulator's data-only view.
pub(crate) struct AnthropicSseAccumulator {
    pending: Vec<u8>,
    content: String,
    usage: TokenUsage,
    /// Tool-use blocks keyed by their stream `index` (ordering preserved).
    tool_uses: BTreeMap<usize, ToolUseBuilder>,
    stop_reason: Option<String>,
}

impl AnthropicSseAccumulator {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            content: String::new(),
            usage: TokenUsage::default(),
            tool_uses: BTreeMap::new(),
            stop_reason: None,
        }
    }

    /// Feed a chunk of bytes, emitting any newly completed text deltas via
    /// `on_delta`. Lines are split on `\n` (ASCII), so multi-byte UTF-8 content
    /// within a line is never split — a line is only decoded once complete. Only
    /// assistant text is streamed; tool-use input fragments are accumulated
    /// silently (they are provisional until the stop reason is known).
    fn push_bytes(&mut self, chunk: &[u8], on_delta: &(dyn Fn(String) + Send + Sync)) {
        self.pending.extend_from_slice(chunk);
        while let Some(idx) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=idx).collect();
            let line = String::from_utf8_lossy(&line);
            self.handle_line(line.trim(), on_delta);
        }
    }

    fn handle_line(&mut self, line: &str, on_delta: &(dyn Fn(String) + Send + Sync)) {
        let Some(data) = line.strip_prefix("data:") else {
            return; // `event:` lines, comments, blank lines
        };
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return; // tolerate a malformed chunk rather than aborting the stream
        };
        match value["type"].as_str() {
            Some("content_block_start") => {
                // A tool_use block opens here with its id and name; a text block
                // needs no setup. Index keys it for the following input deltas.
                if value["content_block"]["type"] == "tool_use"
                    && let Some(index) = value["index"].as_u64()
                {
                    let builder = self.tool_uses.entry(index as usize).or_default();
                    if let Some(id) = value["content_block"]["id"].as_str() {
                        builder.id = id.to_string();
                    }
                    if let Some(name) = value["content_block"]["name"].as_str() {
                        builder.name = name.to_string();
                    }
                }
            }
            Some("content_block_delta") => {
                let delta = &value["delta"];
                if delta["type"] == "text_delta"
                    && let Some(text) = delta["text"].as_str()
                    && !text.is_empty()
                {
                    self.content.push_str(text);
                    on_delta(text.to_string());
                } else if delta["type"] == "input_json_delta"
                    && let Some(partial) = delta["partial_json"].as_str()
                    && let Some(index) = value["index"].as_u64()
                {
                    self.tool_uses
                        .entry(index as usize)
                        .or_default()
                        .arguments
                        .push_str(partial);
                }
            }
            Some("message_start") => {
                let usage = &value["message"]["usage"];
                if let Some(input) = usage["input_tokens"].as_i64() {
                    self.usage.input_tokens = Some(input);
                }
                if let Some(output) = usage["output_tokens"].as_i64() {
                    self.usage.output_tokens = Some(output);
                }
            }
            Some("message_delta") => {
                // The cumulative output token count and the stop reason land here.
                if let Some(output) = value["usage"]["output_tokens"].as_i64() {
                    self.usage.output_tokens = Some(output);
                }
                if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_string());
                }
            }
            _ => {} // ping / content_block_stop / message_stop
        }
    }

    /// Finalize the assembled turn. The Anthropic `stop_reason` maps to a neutral
    /// [`StopReason`] (`end_turn`→EndTurn, `tool_use`→ToolUse,
    /// `max_tokens`→MaxTokens, anything else / absent → Other); assembled tool-use
    /// blocks become [`ToolCall`]s in index order.
    fn finish(self) -> ModelTurn {
        let stop_reason = match self.stop_reason.as_deref() {
            Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            _ => StopReason::Other,
        };
        let tool_calls = self
            .tool_uses
            .into_values()
            .map(|b| ToolCall {
                id: b.id,
                name: b.name,
                arguments_json: b.arguments,
            })
            .collect();
        ModelTurn {
            text: self.content,
            tool_calls,
            stop_reason,
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ChatMessage, ChatRole};
    use super::*;

    fn collect(chunks: &[&[u8]]) -> (ModelTurn, Vec<String>) {
        use std::sync::Mutex;
        let deltas = Mutex::new(Vec::<String>::new());
        let mut acc = AnthropicSseAccumulator::new();
        let on_delta = |d: String| deltas.lock().unwrap().push(d);
        for chunk in chunks {
            acc.push_bytes(chunk, &on_delta);
        }
        (acc.finish(), deltas.into_inner().unwrap())
    }

    /// A standard stream: text deltas accumulate in order, input tokens come from
    /// `message_start` and the final output tokens from `message_delta`.
    #[test]
    fn parses_deltas_and_usage() {
        let stream = b"event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";
        let (resp, deltas) = collect(&[stream]);
        assert_eq!(resp.text, "Hello world");
        assert_eq!(deltas, vec!["Hello", " world"]);
        assert_eq!(resp.usage.input_tokens, Some(25));
        assert_eq!(resp.usage.output_tokens, Some(7));
    }

    /// Byte boundaries that split a `data:` line mid-way are reassembled.
    #[test]
    fn reassembles_split_chunks() {
        let part1 = b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel";
        let part2 = b"lo\"}}\n\n";
        let (resp, deltas) = collect(&[part1, part2]);
        assert_eq!(resp.text, "Hello");
        assert_eq!(deltas, vec!["Hello"]);
    }

    /// Multi-byte UTF-8 content split across a chunk boundary is decoded once the
    /// line completes.
    #[test]
    fn handles_multibyte_across_chunks() {
        let full = "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n".as_bytes();
        let mid = full.len() / 2;
        let (resp, _) = collect(&[&full[..mid], &full[mid..]]);
        assert_eq!(resp.text, "你好");
    }

    /// Non-data lines (the `event:` field, comments, blank lines) are ignored;
    /// only the typed `data` payload drives parsing.
    #[test]
    fn ignores_non_data_lines() {
        let stream = b": ping\n\
event: content_block_delta\n\
\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n";
        let (resp, deltas) = collect(&[stream]);
        assert_eq!(resp.text, "x");
        assert_eq!(deltas, vec!["x"]);
    }

    /// A malformed data chunk is skipped without aborting the stream, and a
    /// non-text delta (e.g. a tool-use `input_json_delta`) is not accumulated as
    /// text.
    #[test]
    fn tolerates_malformed_and_non_text_deltas() {
        let stream = b"data: {not json}\n\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n";
        let (resp, deltas) = collect(&[stream]);
        assert_eq!(resp.text, "ok");
        assert_eq!(deltas, vec!["ok"]);
    }

    #[test]
    fn endpoint_tolerates_trailing_slash() {
        assert_eq!(
            endpoint("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            endpoint("https://api.anthropic.com/"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    fn req() -> ChatRequest {
        ChatRequest {
            base_url: "https://api.anthropic.com".into(),
            api_key: "k".into(),
            model: "claude-x".into(),
            messages: vec![
                ChatMessage::text("s", ChatRole::System, "you are a diagnostician"),
                ChatMessage::text("u", ChatRole::User, "look")
                    .with_image("data:image/jpeg;base64,AAA"),
            ],
            // Anthropic ignores response_format; any value maps the same.
            response_format: super::super::ResponseFormatSpec::JsonObject,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
        }
    }

    /// The system message is hoisted to the top-level `system` field; it never
    /// appears as a `role: "system"` turn in `messages`. `max_tokens` and
    /// `stream` are set.
    #[test]
    fn body_hoists_system_and_sets_required_fields() {
        let body = build_body(&req());
        assert_eq!(body["system"], "you are a diagnostician");
        assert_eq!(body["max_tokens"], MAX_TOKENS);
        assert_eq!(body["stream"], true);
        // Only the user turn is in messages — no system role.
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    /// A vision image maps to a base64 `image` block with the media type split
    /// out of the data URL.
    #[test]
    fn body_maps_vision_image_block() {
        let body = build_body(&req());
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "look");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(content[1]["source"]["data"], "AAA");
    }

    /// A user message without an image is a plain string content.
    #[test]
    fn body_maps_text_only_message_as_string() {
        let mut request = req();
        request.messages[1].image_data_url = None;
        let body = build_body(&request);
        assert_eq!(body["messages"][0]["content"], "look");
    }

    /// A malformed data URL falls back to text-only rather than emitting a broken
    /// image block.
    #[test]
    fn split_data_url_rejects_non_base64() {
        assert!(split_data_url("https://example/x.png").is_none());
        assert!(split_data_url("data:image/png,AAA").is_none());
        assert!(split_data_url("data:image/png;base64,").is_none());
        assert_eq!(
            split_data_url("data:image/png;base64,QkJC"),
            Some(("image/png", "QkJC"))
        );
    }

    fn tool_spec() -> super::super::ToolSpec {
        super::super::ToolSpec {
            name: "file_read".into(),
            description: "read a file".into(),
            parameters_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }
    }

    /// Registered tools map to Anthropic `input_schema` specs; Auto→`{type:"auto"}`
    /// and Required→`{type:"any"}`.
    #[test]
    fn body_advertises_tools_with_input_schema() {
        for (choice, kind) in [(ToolChoice::Auto, "auto"), (ToolChoice::Required, "any")] {
            let mut r = req();
            r.tools = vec![tool_spec()];
            r.tool_choice = choice;
            let body = build_body(&r);
            assert_eq!(body["tools"][0]["name"], "file_read");
            assert_eq!(
                body["tools"][0]["input_schema"]["properties"]["path"]["type"],
                "string"
            );
            assert_eq!(body["tool_choice"]["type"], kind);
        }
    }

    /// `ToolChoice::None` is expressed by omitting `tools` entirely — the
    /// structural difference from the OpenAI adapter (which sends
    /// `tool_choice:"none"`).
    #[test]
    fn tool_choice_none_omits_tools() {
        let mut r = req();
        r.tools = vec![tool_spec()];
        r.tool_choice = ToolChoice::None;
        let body = build_body(&r);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    /// An assistant tool call maps to a `tool_use` block (with parsed `input`), and
    /// a tool result maps to a **user** turn carrying a `tool_result` block — there
    /// is no `tool` role in Anthropic.
    #[test]
    fn body_maps_assistant_tool_use_and_tool_result() {
        let mut r = req();
        r.messages = vec![
            ChatMessage::assistant_tool_calls(
                "a1",
                "checking",
                vec![super::super::ToolCallRef {
                    id: "toolu_1".into(),
                    name: "file_read".into(),
                    arguments_json: r#"{"path":"/x"}"#.into(),
                }],
            ),
            ChatMessage::tool_result("t1", "toolu_1", "contents"),
        ];
        let body = build_body(&r);
        // Assistant turn: optional text block then a tool_use block with parsed input.
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["messages"][0]["content"][1]["type"], "tool_use");
        assert_eq!(body["messages"][0]["content"][1]["id"], "toolu_1");
        assert_eq!(body["messages"][0]["content"][1]["input"]["path"], "/x");
        // Tool result rides on a user turn as a tool_result block.
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(body["messages"][1]["content"][0]["content"], "contents");
    }

    /// A tool-use turn: the block id/name come from `content_block_start` and the
    /// JSON input is concatenated from `input_json_delta` fragments;
    /// `stop_reason:"tool_use"` maps to ToolUse. Tool input is not streamed as text.
    #[test]
    fn stream_assembles_tool_use_block() {
        let stream = b"event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"file_read\",\"input\":{}}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\"\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":\\\"/x\\\"}\"}}\n\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n";
        let (turn, deltas) = collect(&[stream]);
        assert_eq!(turn.stop_reason, StopReason::ToolUse);
        assert!(deltas.is_empty(), "tool input is not streamed as text");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "toolu_9");
        assert_eq!(turn.tool_calls[0].name, "file_read");
        assert_eq!(turn.tool_calls[0].arguments_json, r#"{"path":"/x"}"#);
    }

    /// Stop-reason mapping: `end_turn`→EndTurn, `max_tokens`→MaxTokens, unknown /
    /// absent → Other.
    #[test]
    fn stream_maps_stop_reasons() {
        let max = b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":1}}\n\n";
        assert_eq!(collect(&[max]).0.stop_reason, StopReason::MaxTokens);

        // No message_delta at all → no stop reason → Other.
        let none = b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        assert_eq!(collect(&[none]).0.stop_reason, StopReason::Other);
    }
}
