//! Bind generated text to the actual sealed send payload and current provider surface.
use super::*;
use desk_agent_protocol::communication::{CommunicationSurfaceRef, SendPayloadSnapshot};

/// Resolved by the server from the current authorized device record. Manager's
/// registry key and the provider's host audience are distinct identities.
pub struct TaskMessageTarget<'a> {
    pub task_device_id: &'a str,
    pub provider_device_id: &'a str,
}

/// Stable task-level authority label. It deliberately excludes per-run UI tokens,
/// session IDs, readiness revisions and generated text. A caller must separately
/// bind the current sealed payload before using this label to evaluate a call.
pub fn task_message_resource_scope(
    task_device_id: &str,
    target: &TaskMessageDestination,
) -> Result<Vec<String>, TaskContractError> {
    id(task_device_id)?;
    destination(target)?;
    let encoded = canonical(serde_json::json!({
        "schema": "task-message-resource/v1",
        "device": task_device_id,
        "destination": target,
    }))?;
    Ok(vec![format!(
        "task-message:sha256:{:x}",
        Sha256::digest(encoded.as_bytes())
    )])
}

impl ValidatedTaskContract {
    /// The caller supplies the surface from current trusted provider preflight,
    /// never from model arguments or a historical rehearsal. This checks message
    /// binding only; policy, lineage, budgets, step state and dispatch fencing
    /// remain required. No grant or send permission is produced here.
    /// Checks an approval request's hard boundary, never evidence for dispatch.
    pub(super) fn verify_generated_send_snapshot_approval_scope(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        step_id: &str,
        snapshot: &SendPayloadSnapshot,
        current_surface: &CommunicationSurfaceRef,
        message: &TaskGeneratedMessage,
    ) -> Result<&TaskMessageDestination, TaskContractError> {
        self.verify_generated_send_snapshot_inner(
            target,
            run_id,
            step_id,
            snapshot,
            current_surface,
            message,
            None,
            true,
        )
    }

    pub fn verify_generated_send_snapshot(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        step_id: &str,
        snapshot: &SendPayloadSnapshot,
        current_surface: &CommunicationSurfaceRef,
        message: &TaskGeneratedMessage,
    ) -> Result<&TaskMessageDestination, TaskContractError> {
        self.verify_generated_send_snapshot_inner(
            target,
            run_id,
            step_id,
            snapshot,
            current_surface,
            message,
            None,
            false,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Keep destination, live surface, message and independently verified attachment/source evidence separate at the send boundary"
    )]
    pub fn verify_generated_send_snapshot_with_attachments(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        step_id: &str,
        snapshot: &SendPayloadSnapshot,
        current_surface: &CommunicationSurfaceRef,
        message: &TaskGeneratedMessage,
        attachments: &super::attachment::TaskAttachmentEvaluation,
    ) -> Result<&TaskMessageDestination, TaskContractError> {
        self.verify_generated_send_snapshot_inner(
            target,
            run_id,
            step_id,
            snapshot,
            current_surface,
            message,
            Some(attachments),
            false,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Keep destination, live surface, message and independently verified attachment/source evidence separate at the send boundary"
    )]
    fn verify_generated_send_snapshot_inner(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        step_id: &str,
        snapshot: &SendPayloadSnapshot,
        current_surface: &CommunicationSurfaceRef,
        message: &TaskGeneratedMessage,
        attachments: Option<&super::attachment::TaskAttachmentEvaluation>,
        approval_scope_only: bool,
    ) -> Result<&TaskMessageDestination, TaskContractError> {
        crate::communication::verify_send_payload_snapshot(snapshot)
            .map_err(|_| TaskContractError::InvalidInput)?;
        let payload = &snapshot.payload;
        if run_id.is_empty()
            || snapshot.run_id != run_id
            || payload.surface != *current_surface
            || target.task_device_id != self.contract.target_device_id
            || target.provider_device_id != current_surface.device_id
        {
            return Err(TaskContractError::InvalidIdentity);
        }
        let step = self
            .contract
            .steps
            .iter()
            .find(|step| step.step_id == step_id)
            .ok_or(TaskContractError::InvalidSteps)?;
        let TaskStepBinding::SendMessage { destination, .. } = &step.binding else {
            return Err(TaskContractError::InvalidSteps);
        };
        let rule = self
            .contract
            .permissions
            .iter()
            .find(|rule| rule.rule_id == step.rule_id)
            .ok_or(TaskContractError::InvalidSteps)?;
        let TaskInputConstraint::GeneratedMessage {
            max_subject_bytes,
            max_body_bytes,
            ..
        } = rule.input
        else {
            return Err(TaskContractError::InvalidInput);
        };
        if destination.channel != current_surface.channel
            || destination.surface_kind != current_surface.kind
            || destination.scope != current_surface.scope
            || destination.adapter_id != current_surface.adapter_id
            || destination.adapter_version != current_surface.adapter_version
            || destination.profile_id != current_surface.profile_id
            || destination.account_id != current_surface.account_id
            || destination.recipients != payload.recipients
        {
            return Err(TaskContractError::InvalidDestination);
        }
        if message.body.is_empty()
            || message.body.len() > max_body_bytes as usize
            || message.subject.len() > max_subject_bytes as usize
            || payload.subject != message.subject
            || !(payload.attachments.is_empty()
                || (approval_scope_only
                    && self.attachment_approval_scope(step_id, &payload.attachments))
                || attachments.is_some_and(|evidence| {
                    evidence.matches(self, run_id, step_id, &payload.attachments)
                }))
            || !matches!(
                payload.body.media_type.as_str(),
                "text/plain" | "text/plain; charset=utf-8"
            )
            || payload.body.size_bytes != message.body.len() as u64
            || payload.body.digest_sha256
                != format!("{:x}", Sha256::digest(message.body.as_bytes()))
        {
            return Err(TaskContractError::InvalidInput);
        }
        Ok(destination)
    }
}

impl ValidatedTaskContract {
    /// Historical coverage only. The caller verifies the original owner-bound
    /// dispatch/grant and resolves sources from the model proposal's successful
    /// receipt. Current provider readiness, policy and publication remain separate.
    pub fn observed_generated_message_step(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
        sent: &crate::communication_handoff::SentWebMessageEvidence,
        sources: &crate::schedule::source_graph::ResolvedTaskSources,
    ) -> Option<&TaskFixedStep> {
        self.observed_generated_message_step_inner(target, run_id, observed, sent, sources, None)
    }

    pub fn observed_generated_message_step_with_attachments(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
        sent: &crate::communication_handoff::SentWebMessageEvidence,
        sources: &crate::schedule::source_graph::ResolvedTaskSources,
        attachments: &super::attachment::TaskAttachmentEvaluation,
    ) -> Option<&TaskFixedStep> {
        self.observed_generated_message_step_inner(
            target,
            run_id,
            observed,
            sent,
            sources,
            Some(attachments),
        )
    }

    fn observed_generated_message_step_inner(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
        sent: &crate::communication_handoff::SentWebMessageEvidence,
        sources: &crate::schedule::source_graph::ResolvedTaskSources,
        attachments: Option<&super::attachment::TaskAttachmentEvaluation>,
    ) -> Option<&TaskFixedStep> {
        use desk_agent_protocol::communication::SendOutcome;
        let snapshot = &sent.snapshot;
        if observed.effect != CapabilityEffect::SendExternal
            || observed.resources.is_empty()
            || observed.operations.is_empty()
            || digest(&observed.canonical_input_sha256).is_err()
            || sent.receipt.validate().is_err()
            || sent.receipt.outcome != SendOutcome::Sent
            || sent.receipt.snapshot_id != snapshot.snapshot_id
            || sent.receipt.snapshot_sha256 != snapshot.canonical_payload_sha256
            || sent.receipt.idempotency_key
                != crate::communication::send_idempotency_key(snapshot).ok()?
            || sent.receipt.observed_at_unix_ms < snapshot.sealed_at_unix_ms
            || sources.scopes.is_empty()
            || sources.root_envelope_ids.is_empty()
        {
            return None;
        }
        let message = TaskGeneratedMessage {
            subject: sent.subject.clone(),
            body: sent.body_plain_text.clone(),
        };
        self.match_generated_message_step(
            target,
            run_id,
            observed,
            snapshot,
            &message,
            sources,
            attachments,
        )
    }

    /// A prepared draft can cover only WriteExternalDraft, never SendExternal.
    pub fn observed_generated_draft_step(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
        prepared: &crate::communication_handoff::PreparedWebMessageEvidence,
        sources: &crate::schedule::source_graph::ResolvedTaskSources,
    ) -> Option<&TaskFixedStep> {
        self.observed_generated_draft_step_inner(target, run_id, observed, prepared, sources, None)
    }

    pub fn observed_generated_draft_step_with_attachments(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
        prepared: &crate::communication_handoff::PreparedWebMessageEvidence,
        sources: &crate::schedule::source_graph::ResolvedTaskSources,
        attachments: &super::attachment::TaskAttachmentEvaluation,
    ) -> Option<&TaskFixedStep> {
        self.observed_generated_draft_step_inner(
            target,
            run_id,
            observed,
            prepared,
            sources,
            Some(attachments),
        )
    }

    fn observed_generated_draft_step_inner(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
        prepared: &crate::communication_handoff::PreparedWebMessageEvidence,
        sources: &crate::schedule::source_graph::ResolvedTaskSources,
        attachments: Option<&super::attachment::TaskAttachmentEvaluation>,
    ) -> Option<&TaskFixedStep> {
        if observed.effect != CapabilityEffect::WriteExternalDraft {
            return None;
        }
        self.match_generated_message_step(
            target,
            run_id,
            observed,
            &prepared.snapshot,
            &TaskGeneratedMessage {
                subject: prepared.subject.clone(),
                body: prepared.body_plain_text.clone(),
            },
            sources,
            attachments,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Keep destination, live surface, message and independently verified attachment/source evidence separate at the send boundary"
    )]
    fn match_generated_message_step(
        &self,
        target: &TaskMessageTarget<'_>,
        run_id: &str,
        observed: &crate::provider_preflight::ObservedCapabilityAuthority,
        snapshot: &SendPayloadSnapshot,
        message: &TaskGeneratedMessage,
        sources: &crate::schedule::source_graph::ResolvedTaskSources,
        attachments: Option<&super::attachment::TaskAttachmentEvaluation>,
    ) -> Option<&TaskFixedStep> {
        if observed.resources.is_empty()
            || observed.operations.is_empty()
            || digest(&observed.canonical_input_sha256).is_err()
            || sources.scopes.is_empty()
            || sources.root_envelope_ids.is_empty()
        {
            return None;
        }
        self.contract.steps.iter().find(|step| {
            let TaskStepBinding::SendMessage {
                destination,
                allowed_source_scopes,
            } = &step.binding
            else {
                return false;
            };
            let Some(rule) = self
                .contract
                .permissions
                .iter()
                .find(|rule| rule.rule_id == step.rule_id)
            else {
                return false;
            };
            if !subset(&sources.scopes, allowed_source_scopes)
                || self
                    .verify_generated_send_snapshot_inner(
                        target,
                        run_id,
                        &step.step_id,
                        snapshot,
                        &snapshot.payload.surface,
                        message,
                        attachments,
                        false,
                    )
                    .is_err()
                || rule.provider_id != observed.provider_id
                || rule.capability_id != observed.capability_id
                || rule.tool_name != observed.tool_name
                || rule.tool_schema_version != observed.tool_schema_version
                || rule.effect != observed.effect
                || rule.risk_tier != observed.risk_tier
                || !subset(&rule.automatic.operations, &observed.operations)
                || !subset(&observed.operations, &rule.automatic.operations)
                || !subset(
                    &rule.automatic.export_destinations,
                    &observed.export_destinations,
                )
                || !subset(
                    &observed.export_destinations,
                    &rule.automatic.export_destinations,
                )
            {
                return false;
            }
            // Original provider resource tokens are per-run. Only the exact sealed
            // destination can map them to the stable task resource identity.
            task_message_resource_scope(target.task_device_id, destination)
                .is_ok_and(|resources| resources == rule.automatic.resources)
        })
    }
}

impl ValidatedTaskContract {
    /// Bind a new draft to a page recovered from this run's verified browser
    /// receipt. The caller still checks model sources, budget and live dispatch.
    /// Checks an approval request's hard boundary, never evidence for dispatch.
    pub(super) fn verify_generated_gmail_draft_approval_scope(
        &self,
        target: &TaskMessageTarget<'_>,
        step_id: &str,
        input: &desk_agent_protocol::communication::GmailWebDraftHandoffInput,
        page: &desk_agent_protocol::browser_control::BrowserPageRef,
    ) -> Result<&TaskMessageDestination, TaskContractError> {
        self.verify_generated_gmail_draft_inner(target, step_id, "", None, input, page, true)
    }

    pub fn verify_generated_gmail_draft(
        &self,
        target: &TaskMessageTarget<'_>,
        step_id: &str,
        input: &desk_agent_protocol::communication::GmailWebDraftHandoffInput,
        page: &desk_agent_protocol::browser_control::BrowserPageRef,
    ) -> Result<&TaskMessageDestination, TaskContractError> {
        self.verify_generated_gmail_draft_inner(target, step_id, "", None, input, page, false)
    }

    pub fn verify_generated_gmail_draft_with_attachments(
        &self,
        target: &TaskMessageTarget<'_>,
        step_id: &str,
        run_id: &str,
        attachments: &super::attachment::TaskAttachmentEvaluation,
        input: &desk_agent_protocol::communication::GmailWebDraftHandoffInput,
        page: &desk_agent_protocol::browser_control::BrowserPageRef,
    ) -> Result<&TaskMessageDestination, TaskContractError> {
        self.verify_generated_gmail_draft_inner(
            target,
            step_id,
            run_id,
            Some(attachments),
            input,
            page,
            false,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Keep destination, live surface, message and independently verified attachment/source evidence separate at the send boundary"
    )]
    fn verify_generated_gmail_draft_inner(
        &self,
        target: &TaskMessageTarget<'_>,
        step_id: &str,
        run_id: &str,
        attachments: Option<&super::attachment::TaskAttachmentEvaluation>,
        input: &desk_agent_protocol::communication::GmailWebDraftHandoffInput,
        page: &desk_agent_protocol::browser_control::BrowserPageRef,
        approval_scope_only: bool,
    ) -> Result<&TaskMessageDestination, TaskContractError> {
        use desk_agent_protocol::{
            browser_control::BrowserEngineKind,
            communication::{RecipientIdentity, RecipientKind},
        };
        input
            .validate()
            .map_err(|_| TaskContractError::InvalidInput)?;
        if input.page != *page
            || input.attachment.as_ref().is_some_and(|attachment| {
                let requested = desk_agent_protocol::communication::ImmutableAttachmentSnapshot {
                    content: attachment.artifact.content.clone(),
                    file_name: attachment.artifact.file_name.clone(),
                    media_type: attachment.artifact.media_type.clone(),
                    size_bytes: attachment.artifact.size_bytes,
                    digest_sha256: attachment.artifact.digest_sha256.clone(),
                };
                !(approval_scope_only
                    && self.attachment_approval_scope(step_id, std::slice::from_ref(&requested)))
                    && attachments.is_none_or(|evidence| {
                        !evidence.matches(self, run_id, step_id, &[requested])
                    })
            })
            || page.adapter.engine != BrowserEngineKind::ChromeExtension
            || page.adapter.device_id != target.provider_device_id
            || self.contract.target_device_id != target.task_device_id
        {
            return Err(TaskContractError::InvalidIdentity);
        }
        let step = self
            .contract
            .steps
            .iter()
            .find(|step| step.step_id == step_id)
            .ok_or(TaskContractError::InvalidSteps)?;
        let TaskStepBinding::SendMessage { destination, .. } = &step.binding else {
            return Err(TaskContractError::InvalidSteps);
        };
        let rule = self
            .contract
            .permissions
            .iter()
            .find(|rule| rule.rule_id == step.rule_id)
            .ok_or(TaskContractError::InvalidSteps)?;
        let TaskInputConstraint::GeneratedMessage {
            max_subject_bytes,
            max_body_bytes,
            ..
        } = rule.input
        else {
            return Err(TaskContractError::InvalidInput);
        };
        if rule.effect != CapabilityEffect::WriteExternalDraft
            || input.draft.body_plain_text.is_empty()
            || input.draft.subject.len() > max_subject_bytes as usize
            || input.draft.body_plain_text.len() > max_body_bytes as usize
        {
            return Err(TaskContractError::InvalidInput);
        }
        let canonical =
            crate::communication::canonicalize_email_address(&input.draft.recipients[0].address)
                .map_err(|_| TaskContractError::InvalidDestination)?;
        let recipient = RecipientIdentity {
            role: RecipientRole::To,
            kind: RecipientKind::EmailMailbox,
            stable_id: format!(
                "gmail-mailbox-{:x}",
                Sha256::digest(canonical.value.as_bytes())
            ),
            canonical_address: canonical.value,
            display_name: input.draft.recipients[0].display_name.clone(),
            display_warnings: canonical.display_warnings,
            resolved_members: Vec::new(),
            member_snapshot_sha256: None,
        };
        if destination.channel != CommunicationChannel::Email
            || destination.surface_kind != CommunicationSurfaceKind::ChromeExtension
            || destination.scope
                != (CommunicationSurfaceScope::WebOrigin {
                    origin: page.origin.clone(),
                })
            || destination.adapter_id != crate::device_assistant::GMAIL_WEB_ADAPTER_ID
            || destination.adapter_version != crate::device_assistant::GMAIL_WEB_ADAPTER_VERSION
            || destination.profile_id != page.adapter.profile_incarnation
            || destination.account_id
                != crate::communication::gmail_web_account_id(page)
                    .map_err(|_| TaskContractError::InvalidDestination)?
            || destination.recipients != [recipient]
        {
            return Err(TaskContractError::InvalidDestination);
        }
        Ok(destination)
    }
}

impl ValidatedTaskContract {
    /// The caller recovers the page from this run's authenticated browser receipt.
    /// Preparing a draft does not grant authority to activate the send control.
    pub fn verify_generated_slack_draft(
        &self,
        target: &TaskMessageTarget<'_>,
        step_id: &str,
        input: &desk_agent_protocol::communication::SlackWebDraftHandoffInput,
        page: &desk_agent_protocol::browser_control::BrowserPageRef,
    ) -> Result<&TaskMessageDestination, TaskContractError> {
        use desk_agent_protocol::{
            browser_control::BrowserEngineKind,
            communication::{RecipientIdentity, RecipientKind},
        };
        input
            .validate()
            .map_err(|_| TaskContractError::InvalidInput)?;
        if input.page != *page
            || page.adapter.engine != BrowserEngineKind::ChromeExtension
            || page.adapter.device_id != target.provider_device_id
            || self.contract.target_device_id != target.task_device_id
        {
            return Err(TaskContractError::InvalidIdentity);
        }
        let step = self
            .contract
            .steps
            .iter()
            .find(|step| step.step_id == step_id)
            .ok_or(TaskContractError::InvalidSteps)?;
        let TaskStepBinding::SendMessage { destination, .. } = &step.binding else {
            return Err(TaskContractError::InvalidSteps);
        };
        let rule = self
            .contract
            .permissions
            .iter()
            .find(|rule| rule.rule_id == step.rule_id)
            .ok_or(TaskContractError::InvalidSteps)?;
        let TaskInputConstraint::GeneratedMessage { max_body_bytes, .. } = &rule.input else {
            return Err(TaskContractError::InvalidInput);
        };
        if rule.effect != CapabilityEffect::WriteExternalDraft
            || rule.tool_name != "prepare_slack_web_message_handoff"
            || input.body_plain_text.is_empty()
            || input.body_plain_text.len() > *max_body_bytes as usize
        {
            return Err(TaskContractError::InvalidInput);
        }
        let address = input.composer.accessible_name.trim().to_owned();
        let recipient = RecipientIdentity {
            role: RecipientRole::ChatDestination,
            kind: RecipientKind::ChatChannel,
            stable_id: format!(
                "slack-destination-{:x}",
                Sha256::digest(
                    format!("{}:{address}", page.adapter.profile_incarnation).as_bytes()
                )
            ),
            canonical_address: address,
            display_name: None,
            display_warnings: Vec::new(),
            resolved_members: Vec::new(),
            member_snapshot_sha256: None,
        };
        if destination.channel != CommunicationChannel::Chat
            || destination.surface_kind != CommunicationSurfaceKind::ChromeExtension
            || destination.scope
                != (CommunicationSurfaceScope::WebOrigin {
                    origin: page.origin.clone(),
                })
            || destination.adapter_id != crate::device_assistant::SLACK_WEB_ADAPTER_ID
            || destination.adapter_version != crate::device_assistant::SLACK_WEB_ADAPTER_VERSION
            || destination.profile_id != page.adapter.profile_incarnation
            || destination.account_id
                != crate::communication::slack_web_account_id(page)
                    .map_err(|_| TaskContractError::InvalidDestination)?
            || destination.recipients != [recipient]
        {
            return Err(TaskContractError::InvalidDestination);
        }
        Ok(destination)
    }
}
