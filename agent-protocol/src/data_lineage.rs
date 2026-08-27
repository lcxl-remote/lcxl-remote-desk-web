//! Provider-neutral data lineage and sink-destination wire contracts.
//!
//! Content that may reach a model or another sink must travel through these
//! types.  A plain string or byte buffer has no authority to cross a sink
//! boundary.

use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const DATA_ENVELOPE_SCHEMA_VERSION: u16 = 1;
pub const MAX_LINEAGE_ID_BYTES: usize = 256;
pub const MAX_LINEAGE_ITEMS: usize = 256;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    UserContent,
    Sensitive,
    Secret,
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DestinationIdentity {
    Model {
        connection_id: String,
        connection_revision: u64,
        model_id: String,
        profile_revision: i64,
    },
    WebResearch {
        connector_id: String,
    },
    EmailAccount {
        account_id: String,
    },
    ChatAccount {
        account_id: String,
    },
    LocalArtifact {
        workspace_id: String,
    },
}

impl DestinationIdentity {
    pub fn validate(&self) -> Result<(), DataLineageError> {
        match self {
            Self::Model {
                connection_id,
                connection_revision,
                model_id,
                profile_revision,
            } => {
                validate_id("connection_id", connection_id)?;
                validate_id("model_id", model_id)?;
                if *connection_revision == 0 {
                    return Err(DataLineageError::InvalidRevision("connection_revision"));
                }
                if *profile_revision < 1 {
                    return Err(DataLineageError::InvalidRevision("profile_revision"));
                }
            }
            Self::WebResearch { connector_id } => validate_id("connector_id", connector_id)?,
            Self::EmailAccount { account_id } | Self::ChatAccount { account_id } => {
                validate_id("account_id", account_id)?
            }
            Self::LocalArtifact { workspace_id } => validate_id("workspace_id", workspace_id)?,
        }
        Ok(())
    }

    pub fn is_external(&self) -> bool {
        !matches!(self, Self::LocalArtifact { .. })
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentRef {
    ImmutableBlob {
        blob_id: String,
        sha256: String,
        size_bytes: u64,
        media_type: String,
    },
    EphemeralObservation {
        observation_id: String,
        size_bytes: u64,
        expires_at_unix_ms: u64,
    },
    Artifact {
        artifact_id: String,
        sha256: String,
        size_bytes: u64,
        media_type: String,
    },
}

impl ContentRef {
    pub fn validate(&self) -> Result<(), DataLineageError> {
        match self {
            Self::ImmutableBlob {
                blob_id,
                sha256,
                size_bytes,
                media_type,
            } => {
                validate_id("blob_id", blob_id)?;
                validate_sha256(sha256)?;
                validate_size(*size_bytes)?;
                validate_id("media_type", media_type)?;
            }
            Self::EphemeralObservation {
                observation_id,
                size_bytes,
                expires_at_unix_ms,
            } => {
                validate_id("observation_id", observation_id)?;
                validate_size(*size_bytes)?;
                if *expires_at_unix_ms == 0 {
                    return Err(DataLineageError::InvalidExpiry);
                }
            }
            Self::Artifact {
                artifact_id,
                sha256,
                size_bytes,
                media_type,
            } => {
                validate_id("artifact_id", artifact_id)?;
                validate_sha256(sha256)?;
                validate_size(*size_bytes)?;
                validate_id("media_type", media_type)?;
            }
        }
        Ok(())
    }

    pub fn content_sha256(&self) -> Option<&str> {
        match self {
            Self::ImmutableBlob { sha256, .. } | Self::Artifact { sha256, .. } => Some(sha256),
            Self::EphemeralObservation { .. } => None,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct RetentionBoundary {
    /// `None` means the artifact is retained until explicitly deleted. Derived
    /// data may never turn a finite input expiry into `None`.
    pub expires_at_unix_ms: Option<u64>,
    /// Ephemeral run data is deleted with the run even if its absolute expiry is
    /// later.
    pub delete_with_run: bool,
}

impl RetentionBoundary {
    pub fn validate(&self) -> Result<(), DataLineageError> {
        if self.expires_at_unix_ms == Some(0) {
            Err(DataLineageError::InvalidExpiry)
        } else {
            Ok(())
        }
    }

    pub fn most_restrictive(self, other: Self) -> Self {
        let expires_at_unix_ms = match (self.expires_at_unix_ms, other.expires_at_unix_ms) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        };
        Self {
            expires_at_unix_ms,
            delete_with_run: self.delete_with_run || other.delete_with_run,
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DataProvenance {
    pub source_provider_id: String,
    pub source_tool_name: String,
    pub source_object_id: Option<String>,
    pub source_envelope_ids: Vec<String>,
}

impl DataProvenance {
    pub fn validate(&self) -> Result<(), DataLineageError> {
        validate_id("source_provider_id", &self.source_provider_id)?;
        validate_id("source_tool_name", &self.source_tool_name)?;
        if let Some(value) = &self.source_object_id {
            validate_id("source_object_id", value)?;
        }
        validate_unique_ids("source_envelope_ids", &self.source_envelope_ids)
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DataEnvelope {
    pub schema_version: u16,
    pub envelope_id: String,
    pub content: ContentRef,
    pub provenance: DataProvenance,
    pub digest_sha256: String,
    pub sensitivity: Sensitivity,
    pub allowed_destinations: Vec<DestinationIdentity>,
    pub retention: RetentionBoundary,
}

impl DataEnvelope {
    pub fn validate(&self) -> Result<(), DataLineageError> {
        if self.schema_version != DATA_ENVELOPE_SCHEMA_VERSION {
            return Err(DataLineageError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        validate_id("envelope_id", &self.envelope_id)?;
        self.content.validate()?;
        self.provenance.validate()?;
        validate_sha256(&self.digest_sha256)?;
        if let Some(content_digest) = self.content.content_sha256()
            && content_digest != self.digest_sha256
        {
            return Err(DataLineageError::DigestMismatch);
        }
        if self.allowed_destinations.len() > MAX_LINEAGE_ITEMS {
            return Err(DataLineageError::TooManyItems("allowed_destinations"));
        }
        let mut seen = BTreeSet::new();
        for destination in &self.allowed_destinations {
            destination.validate()?;
            if !seen.insert(destination) {
                return Err(DataLineageError::DuplicateItem("allowed_destinations"));
            }
        }
        self.retention.validate()
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransformationAuthority {
    Conservative,
    DeterministicProjector {
        projector_id: String,
        projector_version: u16,
    },
    ExplicitDeclassificationGrant {
        grant_id: String,
    },
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct TransformationRecord {
    pub input_envelope_ids: Vec<String>,
    pub output_envelope_id: String,
    pub authority: TransformationAuthority,
}

impl TransformationRecord {
    pub fn validate(&self) -> Result<(), DataLineageError> {
        validate_unique_ids("input_envelope_ids", &self.input_envelope_ids)?;
        if self.input_envelope_ids.is_empty() {
            return Err(DataLineageError::MissingItem("input_envelope_ids"));
        }
        validate_id("output_envelope_id", &self.output_envelope_id)?;
        match &self.authority {
            TransformationAuthority::Conservative => {}
            TransformationAuthority::DeterministicProjector {
                projector_id,
                projector_version,
            } => {
                validate_id("projector_id", projector_id)?;
                if *projector_version == 0 {
                    return Err(DataLineageError::InvalidRevision("projector_version"));
                }
            }
            TransformationAuthority::ExplicitDeclassificationGrant { grant_id } => {
                validate_id("grant_id", grant_id)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataLineageError {
    UnsupportedSchemaVersion(u16),
    EmptyField(&'static str),
    OversizedField(&'static str),
    InvalidRevision(&'static str),
    InvalidDigest,
    DigestMismatch,
    InvalidSize,
    InvalidExpiry,
    TooManyItems(&'static str),
    DuplicateItem(&'static str),
    MissingItem(&'static str),
}

impl fmt::Display for DataLineageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(value) => {
                write!(f, "unsupported data envelope schema version: {value}")
            }
            Self::EmptyField(field) => write!(f, "{field} must not be empty"),
            Self::OversizedField(field) => write!(f, "{field} is too long"),
            Self::InvalidRevision(field) => write!(f, "{field} has an invalid revision"),
            Self::InvalidDigest => f.write_str("digest must be lowercase hexadecimal sha256"),
            Self::DigestMismatch => f.write_str("content and envelope digests do not match"),
            Self::InvalidSize => f.write_str("content size must be non-zero"),
            Self::InvalidExpiry => f.write_str("expiry must be non-zero"),
            Self::TooManyItems(field) => write!(f, "{field} has too many items"),
            Self::DuplicateItem(field) => write!(f, "{field} contains a duplicate"),
            Self::MissingItem(field) => write!(f, "{field} must contain an item"),
        }
    }
}

impl std::error::Error for DataLineageError {}

fn validate_id(field: &'static str, value: &str) -> Result<(), DataLineageError> {
    let value = value.trim();
    if value.is_empty() {
        Err(DataLineageError::EmptyField(field))
    } else if value.len() > MAX_LINEAGE_ID_BYTES {
        Err(DataLineageError::OversizedField(field))
    } else {
        Ok(())
    }
}

fn validate_unique_ids(field: &'static str, values: &[String]) -> Result<(), DataLineageError> {
    if values.len() > MAX_LINEAGE_ITEMS {
        return Err(DataLineageError::TooManyItems(field));
    }
    let mut seen = BTreeSet::new();
    for value in values {
        validate_id(field, value)?;
        if !seen.insert(value) {
            return Err(DataLineageError::DuplicateItem(field));
        }
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), DataLineageError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(DataLineageError::InvalidDigest)
    }
}

fn validate_size(size_bytes: u64) -> Result<(), DataLineageError> {
    if size_bytes == 0 {
        Err(DataLineageError::InvalidSize)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        "a".repeat(64)
    }

    #[test]
    fn model_destination_requires_stable_revisions() {
        let destination = DestinationIdentity::Model {
            connection_id: "connection".into(),
            connection_revision: 0,
            model_id: "model".into(),
            profile_revision: 1,
        };
        assert_eq!(
            destination.validate(),
            Err(DataLineageError::InvalidRevision("connection_revision"))
        );
    }

    #[test]
    fn envelope_rejects_duplicate_destinations_and_digest_mismatch() {
        let destination = DestinationIdentity::LocalArtifact {
            workspace_id: "workspace".into(),
        };
        let mut envelope = DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "envelope".into(),
            content: ContentRef::Artifact {
                artifact_id: "artifact".into(),
                sha256: digest(),
                size_bytes: 1,
                media_type: "application/octet-stream".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "provider".into(),
                source_tool_name: "tool".into(),
                source_object_id: None,
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest(),
            sensitivity: Sensitivity::UserContent,
            allowed_destinations: vec![destination.clone(), destination],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: false,
            },
        };
        assert_eq!(
            envelope.validate(),
            Err(DataLineageError::DuplicateItem("allowed_destinations"))
        );
        envelope.allowed_destinations.pop();
        envelope.digest_sha256 = "b".repeat(64);
        assert_eq!(envelope.validate(), Err(DataLineageError::DigestMismatch));
    }

    #[test]
    fn retention_join_never_lengthens_a_finite_input() {
        let persistent = RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: false,
        };
        let ephemeral = RetentionBoundary {
            expires_at_unix_ms: Some(42),
            delete_with_run: true,
        };
        assert_eq!(persistent.most_restrictive(ephemeral), ephemeral);
    }
}
