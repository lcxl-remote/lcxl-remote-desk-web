//! Exact, fail-closed export of a permission review to the independent model.
//!
//! An owner delegation enables review, but it does not turn an arbitrary
//! assembled string into an authorized source. This
//! gate checks the original message envelopes and the persisted request before
//! returning the only prompt bytes a caller may send.

use std::collections::BTreeSet;

use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, DestinationIdentity,
    RetentionBoundary, Sensitivity,
};
use sha2::{Digest, Sha256};

use crate::approval_delegation::ApprovalDelegation;
use crate::approval_review::{
    ApprovalEvidenceTrust, ApprovalInputKind, ApprovalReviewCandidate, ApprovalReviewError,
    ApprovalSource, MAX_APPROVAL_CONTEXT_BYTES, is_historical_permission_pause_result,
    permission_item_action_json, review_user_prompt,
};
use crate::chat::ChatRole;
use crate::dynamic_run::{PermissionRequest, PermissionRequestState};
use crate::goal::GoalRun;
use crate::model_egress::message_content_bytes;
use crate::session::{AgentSessionSurface, PersistedAgentSession};
use crate::sink_authorizer::{
    DefaultSinkAuthorizer, ExportDataAuthorization, SinkAuthorizer, SinkInput, SinkProjectionAudit,
    authorize_export,
};
use desk_agent_protocol::capability_grant::CapabilityRiskTier;

#[derive(Debug, Clone)]
pub struct AuthorizedApprovalReview {
    pub prompt: String,
    /// Identities and digests of original evidence authorized for this review.
    pub source_audit: SinkProjectionAudit,
    /// The exact derived prompt authorized for the independent reviewer.
    pub prompt_audit: SinkProjectionAudit,
}

/// Call only with a candidate assembled from the current persisted permission
/// request. Missing, altered, oversized, expired or non-exportable evidence
/// returns `InvalidContext`; the caller must fail closed without dispatch.
#[allow(clippy::too_many_arguments)]
pub fn authorize_permission_review_egress(
    candidate: &ApprovalReviewCandidate,
    session: &PersistedAgentSession,
    request: &PermissionRequest,
    goal: Option<&GoalRun>,
    delegation: &ApprovalDelegation,
    destination: &DestinationIdentity,
    now_unix_ms: u64,
) -> Result<AuthorizedApprovalReview, ApprovalReviewError> {
    candidate.validate()?;
    request
        .validate()
        .map_err(|_| ApprovalReviewError::InvalidContext)?;
    if session.surface != AgentSessionSurface::AiAssistant
        || request.state != PermissionRequestState::Pending
        || !session
            .permission_requests
            .iter()
            .any(|stored| stored == request)
        || candidate.conversation_id != session.conversation_id
        || candidate.owner_id != session.actor_id
        || candidate.device_id != session.device_id
        || candidate.input_revision != session.input_revision
        || candidate.input_revision != request.input_revision
        || candidate.delegation_id != delegation.delegation_id
        || candidate.delegation_revision != delegation.revision
        || candidate.expires_at_unix_ms <= now_unix_ms
    {
        return Err(ApprovalReviewError::InvalidContext);
    }
    delegation
        .require_current(
            &session.conversation_id,
            &session.actor_id,
            &session.device_id,
        )
        .map_err(|_| ApprovalReviewError::InvalidContext)?;
    if candidate.goal_id.as_deref() != goal.map(|goal| goal.goal_id.as_str())
        || candidate.goal_revision != goal.map(|goal| goal.goal_revision)
        || candidate.context.goal_text.as_deref() != goal.map(|goal| goal.goal_text.as_str())
        || candidate.context.goal_revision != candidate.goal_revision
    {
        return Err(ApprovalReviewError::InvalidContext);
    }
    let (request_id, item_id) = match &candidate.source {
        ApprovalSource::PermissionItem {
            request_id,
            item_id,
        } => (request_id, item_id),
        _ => return Err(ApprovalReviewError::InvalidCandidate),
    };
    if request_id != &request.request_id {
        return Err(ApprovalReviewError::InvalidContext);
    }
    let item = request
        .items
        .iter()
        .find(|item| &item.item_id == item_id)
        .ok_or(ApprovalReviewError::InvalidContext)?;
    if candidate.tool_name != item.tool_name
        || candidate.action_json != permission_item_action_json(item, candidate.input_kind)?
    {
        return Err(ApprovalReviewError::InvalidContext);
    }
    let descriptor: serde_json::Value = serde_json::from_str(&candidate.descriptor_json)
        .map_err(|_| ApprovalReviewError::InvalidContext)?;
    if descriptor
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        != Some(item.tool_name.as_str())
        || descriptor
            .get("provider_id")
            .and_then(serde_json::Value::as_str)
            != Some(item.provider_id.as_str())
    {
        return Err(ApprovalReviewError::InvalidContext);
    }

    let user = crate::permission_resume::latest_user_requirement(&session.conversation)
        .ok_or(ApprovalReviewError::InvalidContext)?;
    if candidate.context.current_user_requirement != user.text {
        return Err(ApprovalReviewError::InvalidContext);
    }
    let action_source = session
        .conversation
        .iter()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message.tool_calls.iter().any(|call| {
                    call.name == "request_permissions"
                        && session.conversation.iter().any(|result| {
                            result.tool_call_id.as_deref() == Some(call.id.as_str())
                                && serde_json::from_str::<serde_json::Value>(&result.text)
                                    .ok()
                                    .and_then(|value| {
                                        value.get("request_id")?.as_str().map(str::to_owned)
                                    })
                                    .as_deref()
                                    == Some(request_id.as_str())
                        })
                })
        })
        .ok_or(ApprovalReviewError::InvalidContext)?;

    let mut source_ids = BTreeSet::new();
    source_ids.insert(user.message_id.as_str());
    source_ids.insert(action_source.message_id.as_str());
    if let Some(goal) = goal {
        let source = session
            .conversation
            .iter()
            .find(|message| message.message_id == goal.source_message_id)
            .ok_or(ApprovalReviewError::InvalidContext)?;
        if source.role != ChatRole::User || !source.text.contains(&goal.goal_text) {
            return Err(ApprovalReviewError::InvalidContext);
        }
        source_ids.insert(source.message_id.as_str());
    }
    for evidence in &candidate.context.evidence {
        let message = session
            .conversation
            .iter()
            .find(|message| message.message_id == evidence.event_id)
            .ok_or(ApprovalReviewError::InvalidContext)?;
        if is_historical_permission_pause_result(message) {
            return Err(ApprovalReviewError::InvalidContext);
        }
        let expected_trust = if message.role == ChatRole::User
            && !crate::permission_resume::is_resume_control_message(message)
        {
            ApprovalEvidenceTrust::OwnerInstruction
        } else {
            ApprovalEvidenceTrust::UntrustedContent
        };
        if message.text != evidence.text
            || evidence.trust != expected_trust
            || (expected_trust == ApprovalEvidenceTrust::UntrustedContent
                && !matches!(
                    message.role,
                    ChatRole::Tool
                        | ChatRole::UntrustedOutput
                        | ChatRole::Assistant
                        | ChatRole::SystemEvent
                ))
        {
            return Err(ApprovalReviewError::InvalidContext);
        }
        source_ids.insert(message.message_id.as_str());
    }
    if !source_ids.contains(user.message_id.as_str()) {
        return Err(ApprovalReviewError::InvalidContext);
    }

    authorize_review_sources(
        candidate,
        session,
        delegation,
        destination,
        now_unix_ms,
        source_ids,
    )
}

/// A host-rebuilt source binding. For command work and concrete UI calls the
/// caller must derive every field from the persisted work/call and held turn,
/// then compare it here before any prompt leaves the process.
pub struct SourceReviewBinding<'a> {
    pub source: &'a ApprovalSource,
    pub tool_call_id: &'a str,
    pub turn_id: &'a str,
    pub lease_token: u64,
    pub tool_name: &'a str,
    pub action_json: &'a str,
    pub descriptor_json: &'a str,
    pub risk: CapabilityRiskTier,
    pub input_kind: ApprovalInputKind,
    pub input_revision: u64,
    pub expires_at_unix_ms: u64,
}

pub fn authorize_source_review_egress(
    candidate: &ApprovalReviewCandidate,
    binding: &SourceReviewBinding<'_>,
    session: &PersistedAgentSession,
    goal: Option<&GoalRun>,
    delegation: &ApprovalDelegation,
    destination: &DestinationIdentity,
    now_unix_ms: u64,
) -> Result<AuthorizedApprovalReview, ApprovalReviewError> {
    candidate.validate()?;
    if session.surface != AgentSessionSurface::AiAssistant
        || !session.turn_state.is_active()
        || session.current_turn_id.as_deref() != Some(binding.turn_id)
        || session.lease_token != binding.lease_token
        || session.input_revision != binding.input_revision
        || candidate.source != *binding.source
        || matches!(candidate.source, ApprovalSource::PermissionItem { .. })
        || candidate.tool_name != binding.tool_name
        || candidate.action_json != binding.action_json
        || candidate.descriptor_json != binding.descriptor_json
        || candidate.risk != binding.risk
        || candidate.input_kind != binding.input_kind
        || candidate.input_revision != binding.input_revision
        || candidate.expires_at_unix_ms != binding.expires_at_unix_ms
        || candidate.conversation_id != session.conversation_id
        || candidate.owner_id != session.actor_id
        || candidate.device_id != session.device_id
        || candidate.delegation_id != delegation.delegation_id
        || candidate.delegation_revision != delegation.revision
        || candidate.expires_at_unix_ms <= now_unix_ms
    {
        return Err(ApprovalReviewError::InvalidContext);
    }
    delegation
        .require_current(
            &session.conversation_id,
            &session.actor_id,
            &session.device_id,
        )
        .map_err(|_| ApprovalReviewError::InvalidContext)?;
    if candidate.goal_id.as_deref() != goal.map(|goal| goal.goal_id.as_str())
        || candidate.goal_revision != goal.map(|goal| goal.goal_revision)
        || candidate.context.goal_text.as_deref() != goal.map(|goal| goal.goal_text.as_str())
    {
        return Err(ApprovalReviewError::InvalidContext);
    }
    let user = crate::permission_resume::latest_user_requirement(&session.conversation)
        .ok_or(ApprovalReviewError::InvalidContext)?;
    if candidate.context.current_user_requirement != user.text {
        return Err(ApprovalReviewError::InvalidContext);
    }
    let action_source = session
        .conversation
        .iter()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message.turn_id.as_deref() == Some(binding.turn_id)
                && message
                    .tool_calls
                    .iter()
                    .any(|call| call.id == binding.tool_call_id && call.name == binding.tool_name)
        })
        .ok_or(ApprovalReviewError::InvalidContext)?;
    let mut source_ids = BTreeSet::new();
    source_ids.insert(user.message_id.as_str());
    source_ids.insert(action_source.message_id.as_str());
    if let Some(goal) = goal {
        let source = session
            .conversation
            .iter()
            .find(|message| message.message_id == goal.source_message_id)
            .ok_or(ApprovalReviewError::InvalidContext)?;
        if source.role != ChatRole::User || !source.text.contains(&goal.goal_text) {
            return Err(ApprovalReviewError::InvalidContext);
        }
        source_ids.insert(source.message_id.as_str());
    }
    for evidence in &candidate.context.evidence {
        let message = session
            .conversation
            .iter()
            .find(|message| message.message_id == evidence.event_id)
            .ok_or(ApprovalReviewError::InvalidContext)?;
        if is_historical_permission_pause_result(message) {
            return Err(ApprovalReviewError::InvalidContext);
        }
        let expected_trust = if message.role == ChatRole::User
            && !crate::permission_resume::is_resume_control_message(message)
        {
            ApprovalEvidenceTrust::OwnerInstruction
        } else {
            ApprovalEvidenceTrust::UntrustedContent
        };
        if message.text != evidence.text
            || evidence.trust != expected_trust
            || (expected_trust == ApprovalEvidenceTrust::UntrustedContent
                && !matches!(
                    message.role,
                    ChatRole::Tool
                        | ChatRole::UntrustedOutput
                        | ChatRole::Assistant
                        | ChatRole::SystemEvent
                ))
        {
            return Err(ApprovalReviewError::InvalidContext);
        }
        source_ids.insert(message.message_id.as_str());
    }
    authorize_review_sources(
        candidate,
        session,
        delegation,
        destination,
        now_unix_ms,
        source_ids,
    )
}

fn authorize_review_sources(
    candidate: &ApprovalReviewCandidate,
    session: &PersistedAgentSession,
    delegation: &ApprovalDelegation,
    destination: &DestinationIdentity,
    now_unix_ms: u64,
    source_ids: BTreeSet<&str>,
) -> Result<AuthorizedApprovalReview, ApprovalReviewError> {
    let authorization = ExportDataAuthorization {
        authorization_id: delegation.owner_authorization_id.clone(),
        source_envelope_ids: Vec::new(),
        destination: destination.clone(),
        max_sensitivity: Sensitivity::Sensitive,
        expires_at_unix_ms: candidate.expires_at_unix_ms,
        max_bytes: MAX_APPROVAL_CONTEXT_BYTES as u64,
    };
    let mut source_payloads = Vec::with_capacity(source_ids.len());
    let mut exported = Vec::with_capacity(source_ids.len());
    let mut retention = RetentionBoundary {
        expires_at_unix_ms: Some(candidate.expires_at_unix_ms),
        delete_with_run: true,
    };
    let mut sensitivity = Sensitivity::Public;
    for message_id in source_ids {
        let message = session
            .conversation
            .iter()
            .find(|message| message.message_id == message_id)
            .ok_or(ApprovalReviewError::InvalidContext)?;
        if message.image_data_url.is_some()
            || message.attachment_read.is_some()
            || message.raw_result.is_some()
        {
            return Err(ApprovalReviewError::InvalidContext);
        }
        let source = message
            .data_envelope
            .as_ref()
            .ok_or(ApprovalReviewError::InvalidContext)?;
        let bytes =
            message_content_bytes(message).map_err(|_| ApprovalReviewError::InvalidContext)?;
        let mut exact = authorization.clone();
        exact.source_envelope_ids = vec![source.envelope_id.clone()];
        let exported_id = format!(
            "approval-export-{:x}",
            Sha256::digest(format!("{}:{message_id}", candidate.candidate_id).as_bytes()),
        );
        let (mut projection, record) = authorize_export(source, &exported_id, &exact, now_unix_ms)
            .map_err(|_| ApprovalReviewError::InvalidContext)?;
        projection.provenance.source_envelope_ids = record.input_envelope_ids;
        retention = retention.most_restrictive(projection.retention);
        if let ContentRef::EphemeralObservation {
            expires_at_unix_ms, ..
        } = &projection.content
        {
            retention = retention.most_restrictive(RetentionBoundary {
                expires_at_unix_ms: Some(*expires_at_unix_ms),
                delete_with_run: true,
            });
        }
        sensitivity = sensitivity.max(projection.sensitivity);
        source_payloads.push(bytes);
        exported.push(projection);
    }
    let inputs = exported
        .iter()
        .zip(&source_payloads)
        .map(|(envelope, bytes)| SinkInput { envelope, bytes })
        .collect::<Vec<_>>();
    let source_audit = DefaultSinkAuthorizer
        .authorize(
            destination,
            &inputs,
            now_unix_ms,
            MAX_APPROVAL_CONTEXT_BYTES,
        )
        .map_err(|_| ApprovalReviewError::InvalidContext)?
        .audit;

    let prompt = review_user_prompt(candidate)?;
    if prompt.len() > MAX_APPROVAL_CONTEXT_BYTES {
        return Err(ApprovalReviewError::InvalidContext);
    }
    let digest = format!("{:x}", Sha256::digest(prompt.as_bytes()));
    let prompt_envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("approval-prompt-{}", candidate.candidate_id),
        content: ContentRef::ImmutableBlob {
            blob_id: candidate.candidate_id.clone(),
            sha256: digest.clone(),
            size_bytes: prompt.len() as u64,
            media_type: "text/plain".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "agent-approval".into(),
            source_tool_name: "approval_review_projection".into(),
            source_object_id: Some(candidate.candidate_id.clone()),
            source_envelope_ids: exported
                .iter()
                .map(|source| source.envelope_id.clone())
                .collect(),
        },
        digest_sha256: digest,
        sensitivity,
        allowed_destinations: vec![destination.clone()],
        retention,
    };
    let prompt_audit = DefaultSinkAuthorizer
        .authorize(
            destination,
            &[SinkInput {
                envelope: &prompt_envelope,
                bytes: prompt.as_bytes(),
            }],
            now_unix_ms,
            MAX_APPROVAL_CONTEXT_BYTES,
        )
        .map_err(|_| ApprovalReviewError::InvalidContext)?
        .audit;
    Ok(AuthorizedApprovalReview {
        prompt,
        source_audit,
        prompt_audit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_review::{
        ApprovalAuthorityFact, ApprovalAuthorityStatus, ApprovalEvidence, ApprovalInputKind,
        PermissionReviewInput, SourceReviewInput, permission_review_candidate,
        source_review_candidate,
    };
    use crate::chat::{ChatMessage, ToolCallRef};
    use crate::dynamic_run::{GrantRequestItem, PERMISSION_REQUEST_SCHEMA_VERSION};
    use desk_agent_protocol::capability_grant::CapabilityRiskTier;
    use desk_agent_protocol::capability_provider::CapabilityEffect;

    fn envelope_for(message: &ChatMessage, sensitivity: Sensitivity) -> DataEnvelope {
        let bytes = message_content_bytes(message).unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("envelope-{}", message.message_id),
            content: ContentRef::ImmutableBlob {
                blob_id: format!("blob-{}", message.message_id),
                sha256: digest.clone(),
                size_bytes: bytes.len() as u64,
                media_type: "text/plain".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "test-owner".into(),
                source_tool_name: "test-message".into(),
                source_object_id: None,
                source_envelope_ids: vec![],
            },
            digest_sha256: digest,
            sensitivity,
            allowed_destinations: vec![],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        }
    }

    #[test]
    fn exact_review_requires_original_exportable_sources() {
        let now_unix_ms = chrono::DateTime::parse_from_rfc3339("2026-09-23T00:00:10Z")
            .unwrap()
            .timestamp_millis() as u64;
        let destination = DestinationIdentity::Model {
            connection_id: "review-connection".into(),
            connection_revision: 1,
            model_id: "review-model".into(),
            profile_revision: 1,
        };
        let delegation = ApprovalDelegation::new(
            "delegation".into(),
            "conversation".into(),
            "owner".into(),
            "device".into(),
            "owner-open".into(),
            1,
        )
        .unwrap();
        let exact = r#"{"path":"report.txt","content":"Updated"}"#;
        let request = PermissionRequest {
            schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
            request_id: "request".into(),
            input_revision: 1,
            state: PermissionRequestState::Pending,
            items: vec![GrantRequestItem {
                item_id: "write".into(),
                provider_id: "file.workspace".into(),
                tool_name: "write_text_file".into(),
                expected_effect: CapabilityEffect::WriteArtifact,
                resource_scope: vec!["directory:home".into()],
                operation_scope: vec!["write_text_file".into()],
                export_destinations: vec![],
                canonical_input_json: Some(exact.into()),
                canonical_input_digest_sha256: Some(format!(
                    "{:x}",
                    Sha256::digest(exact.as_bytes())
                )),
                command_confirmation: None,
                launch_confirmation: None,
                suggested_ttl_seconds: 300,
                suggested_max_uses: 1,
                reason: "Update the report".into(),
            }],
            created_at: "2026-09-23T00:00:00Z".into(),
        };
        let mut session = PersistedAgentSession::new(
            "conversation",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-09-23T00:00:00Z",
        );
        session.surface = AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        let mut user = ChatMessage::text("user", ChatRole::User, "Update the report");
        user.data_envelope = Some(envelope_for(&user, Sensitivity::UserContent));
        let mut assistant = ChatMessage::assistant_tool_calls(
            "assistant",
            "",
            vec![ToolCallRef {
                id: "call".into(),
                name: "request_permissions".into(),
                arguments_json: r#"{"tool_name":"write_text_file"}"#.into(),
            }],
        );
        assistant.data_envelope = Some(envelope_for(&assistant, Sensitivity::UserContent));
        session.conversation.push(user);
        session.conversation.push(assistant);
        session.conversation.push(ChatMessage::tool_result(
            "result",
            "call",
            r#"{"status":"pending_user_decision","request_id":"request"}"#,
        ));
        session.add_permission_request(request.clone()).unwrap();
        let authority = vec![ApprovalAuthorityFact {
            source: ApprovalSource::PermissionItem {
                request_id: "request".into(),
                item_id: "write".into(),
            },
            status: ApprovalAuthorityStatus::Pending,
            decision_event_id: None,
            active_grant_ids: vec![],
            dispatch_ids: vec![],
        }];
        let candidate = permission_review_candidate(PermissionReviewInput {
            session: &session,
            request: &request,
            item_id: "write",
            delegation: &delegation,
            goal: None,
            descriptor_json: r#"{"tool_name":"write_text_file","provider_id":"file.workspace"}"#,
            risk: CapabilityRiskTier::R2,
            input_kind: ApprovalInputKind::ExactCall,
            current_authority: authority,
            now_unix_ms,
        })
        .unwrap();
        let authorized = authorize_permission_review_egress(
            &candidate,
            &session,
            &request,
            None,
            &delegation,
            &destination,
            now_unix_ms,
        )
        .unwrap();
        assert!(authorized.prompt.contains(&candidate.candidate_id));
        assert_eq!(authorized.source_audit.envelope_ids.len(), 2);
        assert_eq!(authorized.prompt_audit.envelope_ids.len(), 1);

        let old_pause = "not executed: waiting for user permission decision";
        session.conversation.push(ChatMessage::tool_result(
            "skipped",
            "describe-call",
            old_pause,
        ));
        let mut contaminated = candidate.clone();
        contaminated.context.evidence.push(ApprovalEvidence {
            event_id: "skipped".into(),
            trust: ApprovalEvidenceTrust::UntrustedContent,
            text: old_pause.into(),
        });
        assert!(matches!(
            authorize_permission_review_egress(
                &contaminated,
                &session,
                &request,
                None,
                &delegation,
                &destination,
                10,
            ),
            Err(ApprovalReviewError::InvalidContext)
        ));
        session.conversation.pop();

        session.conversation[0].data_envelope = None;
        assert!(matches!(
            authorize_permission_review_egress(
                &candidate,
                &session,
                &request,
                None,
                &delegation,
                &destination,
                10,
            ),
            Err(ApprovalReviewError::InvalidContext)
        ));
        session.conversation[0].data_envelope =
            Some(envelope_for(&session.conversation[0], Sensitivity::Secret));
        assert!(matches!(
            authorize_permission_review_egress(
                &candidate,
                &session,
                &request,
                None,
                &delegation,
                &destination,
                10,
            ),
            Err(ApprovalReviewError::InvalidContext)
        ));
    }

    #[test]
    fn manager_command_export_requires_the_held_turn_and_exact_work_binding() {
        let destination = DestinationIdentity::Model {
            connection_id: "review-connection".into(),
            connection_revision: 1,
            model_id: "review-model".into(),
            profile_revision: 1,
        };
        let delegation = ApprovalDelegation::new(
            "delegation".into(),
            "conversation".into(),
            "owner".into(),
            "device".into(),
            "owner-open".into(),
            1,
        )
        .unwrap();
        let mut session = PersistedAgentSession::new(
            "conversation",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-09-23T00:00:00Z",
        );
        session.surface = AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        session.latest_input_seq = 1;
        session
            .begin_turn(
                "turn",
                None,
                None,
                1,
                session.scope_snapshot.clone(),
                "2026-09-23T00:00:00Z",
            )
            .unwrap();
        let mut user = ChatMessage::text("user", ChatRole::User, "Run the approved check");
        user.data_envelope = Some(envelope_for(&user, Sensitivity::UserContent));
        let mut assistant = ChatMessage::assistant_tool_calls(
            "assistant",
            "",
            vec![ToolCallRef {
                id: "command-call".into(),
                name: "exec_command".into(),
                arguments_json: r#"{"shell":"powershell","command":"Get-Date"}"#.into(),
            }],
        );
        assistant.turn_id = Some("turn".into());
        assistant.data_envelope = Some(envelope_for(&assistant, Sensitivity::UserContent));
        session.conversation.extend([user, assistant]);
        let source = ApprovalSource::ManagerCommand {
            work_id: "12".into(),
        };
        let action_json = r#"{"shell":"powershell","command":"Get-Date"}"#;
        let descriptor_json = r#"{"tool_name":"exec_command","provider_id":"manager.exec"}"#;
        let candidate = source_review_candidate(SourceReviewInput {
            session: &session,
            delegation: &delegation,
            goal: None,
            source: source.clone(),
            tool_name: "exec_command",
            descriptor_json,
            risk: CapabilityRiskTier::R3,
            input_kind: ApprovalInputKind::ExactCall,
            action_json,
            expires_at_unix_ms: 60_000,
            current_authority: vec![ApprovalAuthorityFact {
                source: source.clone(),
                status: ApprovalAuthorityStatus::Pending,
                decision_event_id: None,
                active_grant_ids: vec![],
                dispatch_ids: vec![],
            }],
            now_unix_ms: 10,
        })
        .unwrap();
        let binding = SourceReviewBinding {
            source: &source,
            tool_call_id: "command-call",
            turn_id: "turn",
            lease_token: session.lease_token,
            tool_name: "exec_command",
            action_json,
            descriptor_json,
            risk: CapabilityRiskTier::R3,
            input_kind: ApprovalInputKind::ExactCall,
            input_revision: 1,
            expires_at_unix_ms: 60_000,
        };
        assert!(
            authorize_source_review_egress(
                &candidate,
                &binding,
                &session,
                None,
                &delegation,
                &destination,
                10,
            )
            .is_ok()
        );
        let changed_action = SourceReviewBinding {
            action_json: r#"{"shell":"powershell","command":"Remove-Item report.txt"}"#,
            ..binding
        };
        assert!(
            authorize_source_review_egress(
                &candidate,
                &changed_action,
                &session,
                None,
                &delegation,
                &destination,
                10,
            )
            .is_err()
        );
        session.lease_token += 1;
        assert!(
            authorize_source_review_egress(
                &candidate,
                &binding,
                &session,
                None,
                &delegation,
                &destination,
                10,
            )
            .is_err()
        );
    }
}
