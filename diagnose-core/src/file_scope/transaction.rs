//! Storage-neutral directory-consent transactions and immutable replay checks.
//!
//! The host supplies a freshly loaded, owner-authenticated session and commits
//! both the returned state and receipt atomically. Replaying receipts does not
//! resolve a device reference again or mutate the current scope.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::*;
use crate::dynamic_run::{AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind};
use crate::session::PersistedAgentSession;

pub const FILE_SCOPE_EVENT_KIND: &str = "file_scope_updated";

pub fn receipt_event_id(subject: &FileScopeSubject, client_request_id: &str) -> String {
    let bytes = serde_json::to_vec(&(subject, client_request_id))
        .expect("directory update identity is serializable");
    format!("file-scope-{:x}", Sha256::digest(bytes))
}

pub fn owner_selection(
    update: &desk_agent_protocol::device_assistant::DeviceAssistantObjectContextUpdate,
    subject: FileScopeSubject,
    resolved: desk_agent_protocol::computer_use::FileDirectoryResolveOutput,
) -> Result<FileScopeUpdate, FileScopeError> {
    use desk_agent_protocol::device_assistant::DeviceAssistantObjectContextOperation;
    update
        .validate()
        .map_err(|_| FileScopeError::InvalidProposal)?;
    let DeviceAssistantObjectContextOperation::SelectDirectory {
        path,
        purpose,
        expected_revision,
    } = &update.operation
    else {
        return Err(FileScopeError::InvalidProposal);
    };
    let exact_path = *path == resolved.canonical_path;
    let proposal = DirectoryProposal {
        request_id: update.client_request_id.clone(),
        requested_path: path.clone(),
        canonical_path: resolved.canonical_path,
        directory: resolved.directory,
        purpose: purpose.clone(),
        source: DirectoryConsentSource::OwnerSelection,
    };
    Ok(FileScopeUpdate {
        subject,
        client_conversation_id: update.conversation_id.clone(),
        client_request_id: update.client_request_id.clone(),
        expected_revision: *expected_revision,
        mutation: if exact_path {
            FileScopeMutation::Select { proposal }
        } else {
            FileScopeMutation::Propose { proposal }
        },
    })
}

/// Match the original human intent before returning a resolution retry receipt.
pub fn match_owner_selection(
    receipt: &FileScopeReceipt,
    update: &desk_agent_protocol::device_assistant::DeviceAssistantObjectContextUpdate,
    subject: &FileScopeSubject,
) -> Result<(), FileScopeError> {
    use desk_agent_protocol::device_assistant::DeviceAssistantObjectContextOperation;
    let DeviceAssistantObjectContextOperation::SelectDirectory {
        path,
        purpose,
        expected_revision,
    } = &update.operation
    else {
        return Err(FileScopeError::RequestConflict);
    };
    let proposal = match &receipt.update.mutation {
        FileScopeMutation::Select { proposal } | FileScopeMutation::Propose { proposal } => {
            proposal
        }
        _ => return Err(FileScopeError::RequestConflict),
    };
    if receipt.update.subject != *subject
        || receipt.update.client_request_id != update.client_request_id
        || receipt.update.client_conversation_id != update.conversation_id
        || receipt.update.expected_revision != *expected_revision
        || proposal.source != DirectoryConsentSource::OwnerSelection
        || proposal.requested_path != *path
        || proposal.purpose != *purpose
    {
        return Err(FileScopeError::RequestConflict);
    }
    Ok(())
}

/// Bind an owner transport decision to the server-derived storage subject.
pub fn from_owner_decision(
    update: &desk_agent_protocol::device_assistant::DeviceAssistantObjectContextUpdate,
    subject: FileScopeSubject,
) -> Option<FileScopeUpdate> {
    use desk_agent_protocol::device_assistant::DeviceAssistantObjectContextOperation::*;
    let (expected_revision, mutation) = match &update.operation {
        DecideDirectory {
            directory_request_id,
            expected_revision,
            approve,
        } => (
            *expected_revision,
            FileScopeMutation::Decide {
                directory_request_id: directory_request_id.clone(),
                approve: *approve,
            },
        ),
        RevokeDirectory {
            directory_request_id,
            expected_revision,
        } => (
            *expected_revision,
            FileScopeMutation::Revoke {
                directory_request_id: directory_request_id.clone(),
            },
        ),
        SelectDirectory { .. }
        | AttachFile { .. }
        | AttachTerminalOutput { .. }
        | AttachWindow { .. }
        | Detach { .. }
        | RefreshFile { .. } => return None,
    };
    Some(FileScopeUpdate {
        subject,
        client_conversation_id: update.conversation_id.clone(),
        client_request_id: update.client_request_id.clone(),
        expected_revision,
        mutation,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileScopeMutation {
    Propose {
        proposal: DirectoryProposal,
    },
    /// An explicit owner selection is itself consent, without a second dialog.
    Select {
        proposal: DirectoryProposal,
    },
    Decide {
        directory_request_id: String,
        approve: bool,
    },
    Revoke {
        directory_request_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileScopeUpdate {
    pub subject: FileScopeSubject,
    pub client_conversation_id: String,
    pub client_request_id: String,
    pub expected_revision: u64,
    pub mutation: FileScopeMutation,
}

/// Only a history acknowledgement; never sufficient to authorize execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileScopeReceipt {
    pub event: AgentRunEvent,
    pub event_id: String,
    pub event_seq: u64,
    pub scope_revision: u64,
    pub changed: bool,
    pub update: FileScopeUpdate,
}

impl FileScopeUpdate {
    pub fn validate(&self) -> Result<(), FileScopeError> {
        if !valid_id(&self.client_conversation_id) || !valid_id(&self.client_request_id) {
            return Err(FileScopeError::InvalidProposal);
        }
        SessionFileScope::default().check_subject(&self.subject)?;
        match &self.mutation {
            FileScopeMutation::Propose { proposal } | FileScopeMutation::Select { proposal } => {
                if proposal.request_id != self.client_request_id {
                    return Err(FileScopeError::RequestConflict);
                }
                let valid_source = match &self.mutation {
                    FileScopeMutation::Select { .. } => matches!(
                        proposal.source,
                        DirectoryConsentSource::OwnerSelection
                            | DirectoryConsentSource::TaskContract
                    ),
                    _ => proposal.source != DirectoryConsentSource::TaskContract,
                };
                if !valid_source {
                    return Err(FileScopeError::InvalidProposal);
                }
            }
            FileScopeMutation::Decide {
                directory_request_id,
                ..
            }
            | FileScopeMutation::Revoke {
                directory_request_id,
            } => {
                if !valid_id(directory_request_id) {
                    return Err(FileScopeError::InvalidProposal);
                }
            }
        }
        Ok(())
    }

    pub fn event_id(&self) -> String {
        receipt_event_id(&self.subject, &self.client_request_id)
    }

    pub fn fingerprint(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("directory update is serializable");
        format!("{:x}", Sha256::digest(bytes))
    }

    pub fn validate_session(&self, session: &PersistedAgentSession) -> Result<(), FileScopeError> {
        self.validate()?;
        session.file_scope_subject(
            &self.subject.actor_id,
            &self.subject.device_id,
            &self.subject.conversation_id,
        )?;
        if session.client_conversation_id.as_deref() != Some(self.client_conversation_id.as_str()) {
            return Err(FileScopeError::WrongSubject);
        }
        Ok(())
    }
}

/// Preparation is copy-on-write: failures cannot partly approve a selection.
pub fn prepare(
    session: &PersistedAgentSession,
    update: &FileScopeUpdate,
    now_unix_ms: u64,
) -> Result<(PersistedAgentSession, FileScopeReceipt), FileScopeError> {
    update.validate_session(session)?;
    if matches!(&update.mutation, FileScopeMutation::Select { proposal }
        if proposal.source == DirectoryConsentSource::TaskContract)
        && (session.trigger_origin != crate::session::TriggerOrigin::ScheduledTask
            || !session.turn_state.is_active())
    {
        return Err(FileScopeError::InvalidProposal);
    }
    let mut next = session.clone();
    // The immutable ledger must have been checked by the caller first. A
    // surviving proposal with no receipt is corruption, not a fresh operation.
    if matches!(
        update.mutation,
        FileScopeMutation::Propose { .. } | FileScopeMutation::Select { .. }
    ) && next
        .file_scope
        .records()
        .iter()
        .any(|record| record.proposal.request_id == update.client_request_id)
    {
        return Err(FileScopeError::RequestConflict);
    }
    let changed = match &update.mutation {
        FileScopeMutation::Propose { proposal } | FileScopeMutation::Select { proposal } => {
            let changed = next.file_scope.propose(
                &update.subject,
                update.expected_revision,
                proposal.clone(),
                now_unix_ms,
            )?;
            if matches!(update.mutation, FileScopeMutation::Select { .. }) {
                next.file_scope.decide(
                    &update.subject,
                    next.file_scope.revision(),
                    &proposal.request_id,
                    true,
                    now_unix_ms,
                )?;
            }
            changed
        }
        FileScopeMutation::Decide {
            directory_request_id,
            approve,
        } => next.file_scope.decide(
            &update.subject,
            update.expected_revision,
            directory_request_id,
            *approve,
            now_unix_ms,
        )?,
        FileScopeMutation::Revoke {
            directory_request_id,
        } => next.file_scope.revoke(
            &update.subject,
            update.expected_revision,
            directory_request_id,
        )?,
    };
    next.last_event_seq = next
        .last_event_seq
        .checked_add(1)
        .ok_or(FileScopeError::StaleRevision)?;
    next.version = next
        .version
        .checked_add(1)
        .ok_or(FileScopeError::StaleRevision)?;
    // Database projections use signed counters. Reject instead of truncating.
    if next.last_event_seq > i64::MAX as u64 || next.version < 0 {
        return Err(FileScopeError::StaleRevision);
    }
    let receipt = FileScopeReceipt {
        event: AgentRunEvent {
            schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
            event_id: update.event_id(),
            run_id: update.subject.conversation_id.clone(),
            event_seq: next.last_event_seq,
            input_revision: next.input_revision,
            kind: AgentRunEventKind::FileScopeUpdated,
            correlation_id: Some(update.client_request_id.clone()),
            source_envelope_ids: vec![],
            result_envelope_ids: vec![],
            created_at: chrono::DateTime::from_timestamp_millis(
                i64::try_from(now_unix_ms).map_err(|_| FileScopeError::InvalidProposal)?,
            )
            .ok_or(FileScopeError::InvalidProposal)?
            .to_rfc3339(),
        },
        event_id: update.event_id(),
        event_seq: next.last_event_seq,
        scope_revision: next.file_scope.revision(),
        changed,
        update: update.clone(),
    };
    next.file_scope.archive_terminal_records();
    Ok((next, receipt))
}

pub fn replay(
    session: &PersistedAgentSession,
    update: &FileScopeUpdate,
    receipt: &FileScopeReceipt,
) -> Result<(), FileScopeError> {
    update.validate_session(session)?;
    receipt
        .event
        .validate()
        .map_err(|_| FileScopeError::RequestConflict)?;
    if receipt.update != *update
        || receipt.event_id != update.event_id()
        || receipt.event_seq == 0
        || receipt.event_seq > session.last_event_seq
        || receipt.scope_revision > session.file_scope.revision()
        || receipt.event.event_id != receipt.event_id
        || receipt.event.event_seq != receipt.event_seq
        || receipt.event.run_id != update.subject.conversation_id
        || receipt.event.kind != AgentRunEventKind::FileScopeUpdated
        || receipt.event.correlation_id.as_deref() != Some(update.client_request_id.as_str())
        || receipt.event.input_revision > session.input_revision
        || !receipt.event.source_envelope_ids.is_empty()
        || !receipt.event.result_envelope_ids.is_empty()
    {
        return Err(FileScopeError::RequestConflict);
    }
    Ok(())
}
