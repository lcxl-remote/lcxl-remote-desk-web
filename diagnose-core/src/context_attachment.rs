//! Durable metadata for user-selected Assistant context.
//!
//! Attachments never contain document, terminal, file, UI or screen bytes. They
//! bind an opaque, expiring object reference to a [`DataEnvelope`] and bounded
//! read scope. Refresh creates a new immutable attachment identity; stale refs
//! are never rebound to a new object incarnation.

use std::collections::BTreeSet;

use desk_agent_protocol::data_lineage::{DataEnvelope, Sensitivity};
use serde::{Deserialize, Serialize};

use crate::session::AgentSessionSurface;

pub const CONTEXT_ATTACHMENT_SCHEMA_VERSION: u16 = 1;
pub const MAX_CONTEXT_ATTACHMENTS: usize = 32;
pub const MAX_ATTACHMENT_DISPLAY_SUMMARY_BYTES: usize = 512;
pub const MAX_ATTACHMENT_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_ATTACHMENT_OBJECTS: u32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAttachmentKind {
    Device,
    InteractiveSession,
    ActiveApplication,
    UiSelection,
    OfficeDocument,
    Worksheet,
    Range,
    File,
    DirectorySelection,
    TerminalSessionRef,
    CurrentScreen,
    ExternalSourceSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentObjectRef {
    pub opaque_token: String,
    pub object_incarnation: String,
    pub source_provider_id: String,
    pub source_capability_id: String,
}

/// Current live binding reported by a Provider at attachment revalidation time.
/// It contains identity metadata only; observation bytes are never carried here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRuntimeBinding {
    pub source_provider_id: String,
    pub source_capability_id: String,
    pub object_incarnation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentBounds {
    pub max_bytes: u64,
    pub max_objects: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentStaleReason {
    Expired,
    Detached,
    WorkerRespawned,
    SessionChanged,
    DocumentChanged,
    ObjectChanged,
    CapabilityUnavailable,
    PolicyNarrowed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttachmentState {
    Active,
    Stale { reason: AttachmentStaleReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAttachment {
    pub schema_version: u16,
    pub attachment_id: String,
    pub client_request_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub surface: AgentSessionSurface,
    pub kind: ContextAttachmentKind,
    pub object_ref: AttachmentObjectRef,
    pub bounds: AttachmentBounds,
    /// Human-readable metadata only (for example filename or range address).
    /// Content bytes live behind `envelope.content`, never in session JSON.
    pub display_summary: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub envelope: DataEnvelope,
    pub state: AttachmentState,
}

impl ContextAttachment {
    pub fn validate(&self) -> Result<(), ContextAttachmentError> {
        if self.schema_version != CONTEXT_ATTACHMENT_SCHEMA_VERSION {
            return Err(ContextAttachmentError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        for (field, value) in [
            ("attachment_id", self.attachment_id.as_str()),
            ("client_request_id", self.client_request_id.as_str()),
            ("actor_id", self.actor_id.as_str()),
            ("device_id", self.device_id.as_str()),
            ("opaque_token", self.object_ref.opaque_token.as_str()),
            (
                "object_incarnation",
                self.object_ref.object_incarnation.as_str(),
            ),
            (
                "source_provider_id",
                self.object_ref.source_provider_id.as_str(),
            ),
            (
                "source_capability_id",
                self.object_ref.source_capability_id.as_str(),
            ),
        ] {
            validate_id(field, value)?;
        }
        if self.surface != AgentSessionSurface::DeviceAssistant {
            return Err(ContextAttachmentError::InvalidSurface);
        }
        if self.display_summary.len() > MAX_ATTACHMENT_DISPLAY_SUMMARY_BYTES {
            return Err(ContextAttachmentError::DisplaySummaryTooLarge);
        }
        if self.created_at_unix_ms == 0
            || self.expires_at_unix_ms <= self.created_at_unix_ms
            || self.bounds.max_bytes == 0
            || self.bounds.max_bytes > MAX_ATTACHMENT_BYTES
            || self.bounds.max_objects == 0
            || self.bounds.max_objects > MAX_ATTACHMENT_OBJECTS
        {
            return Err(ContextAttachmentError::InvalidBounds);
        }
        self.envelope
            .validate()
            .map_err(|error| ContextAttachmentError::InvalidEnvelope(error.to_string()))?;
        let content_expiry = match self.envelope.content {
            desk_agent_protocol::data_lineage::ContentRef::EphemeralObservation {
                expires_at_unix_ms,
                ..
            } => Some(expires_at_unix_ms),
            _ => None,
        };
        if content_expiry.is_some_and(|expiry| expiry < self.expires_at_unix_ms)
            || self
                .envelope
                .retention
                .expires_at_unix_ms
                .is_some_and(|expiry| expiry < self.expires_at_unix_ms)
        {
            return Err(ContextAttachmentError::AttachmentOutlivesEnvelope);
        }
        if self.kind == ContextAttachmentKind::CurrentScreen
            && self.envelope.sensitivity < Sensitivity::Sensitive
        {
            return Err(ContextAttachmentError::ScreenSensitivityTooLow);
        }
        Ok(())
    }

    pub fn is_active_at(&self, now_unix_ms: u64) -> bool {
        matches!(self.state, AttachmentState::Active)
            && now_unix_ms >= self.created_at_unix_ms
            && now_unix_ms < self.expires_at_unix_ms
    }

    pub fn mark_stale(&mut self, reason: AttachmentStaleReason) {
        self.state = AttachmentState::Stale { reason };
    }

    pub fn stale_reason_against(
        &self,
        now_unix_ms: u64,
        bindings: &[AttachmentRuntimeBinding],
    ) -> Option<AttachmentStaleReason> {
        if let AttachmentState::Stale { reason } = self.state {
            return Some(reason);
        }
        if now_unix_ms < self.created_at_unix_ms || now_unix_ms >= self.expires_at_unix_ms {
            return Some(AttachmentStaleReason::Expired);
        }
        let Some(binding) = bindings.iter().find(|binding| {
            binding.source_provider_id == self.object_ref.source_provider_id
                && binding.source_capability_id == self.object_ref.source_capability_id
        }) else {
            return Some(AttachmentStaleReason::CapabilityUnavailable);
        };
        if binding.object_incarnation != self.object_ref.object_incarnation {
            return Some(match self.kind {
                ContextAttachmentKind::Device
                | ContextAttachmentKind::InteractiveSession
                | ContextAttachmentKind::ActiveApplication
                | ContextAttachmentKind::UiSelection => AttachmentStaleReason::WorkerRespawned,
                ContextAttachmentKind::OfficeDocument
                | ContextAttachmentKind::Worksheet
                | ContextAttachmentKind::Range => AttachmentStaleReason::DocumentChanged,
                ContextAttachmentKind::TerminalSessionRef => AttachmentStaleReason::SessionChanged,
                ContextAttachmentKind::File
                | ContextAttachmentKind::DirectorySelection
                | ContextAttachmentKind::CurrentScreen
                | ContextAttachmentKind::ExternalSourceSet => AttachmentStaleReason::ObjectChanged,
            });
        }
        None
    }
}

/// Revalidate persisted live refs after page refresh/reconnect or before a new
/// turn. Stale attachments are one-way: matching a later binding never revives
/// an identity that was already detached, expired, or invalidated.
pub fn revalidate_attachments(
    attachments: &mut [ContextAttachment],
    now_unix_ms: u64,
    bindings: &[AttachmentRuntimeBinding],
) -> Vec<ContextAttachmentEvent> {
    let mut events = Vec::new();
    for attachment in attachments {
        if !matches!(attachment.state, AttachmentState::Active) {
            continue;
        }
        if let Some(reason) = attachment.stale_reason_against(now_unix_ms, bindings) {
            attachment.mark_stale(reason);
            events.push(ContextAttachmentEvent::MarkedStale {
                attachment_id: attachment.attachment_id.clone(),
                reason,
            });
        }
    }
    events
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ContextAttachmentEvent {
    Attached {
        client_request_id: String,
        attachment: ContextAttachment,
    },
    Detached {
        client_request_id: String,
        attachment_id: String,
    },
    Refreshed {
        client_request_id: String,
        stale_attachment_id: String,
        replacement: ContextAttachment,
    },
    MarkedStale {
        attachment_id: String,
        reason: AttachmentStaleReason,
    },
}

/// Validate one persisted attachment projection. Idempotency request IDs and
/// immutable attachment IDs must both be unique.
pub fn validate_attachment_set(
    attachments: &[ContextAttachment],
) -> Result<(), ContextAttachmentError> {
    if attachments.len() > MAX_CONTEXT_ATTACHMENTS {
        return Err(ContextAttachmentError::TooManyAttachments);
    }
    let mut attachment_ids = BTreeSet::new();
    let mut request_ids = BTreeSet::new();
    for attachment in attachments {
        attachment.validate()?;
        if !attachment_ids.insert(attachment.attachment_id.as_str()) {
            return Err(ContextAttachmentError::DuplicateAttachmentId);
        }
        if !request_ids.insert(attachment.client_request_id.as_str()) {
            return Err(ContextAttachmentError::DuplicateClientRequestId);
        }
    }
    Ok(())
}

pub fn validate_attachment_subject(
    attachment: &ContextAttachment,
    actor_id: &str,
    device_id: &str,
    surface: AgentSessionSurface,
) -> Result<(), ContextAttachmentError> {
    attachment.validate()?;
    if attachment.actor_id != actor_id || attachment.device_id != device_id {
        return Err(ContextAttachmentError::SubjectMismatch);
    }
    if attachment.surface != surface {
        return Err(ContextAttachmentError::SessionSurfaceMismatch);
    }
    Ok(())
}

fn validate_id(field: &'static str, value: &str) -> Result<(), ContextAttachmentError> {
    if value.trim().is_empty() || value.len() > 512 {
        Err(ContextAttachmentError::InvalidId(field))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextAttachmentError {
    UnsupportedSchemaVersion(u16),
    InvalidId(&'static str),
    InvalidSurface,
    DisplaySummaryTooLarge,
    InvalidBounds,
    InvalidEnvelope(String),
    AttachmentOutlivesEnvelope,
    ScreenSensitivityTooLow,
    TooManyAttachments,
    DuplicateAttachmentId,
    DuplicateClientRequestId,
    AttachmentNotFound,
    RefreshIdentityReused,
    SubjectMismatch,
    SessionSurfaceMismatch,
}

impl std::fmt::Display for ContextAttachmentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ContextAttachmentError {}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataProvenance, RetentionBoundary,
    };

    fn attachment(id: &str, request: &str) -> ContextAttachment {
        ContextAttachment {
            schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
            attachment_id: id.into(),
            client_request_id: request.into(),
            actor_id: "owner".into(),
            device_id: "device".into(),
            surface: AgentSessionSurface::DeviceAssistant,
            kind: ContextAttachmentKind::Range,
            object_ref: AttachmentObjectRef {
                opaque_token: format!("token-{id}"),
                object_incarnation: "workbook-1:sheet-1".into(),
                source_provider_id: "office.document".into(),
                source_capability_id: "office.document.inspect".into(),
            },
            bounds: AttachmentBounds {
                max_bytes: 64 * 1024,
                max_objects: 16,
            },
            display_summary: "Book1 / Sheet1 / A1:B2".into(),
            created_at_unix_ms: 100,
            expires_at_unix_ms: 200,
            envelope: DataEnvelope {
                schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
                envelope_id: format!("envelope-{id}"),
                content: ContentRef::EphemeralObservation {
                    observation_id: format!("observation-{id}"),
                    size_bytes: 4,
                    expires_at_unix_ms: 200,
                },
                provenance: DataProvenance {
                    source_provider_id: "office.document".into(),
                    source_tool_name: "inspect_office_document".into(),
                    source_object_id: Some("range".into()),
                    source_envelope_ids: Vec::new(),
                },
                digest_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .into(),
                sensitivity: Sensitivity::Sensitive,
                allowed_destinations: Vec::new(),
                retention: RetentionBoundary {
                    expires_at_unix_ms: Some(200),
                    delete_with_run: true,
                },
            },
            state: AttachmentState::Active,
        }
    }

    #[test]
    fn attachment_persists_metadata_and_content_reference_only() {
        let value = attachment("a", "request-a");
        value.validate().unwrap();
        let json = serde_json::to_string(&value).unwrap();
        assert!(json.contains("A1:B2"));
        assert!(!json.contains("cell value"));
        assert!(!value.is_active_at(99));
        assert!(value.is_active_at(199));
        assert!(!value.is_active_at(200));
    }

    #[test]
    fn refresh_requires_a_new_identity_and_stale_ref_never_rebinds() {
        let mut old = attachment("old", "request-old");
        old.mark_stale(AttachmentStaleReason::DocumentChanged);
        let replacement = attachment("new", "request-new");
        assert!(!old.is_active_at(150));
        assert_ne!(old.attachment_id, replacement.attachment_id);
        assert!(validate_attachment_set(&[old, replacement]).is_ok());
    }

    #[test]
    fn idempotency_and_attachment_id_collisions_fail_closed() {
        let a = attachment("a", "request-a");
        let mut duplicate_request = attachment("b", "request-a");
        assert_eq!(
            validate_attachment_set(&[a.clone(), duplicate_request.clone()]),
            Err(ContextAttachmentError::DuplicateClientRequestId)
        );
        duplicate_request.client_request_id = "request-b".into();
        duplicate_request.attachment_id = "a".into();
        assert_eq!(
            validate_attachment_set(&[a, duplicate_request]),
            Err(ContextAttachmentError::DuplicateAttachmentId)
        );
    }

    #[test]
    fn revalidation_is_one_way_and_maps_document_and_capability_changes() {
        let mut attachments = vec![attachment("range", "request-range")];
        let changed = AttachmentRuntimeBinding {
            source_provider_id: "office.document".into(),
            source_capability_id: "office.document.inspect".into(),
            object_incarnation: "workbook-2:sheet-1".into(),
        };
        let events = revalidate_attachments(&mut attachments, 150, &[changed]);
        assert_eq!(
            events,
            vec![ContextAttachmentEvent::MarkedStale {
                attachment_id: "range".into(),
                reason: AttachmentStaleReason::DocumentChanged,
            }]
        );
        assert!(!attachments[0].is_active_at(150));

        let original = AttachmentRuntimeBinding {
            source_provider_id: "office.document".into(),
            source_capability_id: "office.document.inspect".into(),
            object_incarnation: "workbook-1:sheet-1".into(),
        };
        assert!(revalidate_attachments(&mut attachments, 150, &[original]).is_empty());

        let mut unavailable = vec![attachment("unavailable", "request-unavailable")];
        let events = revalidate_attachments(&mut unavailable, 150, &[]);
        assert_eq!(
            events[0],
            ContextAttachmentEvent::MarkedStale {
                attachment_id: "unavailable".into(),
                reason: AttachmentStaleReason::CapabilityUnavailable,
            }
        );
    }

    #[test]
    fn expired_attachment_is_staled_even_if_provider_still_matches() {
        let mut attachments = vec![attachment("expired", "request-expired")];
        let binding = AttachmentRuntimeBinding {
            source_provider_id: "office.document".into(),
            source_capability_id: "office.document.inspect".into(),
            object_incarnation: "workbook-1:sheet-1".into(),
        };
        let events = revalidate_attachments(&mut attachments, 200, &[binding]);
        assert!(matches!(
            events.as_slice(),
            [ContextAttachmentEvent::MarkedStale {
                reason: AttachmentStaleReason::Expired,
                ..
            }]
        ));
    }
}
