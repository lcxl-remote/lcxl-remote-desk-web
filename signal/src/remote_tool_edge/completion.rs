//! Pure projection of the original sealed action's bounded native completion.

use super::*;
use desk_agent_protocol::browser_control::{BrowserAction, BrowserActionOutcome};

struct PendingWaitGuard(String);

impl Drop for PendingWaitGuard {
    fn drop(&mut self) {
        // This removes a wake-up hint, never the durable work or native action.
        global_computer_action_pending().cancel(&self.0);
    }
}

impl SignalDeviceAssistantTools {
    pub(super) async fn finish_computer_action(
        &self,
        store: &SignalCapabilityGrantStore,
        call: &ToolCall,
        plan: &SealedComputerActionPlan,
        receiver: oneshot::Receiver<Result<ComputerActionCompleted, AgentError>>,
        sent: bool,
        mutating: bool,
    ) -> Result<ExecOutcome, AgentError> {
        let _pending_guard = PendingWaitGuard(plan.execution_generation.clone());
        let foreground_timeout = if mutating {
            store
                .computer_foreground_remaining(&plan.execution_generation)
                .await
                .map_err(|_| invalid())?
                .min(self.timeout)
        } else {
            self.timeout
        };
        let completed = if sent {
            tokio::time::timeout(foreground_timeout, receiver)
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(Result::ok)
        } else {
            None
        };
        global_computer_action_pending().cancel(&plan.execution_generation);
        let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| invalid())?;
        if mutating {
            if let Some(original) = store
                .read_computer_result(
                    &plan.execution_generation,
                    &self.run_id,
                    &self.actor_id,
                    &self.target_device_id,
                )
                .await
                .map_err(|_| invalid())?
            {
                return Ok(original.into_exec());
            }
        } else if let Some(completed) = completed {
            let canonical = canonical_tool_permission_input_json(
                &call.name,
                serde_json::from_str(&call.arguments_json).map_err(|_| invalid())?,
            )
            .map_err(|_| invalid())?;
            if let Some(projection) =
                project(plan, &call.name, &self.run_id, &canonical, &completed)?
            {
                store
                    .record_dispatch_completion(
                        &CapabilityDispatchCompletion {
                            dispatch_id: plan.execution_generation.clone(),
                            call_id: plan.action_request_id.clone(),
                            generation: 1,
                            outcome: projection.outcome,
                            result_digest_sha256: format!(
                                "{:x}",
                                Sha256::digest(projection.content.as_bytes())
                            ),
                        },
                        now_ms,
                    )
                    .await
                    .map_err(|_| invalid())?;
                let output = ToolRunOutput {
                    content: projection.content,
                    image_data_url: None,
                };
                return Ok(match projection.outcome {
                    CapabilityDispatchOutcome::Succeeded => ExecOutcome::Executed {
                        output,
                        event_id: None,
                        data_envelope: None,
                    },
                    CapabilityDispatchOutcome::Failed => ExecOutcome::Failed {
                        output,
                        event_id: None,
                        data_envelope: None,
                    },
                });
            }
        }
        if mutating
            && sent
            && store
                .promote_computer_background(
                    &plan.execution_generation,
                    &self.run_id,
                    &self.actor_id,
                    &self.target_device_id,
                )
                .await
                .map_err(|_| invalid())?
        {
            return Ok(ExecOutcome::Dispatched(
                desk_diagnose_core::session::ActionIdentity::new(
                    plan.work_id.parse().map_err(|_| invalid())?,
                    &plan.action_request_id,
                    &plan.execution_generation,
                    desk_diagnose_core::session::WorkKind::ComputerAction,
                ),
            ));
        }
        store
            .mark_dispatch_outcome_unknown(
                &plan.execution_generation,
                &plan.action_request_id,
                1,
                now_ms,
            )
            .await
            .map_err(|_| invalid())?;
        // A terminal observation can win between the timeout and unknown write.
        if mutating
            && let Some(original) = store
                .read_computer_result(
                    &plan.execution_generation,
                    &self.run_id,
                    &self.actor_id,
                    &self.target_device_id,
                )
                .await
                .map_err(|_| invalid())?
        {
            return Ok(original.into_exec());
        }
        Ok(ExecOutcome::Unknown(
            desk_diagnose_core::session::ActionIdentity::new(
                plan.work_id.parse().map_err(|_| invalid())?,
                &plan.action_request_id,
                &plan.execution_generation,
                desk_diagnose_core::session::WorkKind::ComputerAction,
            ),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Projection {
    pub outcome: CapabilityDispatchOutcome,
    pub content: String,
}

pub(crate) fn invalid() -> AgentError {
    error(
        AgentErrorKind::Internal,
        "invalid original Computer Action result",
        false,
        false,
    )
}

pub(crate) fn project(
    plan: &SealedComputerActionPlan,
    tool_name: &str,
    run_id: &str,
    canonical_input: &str,
    completed: &ComputerActionCompleted,
) -> Result<Option<Projection>, AgentError> {
    plan.validate().map_err(|_| invalid())?;
    let content = serde_json::to_string(completed).map_err(|_| invalid())?;
    if plan.actions.len() != 1
        || completed.work_id != plan.work_id
        || completed.action_request_id != plan.action_request_id
        || completed.execution_generation != plan.execution_generation
        || content.len() > 128 * 1024
        || completed.facts.len() > 1
        || completed
            .facts
            .iter()
            .any(|f| f.index != 0 || f.summary.len() > 4096)
        || completed.message.as_ref().is_some_and(|s| s.len() > 4096)
    {
        return Err(invalid());
    }
    let verified = completed.result == ComputerActionResultClass::Verified;
    if verified && (completed.facts.len() != 1 || !completed.facts[0].verified) {
        return Err(invalid());
    }
    if completed.result == ComputerActionResultClass::DefinitelyNotStarted
        && (completed.output.is_some() || completed.facts.iter().any(|f| f.changed || f.verified))
    {
        return Err(invalid());
    }
    let action = &plan.actions[0].action;
    // An assistive Outlook handoff is usable but deliberately unverified; it
    // never acquires semantic read-back or send authority from this projection.
    if let ComputerActionKind::Communication(request) = action {
        if let Some(ComputerActionOutput::CommunicationHandoff(handoff)) = &completed.output {
            handoff.validate().map_err(|_| invalid())?;
            let expected = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(request).map_err(|_| invalid())?)
            );
            if completed.result != ComputerActionResultClass::ChangedButUnverified
                || completed.facts.len() != 1
                || !completed.facts[0].changed
                || completed.facts[0].verified
                || request.run_id != run_id
                || request.call_id != plan.action_request_id
                || handoff.run_id != run_id
                || handoff.surface != request.surface
                || handoff.prepared_payload_sha256 != expected
                || handoff.verification != CommunicationPrepareVerification::AssistiveUnverified
                || handoff.readback_payload_sha256.is_some()
                || handoff.send_authority != CommunicationSendAuthority::ManualOnly
            {
                return Err(invalid());
            }
            return Ok(Some(Projection {
                outcome: CapabilityDispatchOutcome::Succeeded,
                content: serde_json::to_string(handoff).map_err(|_| invalid())?,
            }));
        }
        if completed.output.is_some() || verified {
            return Err(invalid());
        }
    } else {
        validate_output(action, &plan.device_id, completed)?;
    }
    if matches!(
        completed.result,
        ComputerActionResultClass::OutcomeUnknown
            | ComputerActionResultClass::ChangedButUnverified
            | ComputerActionResultClass::PartiallyApplied
            | ComputerActionResultClass::RollbackUnsafe
    ) || (!verified && completed.facts.iter().any(|f| f.changed))
    {
        return Ok(None);
    }
    let content = if verified && matches!(action, ComputerActionKind::Browser(_)) {
        browser_projection(
            tool_name,
            run_id,
            canonical_input,
            &plan.draft_hash,
            completed,
        )?
        .ok_or_else(invalid)?
    } else {
        content
    };
    Ok(Some(Projection {
        outcome: if verified {
            CapabilityDispatchOutcome::Succeeded
        } else {
            CapabilityDispatchOutcome::Failed
        },
        content,
    }))
}

fn validate_output(
    action: &ComputerActionKind,
    audience: &str,
    completed: &ComputerActionCompleted,
) -> Result<(), AgentError> {
    let verified = completed.result == ComputerActionResultClass::Verified;
    let Some(output) = &completed.output else {
        return if verified
            && matches!(
                action,
                ComputerActionKind::Browser(_)
                    | ComputerActionKind::File(_)
                    | ComputerActionKind::SpreadsheetLiveBatch(_)
                    | ComputerActionKind::DocumentLiveBatch(_)
                    | ComputerActionKind::PresentationLiveBatch(_)
            ) {
            Err(invalid())
        } else {
            Ok(())
        };
    };
    match (action, output) {
        (ComputerActionKind::Browser(request), ComputerActionOutput::Browser(result)) => {
            result.validate().map_err(|_| invalid())?;
            let expected = match request.action {
                BrowserAction::OpenPage { .. } => BrowserActionOutcome::PageOpened,
                BrowserAction::NavigatePage { .. } => BrowserActionOutcome::PageNavigated,
                BrowserAction::TakeSnapshot { .. } => BrowserActionOutcome::SnapshotCaptured,
                BrowserAction::WaitFor { .. } => BrowserActionOutcome::WaitSatisfied,
                BrowserAction::FillForm { .. } => BrowserActionOutcome::FormFilled,
                BrowserAction::FillFormAndUpload { .. } => BrowserActionOutcome::FormFilledWithFile,
                BrowserAction::UploadFile { .. } => BrowserActionOutcome::FileUploaded,
                BrowserAction::ActivateElement { .. } => BrowserActionOutcome::ElementActivated,
            };
            if result.call_id != completed.action_request_id
                || result.page.adapter.device_id != audience
                || result.outcome != expected
            {
                return Err(invalid());
            }
        }
        (ComputerActionKind::File(action), ComputerActionOutput::FileArtifact(artifact)) => {
            artifact.validate().map_err(|_| invalid())?;
            let expected = match action {
                FilePatchAction::CreateTextArtifact { file_name, .. }
                | FilePatchAction::CreateSpreadsheetArtifact { file_name, .. }
                | FilePatchAction::CreateSpreadsheetFormulaArtifact { file_name, .. }
                | FilePatchAction::CreateWordReportArtifact { file_name, .. }
                | FilePatchAction::CreateLocalCommunicationDraftArtifact { file_name, .. } => {
                    file_name
                }
                _ => return Err(invalid()),
            };
            if &artifact.file_name != expected || (verified && !completed.facts[0].changed) {
                return Err(invalid());
            }
        }
        (
            ComputerActionKind::SpreadsheetLiveBatch(_)
            | ComputerActionKind::DocumentLiveBatch(_)
            | ComputerActionKind::PresentationLiveBatch(_),
            ComputerActionOutput::BatchDocumentArtifact(artifact),
        ) => {
            let expected = match action {
                ComputerActionKind::SpreadsheetLiveBatch(a) => &a.output.native_file_name,
                ComputerActionKind::DocumentLiveBatch(a) => &a.output.native_file_name,
                ComputerActionKind::PresentationLiveBatch(a) => &a.output.native_file_name,
                _ => unreachable!(),
            };
            let sha = |s: &str| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            };
            if artifact.file.object_kind != ObjectKind::File
                || artifact.file.token.is_empty()
                || artifact.file.snapshot_id.is_empty()
                || chrono::DateTime::parse_from_rfc3339(&artifact.file.expires_at).is_err()
                || &artifact.file_name != expected
                || artifact.byte_len == 0
                || artifact.validation_byte_len == 0
                || !sha(&artifact.sha256)
                || !sha(&artifact.validation_sha256)
                || (verified && !completed.facts[0].changed)
            {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    Ok(())
}

fn browser_projection(
    tool_name: &str,
    run_id: &str,
    canonical_input: &str,
    canonical_input_digest_sha256: &str,
    completion: &ComputerActionCompleted,
) -> Result<Option<String>, AgentError> {
    if let Some(handoff) = desk_diagnose_core::communication_handoff::project_web_draft_handoff(
        tool_name,
        run_id,
        canonical_input,
        canonical_input_digest_sha256,
        completion,
    )? {
        return serde_json::to_string(&handoff)
            .map(Some)
            .map_err(|_| invalid());
    }
    completion
        .output
        .as_ref()
        .and_then(|output| match output {
            ComputerActionOutput::Browser(result) => Some(result),
            _ => None,
        })
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| invalid())
}

#[cfg(test)]
mod wait_tests {
    use super::*;

    #[tokio::test]
    async fn dropping_a_foreground_future_releases_its_wakeup_registration() {
        let generation = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        assert!(global_computer_action_pending().register(
            generation.clone(),
            "synthetic-host".into(),
            sender
        ));
        let (ready, observed) = oneshot::channel();
        let mut foreground = Box::pin(async move {
            let _guard = PendingWaitGuard(generation);
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        tokio::select! {
            _ = &mut foreground => unreachable!(),
            _ = observed => {}
        }
        drop(foreground);
        assert!(receiver.await.is_err());
    }
}
