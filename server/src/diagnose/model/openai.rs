//! OpenAI-compatible chat-completions adapter (streaming).
//!
//! Talks to any `base_url` exposing the OpenAI `/chat/completions` API with
//! server-sent-event streaming (`stream: true`), which covers OpenAI, Azure
//! OpenAI-compatible gateways, and most local inference servers. Token usage is
//! requested via `stream_options.include_usage` and read from the final chunk.
//!
//! The SSE framing/parse is factored into [`SseAccumulator`] (a pure state
//! machine) so it is unit-tested without a network; the HTTP send is a thin
//! wrapper over `awc`.

use async_trait::async_trait;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{
    ChatMessage, ChatRequest, ChatRole, ModelAdapter, ModelTurn, ResponseFormatSpec, StopReason,
    TokenUsage, ToolCall, ToolChoice,
};

/// OpenAI-compatible streaming adapter.
#[derive(Default)]
pub struct OpenAiCompatAdapter;

impl OpenAiCompatAdapter {
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

/// Map one [`ChatMessage`] to OpenAI message JSON.
///
/// - System / User / Assistant text: a string `content`, or a `text`+`image_url`
///   array when a vision image is attached.
/// - Assistant with tool calls: `content` plus a `tool_calls` array of
///   `{id, type:"function", function:{name, arguments}}`; `content` is `null`
///   when the assistant produced no text alongside the calls.
/// - Tool result: `{role:"tool", tool_call_id, content}` answering one call.
fn message_to_json(m: &ChatMessage) -> Value {
    if m.role == ChatRole::Tool {
        return json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
            "content": m.text,
        });
    }

    // Content: a multimodal array when an image rides along, else a plain string
    // (or null for an assistant turn that is only tool calls).
    let content = match &m.image_data_url {
        Some(url) => json!([
            {"type": "text", "text": m.text},
            {"type": "image_url", "image_url": {"url": url}},
        ]),
        None if m.role == ChatRole::Assistant && m.text.is_empty() && !m.tool_calls.is_empty() => {
            Value::Null
        }
        None => json!(m.text),
    };
    let mut obj = json!({ "role": m.role.as_str(), "content": content });
    if !m.tool_calls.is_empty() {
        obj["tool_calls"] = Value::Array(
            m.tool_calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments_json },
                    })
                })
                .collect(),
        );
    }
    obj
}

/// Build the chat-completions request body, mapping each [`ChatMessage`] to
/// OpenAI message JSON and advertising any tools.
fn build_body(request: &ChatRequest) -> Value {
    let messages: Vec<Value> = request.messages.iter().map(message_to_json).collect();
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    // Ask the gateway to constrain output format. Without any constraint weaker
    // models return prose / fenced markdown and the parser degrades; `json_object`
    // forces valid JSON, `json_schema` additionally locks the shape. The system
    // prompt names the JSON contract (OpenAI's JSON mode requires the word
    // "json"). `None` omits the field for gateways that reject it.
    match &request.response_format {
        ResponseFormatSpec::None => {}
        ResponseFormatSpec::JsonObject => {
            body["response_format"] = json!({ "type": "json_object" });
        }
        ResponseFormatSpec::JsonSchema { name, schema } => {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": { "name": name, "strict": true, "schema": schema },
            });
        }
    }
    // Advertise tools only when some are registered; a tool-free request omits
    // both fields, reproducing the pre-tool-calling body byte-for-byte. With
    // tools present, `tool_choice` steers the model: Auto = decide, None = answer
    // in text without calling, Required = must call at least one.
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters_schema,
                        },
                    })
                })
                .collect(),
        );
        body["tool_choice"] = match request.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
        };
    }
    body
}

/// Join `base_url` with the chat-completions path, tolerating a trailing slash.
fn endpoint(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

#[async_trait(?Send)]
impl ModelAdapter for OpenAiCompatAdapter {
    async fn stream_chat(
        &self,
        request: ChatRequest,
        on_delta: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ModelTurn, AgentError> {
        // Build a TLS-capable client. `awc::Client::default()` has no TLS
        // connector, so it fails instantly on `https://` gateways (hosted
        // providers); a rustls connector handles both http and https.
        let mut root_store = rustls::RootCertStore::empty();
        // `CertificateResult` may carry partial errors; use whatever loaded.
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = root_store.add(cert);
        }
        // Pin the `ring` crypto provider rather than the rustls default
        // (`aws_lc_rs`): on Windows `aws_lc_rs` fast-fails the process with
        // STATUS_STACK_BUFFER_OVERRUN on the first TLS handshake. An explicit
        // provider also makes this independent of the process-wide default.
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
            // awc's per-request default is 5s, which large / hosted models can
            // exceed before the first response bytes arrive. Allow generous
            // headroom for slow first-token latency (streaming body chunks that
            // follow are not bound by this).
            .timeout(std::time::Duration::from_secs(180))
            .insert_header(("Authorization", format!("Bearer {}", request.api_key)))
            .insert_header(("Content-Type", "application/json"))
            .send_json(&body)
            .await
            .map_err(|e| transport_error(format!("model request failed: {e}")))?;

        if !response.status().is_success() {
            return Err(transport_error(format!(
                "model gateway returned status {}",
                response.status()
            )));
        }

        let mut acc = SseAccumulator::new();
        while let Some(chunk) = response.next().await {
            let bytes = chunk.map_err(|e| transport_error(format!("stream error: {e}")))?;
            acc.push_bytes(&bytes, on_delta);
        }
        Ok(acc.finish())
    }

    fn name(&self) -> &'static str {
        "lcxl-openai-compat"
    }
}

/// A tool call assembled incrementally from streamed deltas. OpenAI sends a tool
/// call's `id` / `function.name` once and its `function.arguments` as a sequence
/// of fragments keyed by `index`, so the arguments are concatenated in order.
#[derive(Default)]
struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

/// Incremental parser for an OpenAI SSE stream. Bytes are fed in as they arrive;
/// complete `data:` lines are parsed for content deltas, tool-call deltas, the
/// `finish_reason`, and the final usage.
pub(crate) struct SseAccumulator {
    pending: Vec<u8>,
    content: String,
    usage: TokenUsage,
    tool_calls: Vec<ToolCallBuilder>,
    finish_reason: Option<String>,
}

impl SseAccumulator {
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
            content: String::new(),
            usage: TokenUsage::default(),
            tool_calls: Vec::new(),
            finish_reason: None,
        }
    }

    /// Feed a chunk of bytes, emitting any newly completed content deltas via
    /// `on_delta`. Lines are split on `\n` (ASCII), so multi-byte UTF-8 content
    /// within a line is never split — a line is only decoded once complete. Only
    /// assistant text is streamed; tool-call argument fragments are accumulated
    /// silently (they are provisional until the turn's stop reason is known).
    pub(crate) fn push_bytes(&mut self, chunk: &[u8], on_delta: &(dyn Fn(String) + Send + Sync)) {
        self.pending.extend_from_slice(chunk);
        while let Some(idx) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=idx).collect();
            let line = String::from_utf8_lossy(&line);
            self.handle_line(line.trim(), on_delta);
        }
    }

    fn handle_line(&mut self, line: &str, on_delta: &(dyn Fn(String) + Send + Sync)) {
        let Some(data) = line.strip_prefix("data:") else {
            return; // comments / blank lines / event: fields
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return; // tolerate a malformed chunk rather than aborting the stream
        };
        let choice = &value["choices"][0];
        if let Some(delta) = choice["delta"]["content"].as_str()
            && !delta.is_empty()
        {
            self.content.push_str(delta);
            on_delta(delta.to_string());
        }
        if let Some(calls) = choice["delta"]["tool_calls"].as_array() {
            for call in calls {
                self.accumulate_tool_call(call);
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
            self.usage.input_tokens = usage["prompt_tokens"].as_i64();
            self.usage.output_tokens = usage["completion_tokens"].as_i64();
        }
    }

    /// Fold one streamed `tool_calls[]` entry into the builder at its `index`,
    /// appending argument fragments and recording the id/name when they appear.
    fn accumulate_tool_call(&mut self, call: &Value) {
        let index = call["index"].as_u64().unwrap_or(0) as usize;
        if index >= self.tool_calls.len() {
            self.tool_calls
                .resize_with(index + 1, ToolCallBuilder::default);
        }
        let builder = &mut self.tool_calls[index];
        if let Some(id) = call["id"].as_str()
            && !id.is_empty()
        {
            builder.id = id.to_string();
        }
        if let Some(name) = call["function"]["name"].as_str()
            && !name.is_empty()
        {
            builder.name = name.to_string();
        }
        if let Some(args) = call["function"]["arguments"].as_str() {
            builder.arguments.push_str(args);
        }
    }

    /// Finalize the assembled turn. `finish_reason` maps to a neutral
    /// [`StopReason`] (`stop`→EndTurn, `tool_calls`→ToolUse, `length`→MaxTokens,
    /// anything else / absent → Other); assembled tool calls become [`ToolCall`]s.
    pub(crate) fn finish(self) -> ModelTurn {
        let stop_reason = match self.finish_reason.as_deref() {
            Some("stop") => StopReason::EndTurn,
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            _ => StopReason::Other,
        };
        let tool_calls = self
            .tool_calls
            .into_iter()
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
    use super::super::{ToolCallRef, ToolSpec};
    use super::*;

    fn collect(chunks: &[&[u8]]) -> (ModelTurn, Vec<String>) {
        use std::sync::Mutex;
        let deltas = Mutex::new(Vec::<String>::new());
        let mut acc = SseAccumulator::new();
        let on_delta = |d: String| deltas.lock().unwrap().push(d);
        for chunk in chunks {
            acc.push_bytes(chunk, &on_delta);
        }
        (acc.finish(), deltas.into_inner().unwrap())
    }

    /// A standard stream: content deltas accumulate in order and usage is read
    /// from the final chunk.
    #[test]
    fn parses_deltas_and_usage() {
        let stream = b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n\
data: [DONE]\n\n";
        let (resp, deltas) = collect(&[stream]);
        assert_eq!(resp.text, "Hello world");
        assert_eq!(deltas, vec!["Hello", " world"]);
        assert_eq!(resp.usage.input_tokens, Some(12));
        assert_eq!(resp.usage.output_tokens, Some(3));
    }

    /// Byte boundaries that split a `data:` line mid-way are reassembled.
    #[test]
    fn reassembles_split_chunks() {
        let part1 = b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel";
        let part2 = b"lo\"}}]}\n\ndata: [DONE]\n\n";
        let (resp, deltas) = collect(&[part1, part2]);
        assert_eq!(resp.text, "Hello");
        assert_eq!(deltas, vec!["Hello"]);
    }

    /// Multi-byte UTF-8 content split across a chunk boundary is decoded once the
    /// line completes.
    #[test]
    fn handles_multibyte_across_chunks() {
        // "你好" inside a content delta, bytes split arbitrarily.
        let full = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n".as_bytes();
        let mid = full.len() / 2;
        let (resp, _) = collect(&[&full[..mid], &full[mid..]]);
        assert_eq!(resp.text, "你好");
    }

    /// Non-data lines (SSE comments, blank lines, event fields) are ignored.
    #[test]
    fn ignores_non_data_lines() {
        let stream = b": keep-alive comment\n\
event: message\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
        let (resp, deltas) = collect(&[stream]);
        assert_eq!(resp.text, "x");
        assert_eq!(deltas, vec!["x"]);
    }

    /// A malformed data chunk is skipped without aborting the stream.
    #[test]
    fn tolerates_malformed_chunk() {
        let stream =
            b"data: {not json}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n";
        let (resp, _) = collect(&[stream]);
        assert_eq!(resp.text, "ok");
    }

    #[test]
    fn endpoint_tolerates_trailing_slash() {
        assert_eq!(endpoint("https://x/v1"), "https://x/v1/chat/completions");
        assert_eq!(endpoint("https://x/v1/"), "https://x/v1/chat/completions");
    }

    fn req_with_format(response_format: ResponseFormatSpec) -> ChatRequest {
        ChatRequest {
            base_url: "https://x/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
            messages: vec![
                ChatMessage::text("s", ChatRole::System, "sys"),
                ChatMessage::text("u", ChatRole::User, "look")
                    .with_image("data:image/jpeg;base64,AAA"),
            ],
            response_format,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
        }
    }

    #[test]
    fn body_maps_text_and_vision_messages() {
        let body = build_body(&req_with_format(ResponseFormatSpec::JsonObject));
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        // System message: plain string content.
        assert_eq!(body["messages"][0]["content"], "sys");
        // User message: text + image parts.
        assert_eq!(body["messages"][1]["content"][0]["type"], "text");
        assert_eq!(body["messages"][1]["content"][1]["type"], "image_url");
        assert_eq!(
            body["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/jpeg;base64,AAA"
        );
    }

    /// Each response-format mode serializes to the matching `response_format`
    /// (or omits it for `None`).
    #[test]
    fn body_serializes_each_response_format_mode() {
        // None: the field is omitted entirely (for gateways that reject it).
        let body = build_body(&req_with_format(ResponseFormatSpec::None));
        assert!(body.get("response_format").is_none());

        // JsonObject: plain JSON mode.
        let body = build_body(&req_with_format(ResponseFormatSpec::JsonObject));
        assert_eq!(body["response_format"]["type"], "json_object");

        // JsonSchema: carries the named, strict schema.
        let schema = json!({ "type": "object", "required": ["summary"] });
        let body = build_body(&req_with_format(ResponseFormatSpec::JsonSchema {
            name: "diagnosis".into(),
            schema: schema.clone(),
        }));
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "diagnosis");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
    }

    fn tool_spec() -> ToolSpec {
        ToolSpec {
            name: "file_read".into(),
            description: "read a file".into(),
            parameters_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }
    }

    /// A tool-free request omits both `tools` and `tool_choice`, reproducing the
    /// pre-tool-calling body (the regression contract).
    #[test]
    fn body_omits_tools_when_none_registered() {
        let body = build_body(&req_with_format(ResponseFormatSpec::None));
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    /// Registered tools map to `function` specs, and each `tool_choice` serializes
    /// to its OpenAI token (Auto→"auto", None→"none", Required→"required").
    #[test]
    fn body_advertises_tools_and_each_tool_choice() {
        for (choice, token) in [
            (ToolChoice::Auto, "auto"),
            (ToolChoice::None, "none"),
            (ToolChoice::Required, "required"),
        ] {
            let mut req = req_with_format(ResponseFormatSpec::None);
            req.tools = vec![tool_spec()];
            req.tool_choice = choice;
            let body = build_body(&req);
            assert_eq!(body["tools"][0]["type"], "function");
            assert_eq!(body["tools"][0]["function"]["name"], "file_read");
            assert_eq!(
                body["tools"][0]["function"]["parameters"]["properties"]["path"]["type"],
                "string"
            );
            assert_eq!(body["tool_choice"], token);
        }
    }

    /// An assistant message with tool calls and a following tool-result message
    /// map to OpenAI's `assistant.tool_calls` + `role:"tool"` shapes (history
    /// replay).
    #[test]
    fn body_maps_assistant_tool_calls_and_tool_result() {
        let mut req = req_with_format(ResponseFormatSpec::None);
        req.messages = vec![
            ChatMessage::assistant_tool_calls(
                "a1",
                "",
                vec![ToolCallRef {
                    id: "call_1".into(),
                    name: "file_read".into(),
                    arguments_json: r#"{"path":"/etc/hosts"}"#.into(),
                }],
            ),
            ChatMessage::tool_result("t1", "call_1", "127.0.0.1 localhost"),
        ];
        let body = build_body(&req);
        // Assistant: null content (no text) + a function tool_call carrying the
        // verbatim arguments string.
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert!(body["messages"][0]["content"].is_null());
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"/etc/hosts"}"#
        );
        // Tool result: role "tool" linked by tool_call_id.
        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
        assert_eq!(body["messages"][1]["content"], "127.0.0.1 localhost");
    }

    /// A plain text stream ending with `finish_reason:"stop"` is an EndTurn answer
    /// with no tool calls.
    #[test]
    fn stream_text_answer_is_end_turn() {
        let stream =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";
        let (turn, _) = collect(&[stream]);
        assert_eq!(turn.text, "hi");
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        assert!(turn.tool_calls.is_empty());
    }

    /// Tool-call deltas are assembled across fragments: the id/name arrive once
    /// and the arguments are concatenated in order; `finish_reason:"tool_calls"`
    /// maps to ToolUse. Text is not streamed for a tool turn.
    #[test]
    fn stream_assembles_tool_call() {
        let stream = b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"function\":{\"name\":\"file_read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"/x\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";
        let (turn, deltas) = collect(&[stream]);
        assert_eq!(turn.stop_reason, StopReason::ToolUse);
        assert!(deltas.is_empty(), "tool arguments are not streamed as text");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_9");
        assert_eq!(turn.tool_calls[0].name, "file_read");
        assert_eq!(turn.tool_calls[0].arguments_json, r#"{"path":"/x"}"#);
    }

    /// `finish_reason:"length"` maps to MaxTokens; an unrecognized / absent reason
    /// maps to Other (both discarded by the loop).
    #[test]
    fn stream_maps_length_and_unknown_stop_reasons() {
        let length = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
        let (turn, _) = collect(&[length]);
        assert_eq!(turn.stop_reason, StopReason::MaxTokens);

        let weird = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"content_filter\"}]}\n\ndata: [DONE]\n\n";
        let (turn, _) = collect(&[weird]);
        assert_eq!(turn.stop_reason, StopReason::Other);
    }
}
