//! ManagerProxy gateway client.
//!
//! In `GatewayMode::ManagerProxy` the desk server holds no provider credentials.
//! It relays the prompt to the manager's model-proxy endpoint (authenticated
//! with its `manager_api_token`); the manager injects the provider credentials,
//! streams from the provider, and forwards an OpenAI-compatible SSE stream back.
//! The desk server parses that stream with the same [`SseAccumulator`] the direct
//! adapter uses, so token-by-token streaming and usage accounting are preserved.

use std::sync::Arc;

use desk_agent_protocol::model_proxy::{
    MODEL_PROXY_PATH, ProxyChatMessage, ProxyChatRequest, ProxyResponseFormat,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use futures_util::StreamExt;

use super::openai::SseAccumulator;
use super::{ChatMessage, ChatResponse, ResponseFormatSpec};

fn transport_error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::TransportError,
        message: message.into(),
        retryable: true,
        safe_for_model: true,
    }
}

/// Map the desk server's chat messages to the proxy wire form.
fn to_proxy_messages(messages: &[ChatMessage]) -> Vec<ProxyChatMessage> {
    messages
        .iter()
        .map(|m| ProxyChatMessage {
            role: m.role.as_str().to_string(),
            text: m.text.clone(),
            image_data_url: m.image_data_url.clone(),
        })
        .collect()
}

/// Map the desk server's response-format spec to the proxy wire form.
fn to_proxy_format(format: &ResponseFormatSpec) -> ProxyResponseFormat {
    match format {
        ResponseFormatSpec::None => ProxyResponseFormat::None,
        ResponseFormatSpec::JsonObject => ProxyResponseFormat::JsonObject,
        ResponseFormatSpec::JsonSchema { name, schema } => ProxyResponseFormat::JsonSchema {
            name: name.clone(),
            schema: schema.clone(),
        },
    }
}

/// Derive the manager's HTTP(S) base URL from the (WebSocket) `manager_url` used
/// for signaling: `ws`→`http`, `wss`→`https`, keep the authority, drop the path.
/// Returns `None` for an unrecognized scheme or empty authority.
pub(crate) fn http_base(manager_url: &str) -> Option<String> {
    let s = manager_url.trim();
    let (scheme, rest) = if let Some(r) = s.strip_prefix("wss://") {
        ("https", r)
    } else if let Some(r) = s.strip_prefix("ws://") {
        ("http", r)
    } else if let Some(r) = s.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = s.strip_prefix("http://") {
        ("http", r)
    } else {
        return None;
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// Build the full proxy endpoint URL from the signaling `manager_url`.
pub(crate) fn proxy_url(manager_url: &str) -> Option<String> {
    http_base(manager_url).map(|base| format!("{base}{MODEL_PROXY_PATH}"))
}

/// Build a TLS-capable `awc` client (the manager may be reached over https).
fn build_client() -> awc::Client {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = root_store.add(cert);
    }
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(Arc::new(root_store))
        .with_no_client_auth();
    awc::Client::builder()
        .connector(
            awc::Connector::new()
                .timeout(std::time::Duration::from_secs(30))
                .rustls_0_23(Arc::new(tls)),
        )
        .finish()
}

/// Relay a chat completion through the manager proxy and parse the streamed SSE
/// response, emitting content deltas via `on_delta`.
#[allow(clippy::too_many_arguments)]
pub async fn stream_proxy_chat(
    manager_url: &str,
    manager_api_token: &str,
    messages: &[ChatMessage],
    response_format: &ResponseFormatSpec,
    source_request_id: Option<String>,
    client_id: Option<String>,
    on_delta: &(dyn Fn(String) + Send + Sync),
) -> Result<ChatResponse, AgentError> {
    let url = proxy_url(manager_url).ok_or_else(|| AgentError {
        kind: AgentErrorKind::UnsupportedCapability,
        message: "manager URL is not configured for the model proxy".to_string(),
        retryable: false,
        safe_for_model: true,
    })?;
    // The manager owns the model-accounting audit in this gateway mode; carry the
    // diagnose frame id (ledger key) + device identity so it can attribute that
    // audit to the real operator.
    let request = ProxyChatRequest {
        messages: to_proxy_messages(messages),
        response_format: to_proxy_format(response_format),
        source_request_id,
        client_id,
    };

    let client = build_client();
    let mut response = client
        .post(url)
        .timeout(std::time::Duration::from_secs(180))
        .insert_header(("Authorization", format!("Bearer {manager_api_token}")))
        .insert_header(("Content-Type", "application/json"))
        .send_json(&request)
        .await
        .map_err(|e| transport_error(format!("manager proxy request failed: {e}")))?;

    if !response.status().is_success() {
        return Err(transport_error(format!(
            "manager proxy returned status {}",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_base_converts_ws_schemes() {
        assert_eq!(
            http_base("ws://10.0.0.1:8080/api/desk/signaling").as_deref(),
            Some("http://10.0.0.1:8080")
        );
        assert_eq!(
            http_base("wss://manager.example/api/desk/signaling").as_deref(),
            Some("https://manager.example")
        );
        assert_eq!(
            http_base("https://manager.example/x").as_deref(),
            Some("https://manager.example")
        );
        assert_eq!(http_base("garbage"), None);
        assert_eq!(http_base("ws://"), None);
    }

    #[test]
    fn proxy_url_appends_path() {
        assert_eq!(
            proxy_url("ws://10.0.0.1:8080/api/desk/signaling").as_deref(),
            Some("http://10.0.0.1:8080/api/model/proxy")
        );
    }

    #[test]
    fn message_and_format_mapping() {
        use super::super::ChatRole;
        let msgs = vec![ChatMessage {
            role: ChatRole::User,
            text: "hi".into(),
            image_data_url: Some("data:img".into()),
        }];
        let pm = to_proxy_messages(&msgs);
        assert_eq!(pm[0].role, "user");
        assert_eq!(pm[0].image_data_url.as_deref(), Some("data:img"));
        assert_eq!(
            to_proxy_format(&ResponseFormatSpec::JsonObject),
            ProxyResponseFormat::JsonObject
        );
    }
}
