//! Current-only authority contract for Capability Provider calls.
//!
//! A grant is server-issued durable state. It is not a model-supplied token and
//! does not by itself create work, reserve a use, or authorize dispatch.

use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::{
    capability_provider::{CapabilityEffect, ProductSurface},
    data_lineage::DestinationIdentity,
};

pub const CAPABILITY_GRANT_SCHEMA_VERSION: u16 = 2;
pub const MAX_GRANT_SCOPE_VALUES: usize = 128;
pub const MAX_GRANT_USES: u32 = 10_000;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityRiskTier {
    R0,
    R1,
    R2,
    R3,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityGrantIssuer {
    PolicyAuto,
    UserDecision,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityGrantUsePolicy {
    Reusable,
    OneShotExact,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityGrantLimits {
    pub max_bytes_per_call: u64,
    pub max_items_per_call: u32,
    pub max_calls: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityGrant {
    pub schema_version: u16,
    pub grant_id: String,
    pub actor_id: String,
    pub run_id: String,
    /// Focus/input epoch that issued this authority. A later user input must
    /// never inherit a grant merely because the conversation id is unchanged.
    pub input_revision: u64,
    pub surface: ProductSurface,
    pub target_device_id: String,
    pub target_session_id: Option<String>,
    pub provider_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub tool_schema_version: u16,
    pub effect: CapabilityEffect,
    pub risk_tier: CapabilityRiskTier,
    pub resource_scope: Vec<String>,
    pub operation_scope: Vec<String>,
    pub export_destinations: Vec<DestinationIdentity>,
    pub allowed_envelope_ids: Vec<String>,
    pub allowed_content_digests_sha256: Vec<String>,
    pub use_policy: CapabilityGrantUsePolicy,
    pub canonical_input_digest_sha256: Option<String>,
    pub issued_by: CapabilityGrantIssuer,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub remaining_uses: u32,
    pub limits: CapabilityGrantLimits,
    pub policy_revision: i64,
    pub readiness_revision: u64,
    pub revoked_at_unix_ms: Option<u64>,
    pub revoked_reason: Option<String>,
}

impl CapabilityGrant {
    pub fn validate(&self) -> Result<(), CapabilityGrantError> {
        if self.schema_version != CAPABILITY_GRANT_SCHEMA_VERSION {
            return Err(CapabilityGrantError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        for (field, value) in [
            ("grant_id", self.grant_id.as_str()),
            ("actor_id", self.actor_id.as_str()),
            ("run_id", self.run_id.as_str()),
            ("target_device_id", self.target_device_id.as_str()),
            ("provider_id", self.provider_id.as_str()),
            ("capability_id", self.capability_id.as_str()),
            ("tool_name", self.tool_name.as_str()),
        ] {
            validate_id(field, value)?;
        }
        if let Some(session_id) = &self.target_session_id {
            validate_id("target_session_id", session_id)?;
        }
        if self.tool_schema_version == 0
            || self.issued_at_unix_ms == 0
            || self.expires_at_unix_ms <= self.issued_at_unix_ms
            || self.input_revision == 0
            || self.policy_revision < 1
            || self.readiness_revision == 0
            || self.remaining_uses > MAX_GRANT_USES
            || self.limits.max_calls == 0
            || self.limits.max_calls > MAX_GRANT_USES
            || self.remaining_uses > self.limits.max_calls
            || self.limits.max_bytes_per_call == 0
            || self.limits.max_items_per_call == 0
        {
            return Err(CapabilityGrantError::InvalidLimitOrRevision);
        }
        validate_unique_scope("resource_scope", &self.resource_scope)?;
        validate_unique_scope("operation_scope", &self.operation_scope)?;
        validate_unique_scope("allowed_envelope_ids", &self.allowed_envelope_ids)?;
        validate_unique_digests(&self.allowed_content_digests_sha256)?;
        if self.export_destinations.len() > MAX_GRANT_SCOPE_VALUES {
            return Err(CapabilityGrantError::TooManyValues("export_destinations"));
        }
        let mut destinations = BTreeSet::new();
        for destination in &self.export_destinations {
            destination
                .validate()
                .map_err(|error| CapabilityGrantError::InvalidDestination(error.to_string()))?;
            if !destinations.insert(destination) {
                return Err(CapabilityGrantError::DuplicateValue("export_destinations"));
            }
        }
        match self.use_policy {
            CapabilityGrantUsePolicy::Reusable => {
                if let Some(digest) = &self.canonical_input_digest_sha256 {
                    validate_digest(digest)?;
                }
            }
            CapabilityGrantUsePolicy::OneShotExact => {
                if self.remaining_uses > 1 || self.limits.max_calls != 1 {
                    return Err(CapabilityGrantError::ExactGrantMustBeOneShot);
                }
                validate_digest(
                    self.canonical_input_digest_sha256
                        .as_deref()
                        .ok_or(CapabilityGrantError::ExactGrantMissingDigest)?,
                )?;
            }
        }
        match (self.revoked_at_unix_ms, self.revoked_reason.as_deref()) {
            (None, None) => {}
            (Some(revoked_at), Some(reason))
                if revoked_at >= self.issued_at_unix_ms && !reason.trim().is_empty() => {}
            _ => return Err(CapabilityGrantError::InvalidRevocation),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityGrantError {
    UnsupportedSchemaVersion(u16),
    InvalidId(&'static str),
    TooManyValues(&'static str),
    DuplicateValue(&'static str),
    InvalidDigest,
    InvalidDestination(String),
    InvalidLimitOrRevision,
    ExactGrantMissingDigest,
    ExactGrantMustBeOneShot,
    InvalidRevocation,
}

impl fmt::Display for CapabilityGrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(version) => {
                write!(formatter, "unsupported capability grant schema {version}")
            }
            Self::InvalidId(field) => write!(formatter, "invalid {field}"),
            Self::TooManyValues(field) => write!(formatter, "too many {field} values"),
            Self::DuplicateValue(field) => write!(formatter, "duplicate {field} value"),
            Self::InvalidDigest => formatter.write_str("invalid sha256 digest"),
            Self::InvalidDestination(error) => write!(formatter, "invalid destination: {error}"),
            Self::InvalidLimitOrRevision => formatter.write_str("invalid limit or revision"),
            Self::ExactGrantMissingDigest => {
                formatter.write_str("one-shot exact grant requires canonical input digest")
            }
            Self::ExactGrantMustBeOneShot => {
                formatter.write_str("exact grant must allow exactly one use")
            }
            Self::InvalidRevocation => formatter.write_str("invalid grant revocation"),
        }
    }
}

impl std::error::Error for CapabilityGrantError {}

fn validate_id(field: &'static str, value: &str) -> Result<(), CapabilityGrantError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 256 || trimmed.chars().any(char::is_control) {
        Err(CapabilityGrantError::InvalidId(field))
    } else {
        Ok(())
    }
}

fn validate_unique_scope(
    field: &'static str,
    values: &[String],
) -> Result<(), CapabilityGrantError> {
    if values.len() > MAX_GRANT_SCOPE_VALUES {
        return Err(CapabilityGrantError::TooManyValues(field));
    }
    let mut seen = BTreeSet::new();
    for value in values {
        validate_id(field, value)?;
        if !seen.insert(value) {
            return Err(CapabilityGrantError::DuplicateValue(field));
        }
    }
    Ok(())
}

fn validate_unique_digests(values: &[String]) -> Result<(), CapabilityGrantError> {
    if values.len() > MAX_GRANT_SCOPE_VALUES {
        return Err(CapabilityGrantError::TooManyValues(
            "allowed_content_digests_sha256",
        ));
    }
    let mut seen = BTreeSet::new();
    for value in values {
        validate_digest(value)?;
        if !seen.insert(value) {
            return Err(CapabilityGrantError::DuplicateValue(
                "allowed_content_digests_sha256",
            ));
        }
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), CapabilityGrantError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(CapabilityGrantError::InvalidDigest)
    }
}
