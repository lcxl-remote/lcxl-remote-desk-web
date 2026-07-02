//! Desk-server → manager model-proxy carrier (the `ManagerProxy` gateway).
//!
//! In the `ManagerProxy` gateway mode the desk server does not hold provider
//! credentials. Instead it sends the prompt (messages + response-format
//! preference) to the manager's model-proxy endpoint, authenticated with its
//! `manager_api_token`; the manager fills in the provider `base_url` / `api_key`
//! / `model` from its own (DB-stored) provider config, dials the provider with
//! streaming, and relays the OpenAI-compatible server-sent-event stream back
//! verbatim. Credentials never leave the manager.
//!
//! The response body is an OpenAI-style SSE stream, so the desk server reuses
//! its existing SSE accumulator to parse deltas + usage — there is no separate
//! response DTO here. Only the **request** shape is shared.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// HTTP path of the manager model-proxy endpoint (relative to the manager URL).
pub const MODEL_PROXY_PATH: &str = "/api/model/proxy";

/// One chat message in a proxied request. `role` is `system` / `user`;
/// `image_data_url` carries an optional vision attachment (a `data:` URL).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProxyChatMessage {
    pub role: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_data_url: Option<String>,
}

/// The output-format constraint the model should honor, mirroring the desk
/// server's response-format modes. The manager maps this onto the provider's
/// `response_format` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProxyResponseFormat {
    /// No constraint (the provider may reject an empty `response_format`, so it
    /// is omitted entirely).
    #[default]
    None,
    /// Plain JSON-object mode.
    JsonObject,
    /// Named, strict JSON schema.
    JsonSchema {
        name: String,
        schema: serde_json::Value,
    },
}

/// A proxied chat request. The manager supplies the provider, model, base URL,
/// and API key from its own config; the desk server only supplies the prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProxyChatRequest {
    pub messages: Vec<ProxyChatMessage>,
    #[serde(default)]
    pub response_format: ProxyResponseFormat,
    /// The originating diagnose frame `request_id` — the manager's authorization
    /// ledger key. The manager records the model-accounting audit (which it owns
    /// in this gateway mode) under this key so it can be attributed to the real
    /// operator. `None` on legacy callers; the manager then attributes to the
    /// token owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_request_id: Option<String>,
    /// The desk server's stable device identity (same source as the signaling
    /// `VersionInfo.client_id`). The manager resolves it to a trusted device id
    /// and only applies the operator attribution when it matches the ledger's
    /// recorded device. `None` disables device-matched attribution (falls back
    /// to the token owner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Optional upper bound on the tokens the model may generate. Forwarded to the
    /// provider as `max_tokens` and used by the manager to estimate an admission
    /// hold. `None` on legacy callers; the manager then falls back to its platform
    /// default cap for the estimate and lets the provider apply its own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_format_round_trips() {
        for v in [
            ProxyResponseFormat::None,
            ProxyResponseFormat::JsonObject,
            ProxyResponseFormat::JsonSchema {
                name: "diagnosis".into(),
                schema: serde_json::json!({"type":"object"}),
            },
        ] {
            let s = serde_json::to_string(&v).unwrap();
            let back: ProxyResponseFormat = serde_json::from_str(&s).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn request_round_trips() {
        let req = ProxyChatRequest {
            messages: vec![
                ProxyChatMessage {
                    role: "system".into(),
                    text: "sys".into(),
                    image_data_url: None,
                },
                ProxyChatMessage {
                    role: "user".into(),
                    text: "look".into(),
                    image_data_url: Some("data:image/jpeg;base64,AAA".into()),
                },
            ],
            response_format: ProxyResponseFormat::JsonObject,
            source_request_id: Some("req-1".into()),
            client_id: Some("client-abc".into()),
            max_tokens: Some(4096),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: ProxyChatRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn request_accepts_legacy_payload_without_attribution_fields() {
        // Older desk servers omit source_request_id / client_id; they default to
        // None so the manager attributes to the token owner.
        let json = r#"{"messages":[],"response_format":{"mode":"none"}}"#;
        let req: ProxyChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.source_request_id, None);
        assert_eq!(req.client_id, None);
        assert_eq!(req.max_tokens, None);
    }
}
