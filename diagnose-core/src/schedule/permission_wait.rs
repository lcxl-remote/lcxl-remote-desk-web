//! Resolve a loop's permission pause to one durable request, never a new grant.
use crate::{
    dynamic_run::PermissionRequestState, file_scope::DirectoryConsentState,
    session::PersistedAgentSession,
};

/// Prefixes distinguish permission batches from directory consent identifiers.
/// The caller must fence the session/input and persist this reference atomically.
pub fn reference(session: &PersistedAgentSession, id: &str) -> Option<String> {
    if id.is_empty() || id.len() > 256 || id.trim() != id || id.chars().any(char::is_control) {
        return None;
    }
    let permissions = session
        .permission_requests
        .iter()
        .filter(|request| {
            request.request_id == id
                && request.input_revision == session.input_revision
                && request.validate().is_ok()
                && !matches!(
                    request.state,
                    PermissionRequestState::Replaced | PermissionRequestState::Withdrawn
                )
        })
        .count();
    let directories = session
        .file_scope
        .records()
        .iter()
        .filter(|record| record.proposal.request_id == id)
        .count();
    match (permissions, directories) {
        (1, 0) => Some(format!("permission:{id}")),
        (0, 1) => Some(format!("directory:{id}")),
        _ => None,
    }
}

/// A complete rejection closes this occurrence immediately. Partial approval
/// remains a decision for the task-specific resume path, never a blanket denial.
pub fn rejected(session: &PersistedAgentSession, stored_reference: &str) -> bool {
    let Some((namespace, id)) = stored_reference.split_once(':') else {
        return false;
    };
    if reference(session, id).as_deref() != Some(stored_reference) {
        return false;
    }
    match namespace {
        "permission" => session.permission_requests.iter().any(|request| {
            request.request_id == id && request.state == PermissionRequestState::Denied
        }),
        "directory" => session.file_scope.records().iter().any(|record| {
            record.proposal.request_id == id
                && matches!(
                    record.state,
                    DirectoryConsentState::Rejected | DirectoryConsentState::Revoked
                )
        }),
        _ => false,
    }
}

/// Close only a quiescent task pause. Callers fence and persist the session and
/// settle the original run in one transaction; this function grants no authority.
pub fn expire(
    session: &mut PersistedAgentSession,
    stored_reference: &str,
    now: &str,
) -> Option<()> {
    close(session, stored_reference, now, false)
}

/// Cancellation withdraws the pending request but does not count as a failure.
pub fn cancel(
    session: &mut PersistedAgentSession,
    stored_reference: &str,
    now: &str,
) -> Option<()> {
    close(session, stored_reference, now, true)
}

fn close(
    session: &mut PersistedAgentSession,
    stored_reference: &str,
    now: &str,
    cancelled: bool,
) -> Option<()> {
    use crate::session::{ExecutionState, TriggerOrigin, TurnState};
    let (_, id) = stored_reference.split_once(':')?;
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.turn_state != TurnState::Idle
        || session.execution_state != ExecutionState::None
        || session.terminal_error.is_some()
        || !session.pending_auto_triggers.is_empty()
        || !session.unclosed_tool_call_ids().is_empty()
        || session.handled_input_seq != session.latest_input_seq
        || reference(session, id).as_deref() != Some(stored_reference)
        || session
            .terminal_permission_request_id
            .as_deref()
            .is_some_and(|value| value != id)
    {
        return None;
    }
    // Work on a copy so failed validation cannot partially withdraw requests.
    let mut next = session.clone();
    if stored_reference.starts_with("permission:") {
        let request = next
            .permission_requests
            .iter_mut()
            .find(|request| request.request_id == id)?;
        if matches!(
            request.state,
            PermissionRequestState::Pending | PermissionRequestState::NeedsRevalidation
        ) {
            request.state = PermissionRequestState::Withdrawn;
        }
        // Keep decided requests as audit history. Expiry of the run prevents
        // their use; it must not rewrite an owner's recorded decision.
    } else {
        let record = next
            .file_scope
            .records()
            .iter()
            .find(|record| record.proposal.request_id == id)?;
        if !matches!(
            record.state,
            DirectoryConsentState::Rejected | DirectoryConsentState::Revoked
        ) {
            let subject = crate::file_scope::FileScopeSubject {
                actor_id: next.actor_id.clone(),
                device_id: next.device_id.clone(),
                conversation_id: next.conversation_id.clone(),
            };
            next.file_scope
                .revoke(&subject, next.file_scope.revision(), id)
                .ok()?;
        }
    }
    next.version = next.version.checked_add(1)?;
    next.finish_turn(
        if cancelled {
            TurnState::Cancelled
        } else {
            TurnState::Failed
        },
        now,
    );
    next.terminal_error = Some(desk_agent_protocol::AgentError {
        kind: if cancelled {
            desk_agent_protocol::AgentErrorKind::Cancelled
        } else {
            desk_agent_protocol::AgentErrorKind::Internal
        },
        message: if cancelled {
            "Scheduled task was cancelled while waiting for approval"
        } else if rejected(session, stored_reference) {
            "Scheduled task approval was denied by the owner"
        } else if chrono::DateTime::parse_from_rfc3339(now)
            .ok()
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
            .is_some_and(|time| directory_expired(session, stored_reference, time))
        {
            "Scheduled task directory reference expired while waiting for approval"
        } else {
            "Scheduled task approval exceeded the run time limit"
        }
        .into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    });
    *session = next;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_scope::{DirectoryConsentSource, DirectoryProposal, FileScopeSubject};
    use desk_agent_protocol::{
        AgentScope, ExecutionMode,
        computer_use::{ObjectKind, ObjectRef},
    };

    #[test]
    fn reference_requires_one_current_request_and_keeps_directory_namespace_separate() {
        let mut session = PersistedAgentSession::new(
            "conversation",
            "owner",
            "device",
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-09-06T00:00:00Z",
        );
        session.input_revision = 1;
        session.permission_requests.push(serde_json::from_value(serde_json::json!({
            "schema_version":1, "request_id":"request", "input_revision":1, "state":"pending",
            "created_at":"2026-09-06T00:00:00Z", "items":[{
                "item_id":"read", "provider_id":"desktop.session", "tool_name":"inspect_desktop_session",
                "expected_effect":"read_device", "resource_scope":["target:current_device"],
                "operation_scope":["observe"], "suggested_ttl_seconds":120,
                "suggested_max_uses":1, "reason":"Inspect current device"
            }]
        })).unwrap());
        assert_eq!(
            reference(&session, "request").as_deref(),
            Some("permission:request")
        );
        session.permission_requests[0].state = PermissionRequestState::Approved;
        assert!(
            reference(&session, "request").is_some(),
            "decision may race the pause transaction"
        );
        session.permission_requests[0].state = PermissionRequestState::Withdrawn;
        assert!(reference(&session, "request").is_none());
        session.permission_requests[0].state = PermissionRequestState::Pending;
        session.input_revision = 2;
        assert!(reference(&session, "request").is_none());
        session.input_revision = 1;
        let subject = FileScopeSubject {
            actor_id: "owner".into(),
            device_id: "device".into(),
            conversation_id: "conversation".into(),
        };
        session
            .file_scope
            .propose(
                &subject,
                0,
                DirectoryProposal {
                    request_id: "directory".into(),
                    requested_path: "/tmp/output".into(),
                    canonical_path: "/tmp/output".into(),
                    directory: ObjectRef {
                        token: "opaque".into(),
                        snapshot_id: "snapshot".into(),
                        object_kind: ObjectKind::Directory,
                        expires_at: "2099-01-01T00:00:00Z".into(),
                    },
                    purpose: "Save report".into(),
                    source: DirectoryConsentSource::ModelProposal,
                },
                0,
            )
            .unwrap();
        assert_eq!(
            reference(&session, "directory").as_deref(),
            Some("directory:directory")
        );
        session.permission_requests[0].request_id = "directory".into();
        assert!(
            reference(&session, "directory").is_none(),
            "ambiguous namespaces cannot pick authority"
        );
        assert!(reference(&session, "directory\n").is_none());
        assert!(reference(&session, "missing").is_none());
    }
}

/// Recognize the exact pause checkpoint saved atomically with a permission event,
/// before the outer loop persisted Idle. The store must still verify that original
/// event; model prose or a request list alone is not a recovery receipt.
pub fn unfinished_pause(session: &PersistedAgentSession) -> Option<String> {
    use crate::{
        chat::ChatRole,
        session::{ExecutionState, TriggerOrigin, TurnState},
    };
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || !matches!(
            session.turn_state,
            TurnState::Running | TurnState::AwaitingApproval
        )
        || session.execution_state != ExecutionState::None
        || session.terminal_error.is_some()
        || session.terminal_permission_request_id.is_some()
        || !session.pending_auto_triggers.is_empty()
        || !session.unclosed_tool_call_ids().is_empty()
    {
        return None;
    }
    let (index, proposal) = session
        .conversation
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| message.role == ChatRole::Assistant)?;
    if proposal.turn_id != session.current_turn_id {
        return None;
    }
    let tail = &session.conversation[index + 1..];
    if tail.is_empty() || tail.iter().any(|message| message.role != ChatRole::Tool) {
        return None;
    }
    let mut matched = None;
    for call in &proposal.tool_calls {
        if call.name == crate::directory_tools::REQUEST_DIRECTORY {
            let input = crate::directory_tools::parse(&crate::chat::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            })
            .ok()?;
            use sha2::{Digest, Sha256};
            let expected_id = format!(
                "directory-proposal-{:x}",
                Sha256::digest(
                    format!(
                        "{}:{}:{}",
                        session.conversation_id, session.input_revision, call.id
                    )
                    .as_bytes()
                )
            );
            for record in session.file_scope.records() {
                if record.proposal.request_id != expected_id
                    || record.proposal.source
                        != crate::file_scope::DirectoryConsentSource::ModelProposal
                    || record.proposal.requested_path != input.path
                    || record.proposal.purpose != input.purpose
                {
                    continue;
                }
                let id = &record.proposal.request_id;
                if reference(session, id).as_deref() != Some(format!("directory:{id}").as_str()) {
                    continue;
                }
                let expected = crate::directory_tools::pending_result(id);
                if tail.iter().any(|message| {
                    message.tool_call_id.as_deref() == Some(call.id.as_str())
                        && serde_json::from_str::<serde_json::Value>(&message.text)
                            .ok()
                            .as_ref()
                            == Some(&expected)
                }) {
                    if matched.is_some() {
                        return None;
                    }
                    matched = Some(id.clone());
                }
            }
            continue;
        }
        let explicit = call.name == crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME;
        for request in &session.permission_requests {
            if request.input_revision != session.input_revision
                || matches!(
                    request.state,
                    PermissionRequestState::Replaced | PermissionRequestState::Withdrawn
                )
                || request.validate().is_err()
            {
                continue;
            }
            let mut expected = serde_json::json!({"status":"pending_user_decision", "request_id":request.request_id,
                "item_count":request.items.len(), "authority":"none"});
            if !explicit {
                let original = crate::chat::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments_json: call.arguments_json.clone(),
                };
                if request.items.len() != 1
                    || request.items[0].item_id != call.id
                    || request.items[0].tool_name != call.name
                    || request.items[0].suggested_max_uses != 1
                    || !super::contract::exception::unique_original_call(
                        &session.conversation,
                        &original,
                    )
                {
                    continue;
                }
                let Some(canonical) =
                    serde_json::from_str(&call.arguments_json)
                        .ok()
                        .and_then(|value| {
                            crate::permission_tools::canonical_tool_permission_input_json(
                                &call.name, value,
                            )
                            .ok()
                        })
                else {
                    continue;
                };
                if request.items[0].canonical_input_json.as_deref() != Some(canonical.as_str()) {
                    continue;
                }
                expected["executed"] = serde_json::Value::Bool(false);
            }
            if tail.iter().any(|message| {
                message.tool_call_id.as_deref() == Some(call.id.as_str())
                    && serde_json::from_str::<serde_json::Value>(&message.text)
                        .ok()
                        .as_ref()
                        == Some(&expected)
            }) {
                if matched.is_some() {
                    return None;
                }
                matched = Some(request.request_id.clone());
            }
        }
    }
    matched
}

/// Check the exact decision bound to a task wait. Directory consent is only a
/// file-scope prerequisite; this predicate grants no capability or dispatch right.
pub fn approved(session: &PersistedAgentSession, stored_reference: &str, now_unix_ms: u64) -> bool {
    let Some((namespace, id)) = stored_reference.split_once(':') else {
        return false;
    };
    if reference(session, id).as_deref() != Some(stored_reference) {
        return false;
    }
    match namespace {
        "permission" => session.permission_requests.iter().any(|request| {
            request.request_id == id
                && request.input_revision == session.input_revision
                && matches!(
                    request.state,
                    PermissionRequestState::Approved | PermissionRequestState::PartiallyApproved
                )
        }),
        "directory" => {
            let subject = crate::file_scope::FileScopeSubject {
                actor_id: session.actor_id.clone(),
                device_id: session.device_id.clone(),
                conversation_id: session.conversation_id.clone(),
            };
            session
                .file_scope
                .approved_directory(&subject, session.file_scope.revision(), id, now_unix_ms)
                .is_ok()
        }
        _ => false,
    }
}

/// A directory reference cannot be refreshed from a saved owner decision. End
/// its wait when the original device-issued reference reaches its expiry.
pub fn directory_expired(
    session: &PersistedAgentSession,
    stored_reference: &str,
    now_unix_ms: u64,
) -> bool {
    let Some(id) = stored_reference.strip_prefix("directory:") else {
        return false;
    };
    if reference(session, id).as_deref() != Some(stored_reference) {
        return false;
    }
    session
        .file_scope
        .records()
        .iter()
        .find(|record| record.proposal.request_id == id)
        .is_some_and(|record| {
            chrono::DateTime::parse_from_rfc3339(&record.proposal.directory.expires_at)
                .ok()
                .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
                .is_none_or(|expiry| expiry <= now_unix_ms)
        })
}
