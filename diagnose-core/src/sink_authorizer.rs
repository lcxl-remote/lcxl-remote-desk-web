//! Unified, fail-closed information-flow gate for every external sink.
//!
//! Reading data never adds an allowed destination. Callers must first wrap
//! bytes in a validated [`DataEnvelope`], then pass the exact resolved sink
//! identity here. The returned projection is the only payload a transport is
//! allowed to serialize.

use std::collections::BTreeSet;

use desk_agent_protocol::data_lineage::{
    ContentRef, DataEnvelope, DestinationIdentity, Sensitivity, TransformationAuthority,
    TransformationRecord,
};
use sha2::{Digest, Sha256};

pub const MAX_SINK_ITEMS: usize = 64;
pub const MAX_SINK_BYTES: usize = 4 * 1024 * 1024;

/// Server-issued ExportData decision. It is deliberately separate from the
/// read capability and binds exact source envelope identities, destination,
/// sensitivity ceiling, expiry and byte budget. A model/provider cannot mint or
/// widen one by returning different JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportDataAuthorization {
    pub authorization_id: String,
    pub source_envelope_ids: Vec<String>,
    pub destination: DestinationIdentity,
    pub max_sensitivity: Sensitivity,
    pub expires_at_unix_ms: u64,
    pub max_bytes: u64,
}

/// Expand one exact source envelope to one exact sink under an explicit
/// ExportData authorization. No sensitivity lowering occurs here.
pub fn authorize_export(
    source: &DataEnvelope,
    exported_envelope_id: &str,
    authorization: &ExportDataAuthorization,
    now_unix_ms: u64,
) -> Result<(DataEnvelope, TransformationRecord), SinkAuthorizationError> {
    source
        .validate()
        .map_err(|error| SinkAuthorizationError::InvalidEnvelope(error.to_string()))?;
    authorization
        .destination
        .validate()
        .map_err(|error| SinkAuthorizationError::InvalidDestination(error.to_string()))?;
    if authorization.authorization_id.trim().is_empty()
        || exported_envelope_id.trim().is_empty()
        || authorization.source_envelope_ids.is_empty()
        || authorization.expires_at_unix_ms <= now_unix_ms
        || authorization.max_bytes == 0
        || authorization.max_bytes > MAX_SINK_BYTES as u64
    {
        return Err(SinkAuthorizationError::InvalidExportAuthorization);
    }
    if !authorization
        .source_envelope_ids
        .iter()
        .any(|envelope_id| envelope_id == &source.envelope_id)
    {
        return Err(SinkAuthorizationError::ExportSourceNotAuthorized);
    }
    if source.sensitivity == Sensitivity::Secret
        || source.sensitivity > authorization.max_sensitivity
    {
        return Err(SinkAuthorizationError::ExportSensitivityExceeded);
    }
    if content_size(&source.content) > authorization.max_bytes {
        return Err(SinkAuthorizationError::ByteCapExceeded);
    }
    let mut exported = source.clone();
    exported.envelope_id = exported_envelope_id.to_string();
    if !exported
        .allowed_destinations
        .contains(&authorization.destination)
    {
        exported
            .allowed_destinations
            .push(authorization.destination.clone());
        exported.allowed_destinations.sort();
    }
    exported
        .validate()
        .map_err(|error| SinkAuthorizationError::InvalidEnvelope(error.to_string()))?;
    let transformation = TransformationRecord {
        input_envelope_ids: vec![source.envelope_id.clone()],
        output_envelope_id: exported.envelope_id.clone(),
        authority: TransformationAuthority::ExplicitDeclassificationGrant {
            grant_id: authorization.authorization_id.clone(),
        },
    };
    transformation
        .validate()
        .map_err(|error| SinkAuthorizationError::InvalidExportTransformation(error.to_string()))?;
    Ok((exported, transformation))
}

#[derive(Debug, Clone)]
pub struct SinkInput<'a> {
    pub envelope: &'a DataEnvelope,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedSinkItem {
    pub envelope_id: String,
    pub digest_sha256: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkProjectionAudit {
    pub destination: DestinationIdentity,
    pub envelope_ids: Vec<String>,
    pub digests_sha256: Vec<String>,
    pub total_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedSinkProjection {
    pub items: Vec<AuthorizedSinkItem>,
    pub audit: SinkProjectionAudit,
}

pub trait SinkAuthorizer {
    fn authorize(
        &self,
        destination: &DestinationIdentity,
        inputs: &[SinkInput<'_>],
        now_unix_ms: u64,
        byte_cap: usize,
    ) -> Result<AuthorizedSinkProjection, SinkAuthorizationError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultSinkAuthorizer;

impl SinkAuthorizer for DefaultSinkAuthorizer {
    fn authorize(
        &self,
        destination: &DestinationIdentity,
        inputs: &[SinkInput<'_>],
        now_unix_ms: u64,
        byte_cap: usize,
    ) -> Result<AuthorizedSinkProjection, SinkAuthorizationError> {
        destination
            .validate()
            .map_err(|error| SinkAuthorizationError::InvalidDestination(error.to_string()))?;
        if inputs.is_empty() {
            return Err(SinkAuthorizationError::EmptyProjection);
        }
        if inputs.len() > MAX_SINK_ITEMS {
            return Err(SinkAuthorizationError::TooManyItems);
        }
        if byte_cap == 0 || byte_cap > MAX_SINK_BYTES {
            return Err(SinkAuthorizationError::InvalidByteCap);
        }

        let mut seen = BTreeSet::new();
        let mut total_bytes = 0usize;
        let mut items = Vec::with_capacity(inputs.len());
        for input in inputs {
            input
                .envelope
                .validate()
                .map_err(|error| SinkAuthorizationError::InvalidEnvelope(error.to_string()))?;
            if !seen.insert(input.envelope.envelope_id.as_str()) {
                return Err(SinkAuthorizationError::DuplicateEnvelope);
            }
            if input.envelope.sensitivity == Sensitivity::Secret && destination.is_external() {
                return Err(SinkAuthorizationError::SecretExternalEgressDenied);
            }
            if !input
                .envelope
                .allowed_destinations
                .iter()
                .any(|allowed| allowed == destination)
            {
                return Err(SinkAuthorizationError::DestinationNotAllowed);
            }
            if is_expired(input.envelope, now_unix_ms) {
                return Err(SinkAuthorizationError::ExpiredEnvelope);
            }
            let expected_size = content_size(&input.envelope.content);
            if usize::try_from(expected_size).ok() != Some(input.bytes.len()) {
                return Err(SinkAuthorizationError::ContentSizeMismatch);
            }
            let digest = hex_digest(input.bytes);
            if digest != input.envelope.digest_sha256 {
                return Err(SinkAuthorizationError::DigestMismatch);
            }
            total_bytes = total_bytes
                .checked_add(input.bytes.len())
                .ok_or(SinkAuthorizationError::ByteCapExceeded)?;
            if total_bytes > byte_cap {
                return Err(SinkAuthorizationError::ByteCapExceeded);
            }
            items.push(AuthorizedSinkItem {
                envelope_id: input.envelope.envelope_id.clone(),
                digest_sha256: digest,
                bytes: input.bytes.to_vec(),
            });
        }

        Ok(AuthorizedSinkProjection {
            audit: SinkProjectionAudit {
                destination: destination.clone(),
                envelope_ids: items.iter().map(|item| item.envelope_id.clone()).collect(),
                digests_sha256: items
                    .iter()
                    .map(|item| item.digest_sha256.clone())
                    .collect(),
                total_bytes,
            },
            items,
        })
    }
}

fn content_size(content: &ContentRef) -> u64 {
    match content {
        ContentRef::ImmutableBlob { size_bytes, .. }
        | ContentRef::EphemeralObservation { size_bytes, .. }
        | ContentRef::Artifact { size_bytes, .. } => *size_bytes,
    }
}

fn is_expired(envelope: &DataEnvelope, now_unix_ms: u64) -> bool {
    envelope
        .retention
        .expires_at_unix_ms
        .is_some_and(|expiry| expiry <= now_unix_ms)
        || match envelope.content {
            ContentRef::EphemeralObservation {
                expires_at_unix_ms, ..
            } => expires_at_unix_ms <= now_unix_ms,
            _ => false,
        }
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkAuthorizationError {
    InvalidDestination(String),
    EmptyProjection,
    TooManyItems,
    InvalidByteCap,
    InvalidEnvelope(String),
    DuplicateEnvelope,
    SecretExternalEgressDenied,
    DestinationNotAllowed,
    ExpiredEnvelope,
    ContentSizeMismatch,
    DigestMismatch,
    ByteCapExceeded,
    InvalidExportAuthorization,
    ExportSourceNotAuthorized,
    ExportSensitivityExceeded,
    InvalidExportTransformation(String),
}

impl std::fmt::Display for SinkAuthorizationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDestination(error) => write!(formatter, "invalid destination: {error}"),
            Self::EmptyProjection => formatter.write_str("sink projection is empty"),
            Self::TooManyItems => formatter.write_str("sink projection has too many items"),
            Self::InvalidByteCap => formatter.write_str("sink byte cap is invalid"),
            Self::InvalidEnvelope(error) => write!(formatter, "invalid envelope: {error}"),
            Self::DuplicateEnvelope => formatter.write_str("sink projection repeats an envelope"),
            Self::SecretExternalEgressDenied => {
                formatter.write_str("Secret data cannot enter an external sink")
            }
            Self::DestinationNotAllowed => {
                formatter.write_str("destination is not allowed by the envelope")
            }
            Self::ExpiredEnvelope => formatter.write_str("envelope is expired"),
            Self::ContentSizeMismatch => {
                formatter.write_str("content size does not match envelope")
            }
            Self::DigestMismatch => formatter.write_str("content digest does not match envelope"),
            Self::ByteCapExceeded => formatter.write_str("sink byte cap exceeded"),
            Self::InvalidExportAuthorization => {
                formatter.write_str("ExportData authorization is invalid or expired")
            }
            Self::ExportSourceNotAuthorized => {
                formatter.write_str("source envelope is not authorized for export")
            }
            Self::ExportSensitivityExceeded => {
                formatter.write_str("source sensitivity exceeds the export authorization")
            }
            Self::InvalidExportTransformation(error) => {
                write!(formatter, "invalid export transformation: {error}")
            }
        }
    }
}

impl std::error::Error for SinkAuthorizationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_policy::{ConservativeDerivation, derive_conservatively};
    use desk_agent_protocol::data_lineage::{
        DATA_ENVELOPE_SCHEMA_VERSION, DataProvenance, RetentionBoundary,
    };

    #[derive(Default)]
    struct FakeSink {
        sent: Vec<Vec<u8>>,
    }

    impl FakeSink {
        fn send(
            &mut self,
            destination: &DestinationIdentity,
            envelope: &DataEnvelope,
            bytes: &[u8],
        ) -> Result<(), SinkAuthorizationError> {
            let projection = DefaultSinkAuthorizer.authorize(
                destination,
                &[SinkInput { envelope, bytes }],
                100,
                MAX_SINK_BYTES,
            )?;
            self.sent
                .extend(projection.items.into_iter().map(|item| item.bytes));
            Ok(())
        }
    }

    fn model(id: &str) -> DestinationIdentity {
        DestinationIdentity::Model {
            connection_id: id.into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        }
    }

    fn envelope(
        id: &str,
        bytes: &[u8],
        sensitivity: Sensitivity,
        destinations: Vec<DestinationIdentity>,
    ) -> DataEnvelope {
        let digest = hex_digest(bytes);
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: id.into(),
            content: ContentRef::EphemeralObservation {
                observation_id: format!("observation-{id}"),
                size_bytes: bytes.len() as u64,
                expires_at_unix_ms: 200,
            },
            provenance: DataProvenance {
                source_provider_id: "provider".into(),
                source_tool_name: "read".into(),
                source_object_id: None,
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest,
            sensitivity,
            allowed_destinations: destinations,
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(200),
                delete_with_run: true,
            },
        }
    }

    #[test]
    fn exact_destination_digest_expiry_and_cap_are_authoritative() {
        let destination = model("gateway-a");
        let bytes = b"bounded content";
        let envelope = envelope(
            "allowed",
            bytes,
            Sensitivity::UserContent,
            vec![destination.clone()],
        );
        let projection = DefaultSinkAuthorizer
            .authorize(
                &destination,
                &[SinkInput {
                    envelope: &envelope,
                    bytes,
                }],
                100,
                bytes.len(),
            )
            .unwrap();
        assert_eq!(projection.items[0].bytes, bytes);
        assert_eq!(projection.audit.envelope_ids, vec!["allowed"]);
        assert!(matches!(
            DefaultSinkAuthorizer.authorize(
                &model("gateway-b"),
                &[SinkInput {
                    envelope: &envelope,
                    bytes,
                }],
                100,
                bytes.len(),
            ),
            Err(SinkAuthorizationError::DestinationNotAllowed)
        ));
        assert!(matches!(
            DefaultSinkAuthorizer.authorize(
                &destination,
                &[SinkInput {
                    envelope: &envelope,
                    bytes: b"tampered content",
                }],
                100,
                MAX_SINK_BYTES,
            ),
            Err(SinkAuthorizationError::ContentSizeMismatch
                | SinkAuthorizationError::DigestMismatch)
        ));
        assert!(matches!(
            DefaultSinkAuthorizer.authorize(
                &destination,
                &[SinkInput {
                    envelope: &envelope,
                    bytes,
                }],
                200,
                MAX_SINK_BYTES,
            ),
            Err(SinkAuthorizationError::ExpiredEnvelope)
        ));
    }

    #[test]
    fn read_does_not_imply_export_and_secret_never_reaches_external_sink() {
        let destination = model("gateway");
        let bytes = b"secret";
        let read_only = envelope("read", bytes, Sensitivity::Sensitive, Vec::new());
        assert!(matches!(
            DefaultSinkAuthorizer.authorize(
                &destination,
                &[SinkInput {
                    envelope: &read_only,
                    bytes,
                }],
                100,
                MAX_SINK_BYTES,
            ),
            Err(SinkAuthorizationError::DestinationNotAllowed)
        ));
        let secret = envelope(
            "secret",
            bytes,
            Sensitivity::Secret,
            vec![destination.clone()],
        );
        assert!(matches!(
            DefaultSinkAuthorizer.authorize(
                &destination,
                &[SinkInput {
                    envelope: &secret,
                    bytes,
                }],
                100,
                MAX_SINK_BYTES,
            ),
            Err(SinkAuthorizationError::SecretExternalEgressDenied)
        ));
    }

    #[test]
    fn explicit_export_authorization_is_exact_and_does_not_lower_sensitivity() {
        let destination = model("gateway");
        let bytes = b"bounded content";
        let source = envelope("source", bytes, Sensitivity::Sensitive, Vec::new());
        let authorization = ExportDataAuthorization {
            authorization_id: "export-grant".into(),
            source_envelope_ids: vec![source.envelope_id.clone()],
            destination: destination.clone(),
            max_sensitivity: Sensitivity::Sensitive,
            expires_at_unix_ms: 150,
            max_bytes: bytes.len() as u64,
        };
        let (exported, transformation) =
            authorize_export(&source, "exported", &authorization, 100).unwrap();
        assert_eq!(exported.sensitivity, Sensitivity::Sensitive);
        assert_eq!(exported.allowed_destinations, vec![destination.clone()]);
        assert_eq!(transformation.input_envelope_ids, vec!["source"]);
        assert!(
            DefaultSinkAuthorizer
                .authorize(
                    &destination,
                    &[SinkInput {
                        envelope: &exported,
                        bytes,
                    }],
                    100,
                    bytes.len(),
                )
                .is_ok()
        );

        let other = envelope("other", bytes, Sensitivity::Sensitive, Vec::new());
        assert!(matches!(
            authorize_export(&other, "denied", &authorization, 100),
            Err(SinkAuthorizationError::ExportSourceNotAuthorized)
        ));
        assert!(matches!(
            authorize_export(&source, "expired", &authorization, 150),
            Err(SinkAuthorizationError::InvalidExportAuthorization)
        ));
    }

    #[test]
    fn every_sink_kind_uses_the_same_exact_identity_gate() {
        let sinks = vec![
            DestinationIdentity::WebResearch {
                connector_id: "web".into(),
            },
            DestinationIdentity::EmailAccount {
                account_id: "email".into(),
            },
            DestinationIdentity::ChatAccount {
                account_id: "chat".into(),
            },
            DestinationIdentity::LocalArtifact {
                workspace_id: "workspace".into(),
            },
        ];
        for allowed in sinks {
            let bytes = b"data";
            let envelope = envelope(
                "item",
                bytes,
                Sensitivity::UserContent,
                vec![allowed.clone()],
            );
            assert!(
                DefaultSinkAuthorizer
                    .authorize(
                        &allowed,
                        &[SinkInput {
                            envelope: &envelope,
                            bytes,
                        }],
                        100,
                        MAX_SINK_BYTES,
                    )
                    .is_ok()
            );
        }
    }

    #[test]
    fn model_derived_query_and_draft_cannot_reach_web_email_or_chat_without_exact_export() {
        let model_destination = model("gateway");
        let source_bytes = b"sensitive selected workbook values";
        let source = envelope(
            "source",
            source_bytes,
            Sensitivity::Sensitive,
            vec![model_destination],
        );
        let derived_bytes = b"derived search query or outbound draft";
        let derived_digest = hex_digest(derived_bytes);
        let (derived, record) = derive_conservatively(
            &[source],
            ConservativeDerivation {
                output_envelope_id: "derived",
                content: ContentRef::EphemeralObservation {
                    observation_id: "derived-content".into(),
                    size_bytes: derived_bytes.len() as u64,
                    expires_at_unix_ms: 200,
                },
                digest_sha256: &derived_digest,
                source_provider_id: "model",
                source_tool_name: "derive_outbound_content",
                source_object_id: None,
            },
        )
        .unwrap();
        assert_eq!(record.input_envelope_ids, vec!["source"]);
        assert_eq!(derived.sensitivity, Sensitivity::Sensitive);

        let web = DestinationIdentity::WebResearch {
            connector_id: "web".into(),
        };
        let email = DestinationIdentity::EmailAccount {
            account_id: "email".into(),
        };
        let chat = DestinationIdentity::ChatAccount {
            account_id: "chat".into(),
        };
        for destination in [&web, &email, &chat] {
            let mut sink = FakeSink::default();
            assert_eq!(
                sink.send(destination, &derived, derived_bytes),
                Err(SinkAuthorizationError::DestinationNotAllowed)
            );
            assert!(sink.sent.is_empty());
        }

        let export = ExportDataAuthorization {
            authorization_id: "web-only-export".into(),
            source_envelope_ids: vec![derived.envelope_id.clone()],
            destination: web.clone(),
            max_sensitivity: Sensitivity::Sensitive,
            expires_at_unix_ms: 150,
            max_bytes: derived_bytes.len() as u64,
        };
        let (web_export, _) = authorize_export(&derived, "web-export", &export, 100).unwrap();
        let mut web_sink = FakeSink::default();
        web_sink.send(&web, &web_export, derived_bytes).unwrap();
        assert_eq!(web_sink.sent, vec![derived_bytes.to_vec()]);
        for destination in [&email, &chat] {
            let mut sink = FakeSink::default();
            assert_eq!(
                sink.send(destination, &web_export, derived_bytes),
                Err(SinkAuthorizationError::DestinationNotAllowed)
            );
            assert!(sink.sent.is_empty());
        }
    }
}
