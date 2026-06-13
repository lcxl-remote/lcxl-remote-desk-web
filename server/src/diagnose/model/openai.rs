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

use super::{ChatRequest, ChatResponse, ModelAdapter, ResponseFormatSpec, TokenUsage};

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

/// Build the chat-completions request body, mapping each [`super::ChatMessage`]
/// to OpenAI message JSON (string content, or a text+image_url array when a
/// vision image is attached).
fn build_body(request: &ChatRequest) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|m| match &m.image_data_url {
            Some(url) => json!({
                "role": m.role.as_str(),
                "content": [
                    {"type": "text", "text": m.text},
                    {"type": "image_url", "image_url": {"url": url}},
                ],
            }),
            None => json!({"role": m.role.as_str(), "content": m.text}),
        })
        .collect();
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
    ) -> Result<ChatResponse, AgentError> {
        // Build a TLS-capable client. `awc::Client::default()` has no TLS
        // connector, so it fails instantly on `https://` gateways (hosted
        // providers); a rustls connector handles both http and https. The crypto
        // provider install is idempotent — production daemon/worker already
        // install one, but the diagnose path may also run in contexts that did
        // not.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut root_store = rustls::RootCertStore::empty();
        // `CertificateResult` may carry partial errors; use whatever loaded.
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = root_store.add(cert);
        }
        let tls = rustls::ClientConfig::builder()
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

/// Incremental parser for an OpenAI SSE stream. Bytes are fed in as they arrive;
/// complete `data:` lines are parsed for content deltas and the final usage.
pub(crate) struct SseAccumulator {
    pending: Vec<u8>,
    content: String,
    usage: TokenUsage,
}

impl SseAccumulator {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            content: String::new(),
            usage: TokenUsage::default(),
        }
    }

    /// Feed a chunk of bytes, emitting any newly completed content deltas via
    /// `on_delta`. Lines are split on `\n` (ASCII), so multi-byte UTF-8 content
    /// within a line is never split — a line is only decoded once complete.
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
            return; // comments / blank lines / event: fields
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return; // tolerate a malformed chunk rather than aborting the stream
        };
        if let Some(delta) = value["choices"][0]["delta"]["content"].as_str()
            && !delta.is_empty()
        {
            self.content.push_str(delta);
            on_delta(delta.to_string());
        }
        if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
            self.usage.input_tokens = usage["prompt_tokens"].as_i64();
            self.usage.output_tokens = usage["completion_tokens"].as_i64();
        }
    }

    fn finish(self) -> ChatResponse {
        ChatResponse {
            content: self.content,
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> (ChatResponse, Vec<String>) {
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
        assert_eq!(resp.content, "Hello world");
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
        assert_eq!(resp.content, "Hello");
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
        assert_eq!(resp.content, "你好");
    }

    /// Non-data lines (SSE comments, blank lines, event fields) are ignored.
    #[test]
    fn ignores_non_data_lines() {
        let stream = b": keep-alive comment\n\
event: message\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
        let (resp, deltas) = collect(&[stream]);
        assert_eq!(resp.content, "x");
        assert_eq!(deltas, vec!["x"]);
    }

    /// A malformed data chunk is skipped without aborting the stream.
    #[test]
    fn tolerates_malformed_chunk() {
        let stream =
            b"data: {not json}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n";
        let (resp, _) = collect(&[stream]);
        assert_eq!(resp.content, "ok");
    }

    #[test]
    fn endpoint_tolerates_trailing_slash() {
        assert_eq!(endpoint("https://x/v1"), "https://x/v1/chat/completions");
        assert_eq!(endpoint("https://x/v1/"), "https://x/v1/chat/completions");
    }

    fn req_with_format(response_format: ResponseFormatSpec) -> ChatRequest {
        use super::super::{ChatMessage, ChatRole};
        ChatRequest {
            base_url: "https://x/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
            messages: vec![
                ChatMessage {
                    role: ChatRole::System,
                    text: "sys".into(),
                    image_data_url: None,
                },
                ChatMessage {
                    role: ChatRole::User,
                    text: "look".into(),
                    image_data_url: Some("data:image/jpeg;base64,AAA".into()),
                },
            ],
            response_format,
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
}
