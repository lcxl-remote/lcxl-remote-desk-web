//! Opaque provider replay material carried with assistant tool-call groups.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::chat::StopReason;
use crate::model_profile::WireProtocol;

pub const PROVIDER_REPLAY_SCHEMA_VERSION: u16 = 1;

/// Versioned digest binding opaque replay material to its provider context.
/// Endpoint URLs and credentials are never copied into the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceContextKey(String);

impl SourceContextKey {
    /// Bind replay to an exact configured connection and endpoint without
    /// persisting either value in the conversation. Trimming surrounding
    /// whitespace and trailing slashes treats the two common spelling variants
    /// as the same endpoint; the remaining URL is deliberately case-sensitive
    /// because URL paths can be case-sensitive.
    pub fn derive_for_endpoint(
        protocol: WireProtocol,
        connection_identity: &str,
        endpoint: &str,
        model_identity: &str,
        model_string: &str,
    ) -> Self {
        let endpoint = endpoint.trim().trim_end_matches('/');
        let connection_and_endpoint = format!("{connection_identity}\0{endpoint}");
        Self::derive(
            protocol,
            &connection_and_endpoint,
            model_identity,
            model_string,
        )
    }

    pub fn derive(
        protocol: WireProtocol,
        endpoint_or_connection_identity: &str,
        model_identity: &str,
        model_string: &str,
    ) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"lrdm-source-context-v1\0");
        for component in [
            protocol.as_str(),
            endpoint_or_connection_identity,
            model_identity,
            model_string,
        ] {
            digest.update((component.len() as u64).to_be_bytes());
            digest.update(component.as_bytes());
        }
        Self(format!("v1:{:x}", digest.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Codec determining how an adapter validates and reinserts an opaque payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayCodec {
    OpenAiReasoningContent,
    AnthropicContentBlocks,
    /// Reserved until the Responses adapter implements symmetric replay.
    OpenAiResponsesItems,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderReplayEnvelope {
    pub schema_version: u16,
    pub codec: ReplayCodec,
    pub source_context_key: SourceContextKey,
    /// Provider material preserved without projecting it into public DTOs,
    /// content-safety input, or logs.
    pub payload: Value,
}

impl std::fmt::Debug for ProviderReplayEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderReplayEnvelope")
            .field("schema_version", &self.schema_version)
            .field("codec", &self.codec)
            .field("source_context_key", &self.source_context_key)
            .field("payload_bytes", &self.encoded_cost())
            .field("payload", &"<redacted>")
            .finish()
    }
}

impl ProviderReplayEnvelope {
    pub fn new(codec: ReplayCodec, source_context_key: SourceContextKey, payload: Value) -> Self {
        Self {
            schema_version: PROVIDER_REPLAY_SCHEMA_VERSION,
            codec,
            source_context_key,
            payload,
        }
    }

    pub fn validate(&self) -> Result<(), ReplayError> {
        if self.schema_version != PROVIDER_REPLAY_SCHEMA_VERSION {
            return Err(ReplayError::UnsupportedSchemaVersion(self.schema_version));
        }
        if self.codec == ReplayCodec::OpenAiResponsesItems {
            return Err(ReplayError::UnsupportedCodec(self.codec));
        }
        match self.codec {
            ReplayCodec::OpenAiReasoningContent if !self.payload.is_string() => Err(
                ReplayError::InvalidPayload("OpenAI reasoning_content replay must be a string"),
            ),
            ReplayCodec::AnthropicContentBlocks if !self.payload.is_array() => Err(
                ReplayError::InvalidPayload("Anthropic replay must be a content-block array"),
            ),
            _ => Ok(()),
        }
    }

    pub fn encoded_cost(&self) -> usize {
        serde_json::to_vec(self).map_or(0, |bytes| bytes.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayUnavailableReason {
    LegacyUnknown,
    EvictedByStorageLimit,
    InvalidPayload,
    UnsupportedCodec,
}

/// Replay decision frozen when an assistant tool-call response is parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplayDisposition {
    NotRequired {
        source_context_key: SourceContextKey,
    },
    Present {
        envelope: ProviderReplayEnvelope,
    },
    Unavailable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_context_key: Option<SourceContextKey>,
        reason: ReplayUnavailableReason,
    },
}

impl ReplayDisposition {
    pub fn legacy_unknown() -> Self {
        Self::Unavailable {
            source_context_key: None,
            reason: ReplayUnavailableReason::LegacyUnknown,
        }
    }

    pub fn source_context_key(&self) -> Option<&SourceContextKey> {
        match self {
            Self::NotRequired { source_context_key } => Some(source_context_key),
            Self::Present { envelope } => Some(&envelope.source_context_key),
            Self::Unavailable {
                source_context_key, ..
            } => source_context_key.as_ref(),
        }
    }

    pub fn model_context_cost(&self) -> usize {
        match self {
            Self::Present { envelope } => envelope.encoded_cost(),
            _ => 0,
        }
    }
}

/// Provider metadata normalized by stream scanners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderResponseMeta {
    #[serde(default)]
    pub reasoning_observed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<ReplayDisposition>,
}

impl Default for ProviderResponseMeta {
    fn default() -> Self {
        Self {
            reasoning_observed: false,
            reasoning_tokens: None,
            stop_reason: StopReason::Other,
            replay: None,
        }
    }
}

impl ProviderResponseMeta {
    pub fn without_reasoning(stop_reason: StopReason) -> Self {
        Self {
            stop_reason,
            ..Self::default()
        }
    }

    pub fn validate_for_tool_calls(&self, has_tool_calls: bool) -> Result<(), ReplayError> {
        if has_tool_calls != self.replay.is_some() {
            return Err(ReplayError::MissingOrUnexpectedDisposition);
        }
        if let Some(ReplayDisposition::Present { envelope }) = &self.replay {
            envelope.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    UnsupportedSchemaVersion(u16),
    UnsupportedCodec(ReplayCodec),
    InvalidPayload(&'static str),
    MissingOrUnexpectedDisposition,
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(version) => {
                write!(f, "unsupported provider replay schema version: {version}")
            }
            Self::UnsupportedCodec(codec) => write!(f, "unsupported replay codec: {codec:?}"),
            Self::InvalidPayload(detail) => write!(f, "invalid replay payload: {detail}"),
            Self::MissingOrUnexpectedDisposition => {
                f.write_str("assistant tool-call responses require exactly one replay disposition")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn source() -> SourceContextKey {
        SourceContextKey::derive(
            WireProtocol::OpenAiChatCompletions,
            "connection:7",
            "model:9",
            "custom-model",
        )
    }

    #[test]
    fn source_key_is_deterministic_and_secret_free() {
        let first = source();
        let second = source();
        assert_eq!(first, second);
        assert!(first.as_str().starts_with("v1:"));
        assert!(!first.as_str().contains("custom-model"));
    }

    #[test]
    fn envelope_debug_never_exposes_opaque_reasoning() {
        let secret_reasoning = "private chain of thought sentinel";
        let envelope = ProviderReplayEnvelope::new(
            ReplayCodec::OpenAiReasoningContent,
            source(),
            Value::String(secret_reasoning.to_string()),
        );

        let debug = format!("{envelope:?}");
        assert!(!debug.contains(secret_reasoning));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("payload_bytes"));
    }

    #[test]
    fn endpoint_source_key_is_exact_and_connection_scoped() {
        let key = |connection: &str, endpoint: &str, protocol, model: &str| {
            SourceContextKey::derive_for_endpoint(protocol, connection, endpoint, "model:9", model)
        };
        let base = key(
            "connection:7",
            "https://gateway.test/CasePath/",
            WireProtocol::OpenAiChatCompletions,
            "custom-model",
        );
        assert_eq!(
            base,
            key(
                "connection:7",
                " https://gateway.test/CasePath ",
                WireProtocol::OpenAiChatCompletions,
                "custom-model",
            )
        );
        assert_ne!(
            base,
            key(
                "connection:7",
                "https://gateway.test/casepath",
                WireProtocol::OpenAiChatCompletions,
                "custom-model",
            ),
            "case-sensitive endpoint paths must not share opaque replay",
        );
        assert_ne!(
            base,
            key(
                "connection:8",
                "https://gateway.test/CasePath",
                WireProtocol::OpenAiChatCompletions,
                "custom-model",
            )
        );
        assert_ne!(
            base,
            key(
                "connection:7",
                "https://gateway.test/CasePath",
                WireProtocol::AnthropicMessages,
                "custom-model",
            )
        );
        assert_ne!(
            base,
            key(
                "connection:7",
                "https://gateway.test/CasePath",
                WireProtocol::OpenAiChatCompletions,
                "renamed-model",
            )
        );
        assert!(!base.as_str().contains("gateway.test"));
    }

    #[test]
    fn validates_codec_payload_shape() {
        ProviderReplayEnvelope::new(
            ReplayCodec::OpenAiReasoningContent,
            source(),
            json!("opaque reasoning"),
        )
        .validate()
        .unwrap();
        assert!(
            ProviderReplayEnvelope::new(
                ReplayCodec::AnthropicContentBlocks,
                source(),
                json!({"not": "blocks"}),
            )
            .validate()
            .is_err()
        );
    }

    #[test]
    fn tool_call_metadata_requires_a_disposition() {
        let meta = ProviderResponseMeta::without_reasoning(StopReason::ToolUse);
        assert!(meta.validate_for_tool_calls(true).is_err());
        assert!(meta.validate_for_tool_calls(false).is_ok());
    }
}
