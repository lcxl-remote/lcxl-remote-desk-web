//! Durable, conversation-bound directory consent, independent of tool grants.
//!
//! Hosts must authenticate the owner and validate the device-issued directory
//! before calling this reducer. Persist changes under the session store's CAS
//! transaction; this module does not provide a distributed lock or dispatch
//! authority. A scope match never grants read egress or a file mutation.

use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
use serde::{Deserialize, Serialize};

use crate::session::{AgentSessionSurface, PersistedAgentSession};

pub mod transaction;

pub fn approved_directories(
    session: &PersistedAgentSession,
    now_unix_ms: u64,
) -> Result<Vec<ObjectRef>, FileScopeError> {
    let subject = session.file_scope_subject(
        &session.actor_id,
        &session.device_id,
        &session.conversation_id,
    )?;
    Ok(session
        .file_scope
        .records()
        .iter()
        .filter_map(|record| {
            session
                .file_scope
                .approved_directory(
                    &subject,
                    session.file_scope.revision(),
                    &record.proposal.request_id,
                    now_unix_ms,
                )
                .ok()
                .cloned()
        })
        .collect())
}

pub fn requires_directory_scope(tool_name: &str) -> bool {
    crate::provider_preflight::ArtifactCallPreflight::supports(tool_name)
        || crate::provider_preflight::text_file::TextMutationPreflight::supports(tool_name)
        || matches!(
            tool_name,
            "patch_selected_numbers_copy"
                | "replace_selected_pages_copy_body"
                | "patch_selected_keynote_copy"
        )
}

/// Resolve an output selector only against durable, approved conversation roots.
/// Omission is unambiguous only when exactly one live root exists.
pub fn select_output_directory(
    session: &PersistedAgentSession,
    call: &crate::chat::ToolCall,
    now_unix_ms: u64,
) -> Result<ObjectRef, FileScopeError> {
    let subject = session.file_scope_subject(
        &session.actor_id,
        &session.device_id,
        &session.conversation_id,
    )?;
    let value: serde_json::Value =
        serde_json::from_str(&call.arguments_json).map_err(|_| FileScopeError::InvalidProposal)?;
    if let Some(id) = value.get("directory_request_id") {
        let id = id
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or(FileScopeError::InvalidProposal)?;
        return session
            .file_scope
            .approved_directory(&subject, session.file_scope.revision(), id, now_unix_ms)
            .cloned();
    }
    let mut refs = session.file_scope.records().iter().filter_map(|record| {
        session
            .file_scope
            .approved_directory(
                &subject,
                session.file_scope.revision(),
                &record.proposal.request_id,
                now_unix_ms,
            )
            .ok()
    });
    let selected = refs.next().ok_or(FileScopeError::NotApproved)?.clone();
    if refs.next().is_some() {
        return Err(FileScopeError::InvalidProposal);
    }
    Ok(selected)
}

/// Extra directory boundary for file creation, never a replacement for an exact
/// tool grant. Call against the authoritative session inside issuance/dispatch
/// transactions so a revoked root cannot survive in a cached tool selection.
pub fn validate_artifact_scope(
    session: &PersistedAgentSession,
    tool_name: &str,
    resources: &[String],
    now_unix_ms: u64,
) -> Result<(), FileScopeError> {
    let expected_resources =
        if crate::provider_preflight::ArtifactCallPreflight::supports(tool_name) {
            1
        } else if crate::provider_preflight::text_file::TextMutationPreflight::supports(tool_name)
            || matches!(
                tool_name,
                "patch_selected_numbers_copy"
                    | "replace_selected_pages_copy_body"
                    | "patch_selected_keynote_copy"
            )
        {
            // The other exact reference is the selected source file/live target.
            // Its independent preflight still owns source identity validation.
            2
        } else {
            return Ok(());
        };
    let subject = session.file_scope_subject(
        &session.actor_id,
        &session.device_id,
        &session.conversation_id,
    )?;
    if resources.len() != expected_resources {
        return Err(FileScopeError::NotApproved);
    }
    for record in session.file_scope.records() {
        if let Ok(reference) = session.file_scope.approved_directory(
            &subject,
            session.file_scope.revision(),
            &record.proposal.request_id,
            now_unix_ms,
        ) {
            let labels = crate::capability_grant::fresh_object_resource_scope(
                std::slice::from_ref(reference),
            );
            if labels.len() == 1
                && resources
                    .iter()
                    .filter(|resource| *resource == &labels[0])
                    .count()
                    == 1
            {
                return Ok(());
            }
        }
    }
    Err(FileScopeError::NotApproved)
}

pub const MAX_SESSION_DIRECTORY_RECORDS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileScopeSubject {
    pub actor_id: String,
    pub device_id: String,
    /// The trusted, subject-namespaced storage identity, not a browser tab id.
    pub conversation_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryConsentSource {
    ModelProposal,
    OwnerSelection,
    /// Fresh directory reference admitted under this run's current task contract.
    TaskContract,
}

/// Metadata returned by an authenticated device resolution, not model input.
/// The exact canonical path is shown to the owner before confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryProposal {
    pub request_id: String,
    pub requested_path: String,
    pub canonical_path: String,
    pub directory: ObjectRef,
    pub purpose: String,
    pub source: DirectoryConsentSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryConsentState {
    Pending,
    Approved,
    Rejected,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryConsent {
    pub proposal: DirectoryProposal,
    pub state: DirectoryConsentState,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFileScope {
    subject: Option<FileScopeSubject>,
    revision: u64,
    records: Vec<DirectoryConsent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileScopeError {
    WrongSubject,
    InvalidProposal,
    ExpiredReference,
    StaleRevision,
    RequestConflict,
    InvalidTransition,
    CapacityExceeded,
    NotApproved,
}

impl SessionFileScope {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn records(&self) -> &[DirectoryConsent] {
        &self.records
    }

    /// Call only in the transaction that retains the immutable operation ledger.
    /// Hosts must replay that ledger before applying an incoming request, so
    /// pruning metadata cannot make an old proposal a new authorization.
    pub fn archive_terminal_records(&mut self) {
        self.records.retain(|record| {
            matches!(
                record.state,
                DirectoryConsentState::Pending | DirectoryConsentState::Approved
            )
        });
    }

    fn check_subject(&self, subject: &FileScopeSubject) -> Result<(), FileScopeError> {
        if [
            &subject.actor_id,
            &subject.device_id,
            &subject.conversation_id,
        ]
        .iter()
        .any(|value| !valid_id(value))
            || self.subject.as_ref().is_some_and(|bound| bound != subject)
        {
            return Err(FileScopeError::WrongSubject);
        }
        Ok(())
    }

    fn next_revision(&self, expected: u64) -> Result<u64, FileScopeError> {
        if self.revision != expected {
            return Err(FileScopeError::StaleRevision);
        }
        self.revision
            .checked_add(1)
            .ok_or(FileScopeError::StaleRevision)
    }

    /// Retrying an identical proposal never resets its decision or renews it.
    pub fn propose(
        &mut self,
        subject: &FileScopeSubject,
        expected_revision: u64,
        proposal: DirectoryProposal,
        now_unix_ms: u64,
    ) -> Result<bool, FileScopeError> {
        self.check_subject(subject)?;
        validate_proposal(&proposal, now_unix_ms)?;
        if let Some(existing) = self
            .records
            .iter()
            .find(|record| record.proposal.request_id == proposal.request_id)
        {
            return if existing.proposal == proposal {
                Ok(false)
            } else {
                Err(FileScopeError::RequestConflict)
            };
        }
        let revision = self.next_revision(expected_revision)?;
        if self.records.len() >= MAX_SESSION_DIRECTORY_RECORDS {
            return Err(FileScopeError::CapacityExceeded);
        }
        self.records.push(DirectoryConsent {
            proposal,
            state: DirectoryConsentState::Pending,
        });
        self.subject = Some(subject.clone());
        self.revision = revision;
        Ok(true)
    }

    /// The owner confirms the immutable device-resolved proposal, not a path
    /// supplied again by the model. Manual selection uses this same transition.
    pub fn decide(
        &mut self,
        subject: &FileScopeSubject,
        expected_revision: u64,
        request_id: &str,
        approve: bool,
        now_unix_ms: u64,
    ) -> Result<bool, FileScopeError> {
        self.check_subject(subject)?;
        let index = self.index(request_id)?;
        let desired = if approve {
            DirectoryConsentState::Approved
        } else {
            DirectoryConsentState::Rejected
        };
        let record = &self.records[index];
        if approve {
            validate_proposal(&record.proposal, now_unix_ms)?;
        }
        if record.state == desired {
            return Ok(false);
        }
        if record.state != DirectoryConsentState::Pending {
            return Err(FileScopeError::InvalidTransition);
        }
        let revision = self.next_revision(expected_revision)?;
        self.records[index].state = desired;
        self.revision = revision;
        Ok(true)
    }

    /// Cancellation and revocation invalidate even previously frozen revisions.
    pub fn revoke(
        &mut self,
        subject: &FileScopeSubject,
        expected_revision: u64,
        request_id: &str,
    ) -> Result<bool, FileScopeError> {
        self.check_subject(subject)?;
        let index = self.index(request_id)?;
        let state = self.records[index].state;
        if state == DirectoryConsentState::Revoked {
            return Ok(false);
        }
        if state == DirectoryConsentState::Rejected {
            return Err(FileScopeError::InvalidTransition);
        }
        let revision = self.next_revision(expected_revision)?;
        self.records[index].state = DirectoryConsentState::Revoked;
        self.revision = revision;
        Ok(true)
    }

    /// Returns only the exact approved root. Child resolution must be done by
    /// the device using directory handles, never a textual path-prefix check.
    /// Callers must separately validate tool grants and the device identity.
    pub fn approved_directory(
        &self,
        subject: &FileScopeSubject,
        expected_revision: u64,
        request_id: &str,
        now_unix_ms: u64,
    ) -> Result<&ObjectRef, FileScopeError> {
        self.check_subject(subject)?;
        if self.revision != expected_revision {
            return Err(FileScopeError::StaleRevision);
        }
        let record = &self.records[self.index(request_id)?];
        if record.state != DirectoryConsentState::Approved {
            return Err(FileScopeError::NotApproved);
        }
        validate_proposal(&record.proposal, now_unix_ms)?;
        Ok(&record.proposal.directory)
    }

    fn index(&self, request_id: &str) -> Result<usize, FileScopeError> {
        self.records
            .iter()
            .position(|record| record.proposal.request_id == request_id)
            .ok_or(FileScopeError::NotApproved)
    }
}

impl PersistedAgentSession {
    /// The host must first authenticate this actor as the device owner.
    pub fn file_scope_subject(
        &self,
        actor_id: &str,
        device_id: &str,
        conversation_id: &str,
    ) -> Result<FileScopeSubject, FileScopeError> {
        if self.surface != AgentSessionSurface::DeviceAssistant
            || self.actor_id != actor_id
            || self.device_id != device_id
            || self.conversation_id != conversation_id
        {
            return Err(FileScopeError::WrongSubject);
        }
        let subject = FileScopeSubject {
            actor_id: actor_id.into(),
            device_id: device_id.into(),
            conversation_id: conversation_id.into(),
        };
        self.file_scope.check_subject(&subject)?;
        Ok(subject)
    }
}

fn valid_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

/// Shared bounds for Provider-resolved metadata; this does not approve a directory.
pub(crate) fn validate_resolved_proposal(
    proposal: &DirectoryProposal,
    now: u64,
) -> Result<(), FileScopeError> {
    validate_proposal(proposal, now)
}

fn validate_proposal(proposal: &DirectoryProposal, now: u64) -> Result<(), FileScopeError> {
    if !valid_id(&proposal.request_id)
        || !valid_id(&proposal.directory.token)
        || !valid_id(&proposal.directory.snapshot_id)
        || proposal.directory.object_kind != ObjectKind::Directory
        || proposal.canonical_path.trim().is_empty()
        || proposal.requested_path.trim().is_empty()
        || proposal.requested_path.len() > 4096
        || proposal.requested_path.chars().any(char::is_control)
        || proposal.canonical_path.len() > 4096
        || proposal.canonical_path.chars().any(char::is_control)
        || proposal.purpose.trim().is_empty()
        || proposal.purpose.len() > 2048
        || proposal.purpose.chars().any(char::is_control)
    {
        return Err(FileScopeError::InvalidProposal);
    }
    // Native path canonicalization belongs to the device, not the central OS.
    let expiry = chrono::DateTime::parse_from_rfc3339(&proposal.directory.expires_at)
        .map_err(|_| FileScopeError::InvalidProposal)?
        .timestamp_millis();
    if u64::try_from(expiry)
        .ok()
        .is_none_or(|expiry| expiry <= now)
    {
        return Err(FileScopeError::ExpiredReference);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
