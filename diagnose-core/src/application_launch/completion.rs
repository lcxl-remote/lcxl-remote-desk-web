//! Launch submission evidence must never imply that an application's UI is ready.
use desk_agent_protocol::AgentError;
use desk_agent_protocol::application_launch::{
    ArgumentDelivery, LaunchApprovalBinding, LaunchOutcome,
};
use desk_agent_protocol::computer_use::{
    ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass,
};

pub fn validate(
    binding: &LaunchApprovalBinding,
    completed: &ComputerActionCompleted,
) -> Result<(), AgentError> {
    let invalid = || super::permission_error("Invalid application launch receipt");
    if completed
        .facts
        .iter()
        .any(|fact| fact.verified || fact.index != 0)
        || completed.facts.len() > 1
    {
        return Err(invalid());
    }
    let Some(ComputerActionOutput::ApplicationLaunch(result)) = &completed.output else {
        return if completed.output.is_none()
            && matches!(
                completed.result,
                ComputerActionResultClass::DefinitelyNotStarted
                    | ComputerActionResultClass::OutcomeUnknown
            )
            && completed.facts.iter().all(|fact| !fact.changed)
        {
            Ok(())
        } else {
            Err(invalid())
        };
    };
    let expected = match result.launch_outcome {
        LaunchOutcome::LaunchAccepted => ComputerActionResultClass::ChangedButUnverified,
        LaunchOutcome::NotDispatched | LaunchOutcome::LaunchFailed => {
            ComputerActionResultClass::DefinitelyNotStarted
        }
        LaunchOutcome::OutcomeUnknown => ComputerActionResultClass::OutcomeUnknown,
    };
    let accepted = result.launch_outcome == LaunchOutcome::LaunchAccepted;
    if completed.result != expected
        || result.dispatch_id != completed.execution_generation
        || result.requested_admin != binding.request().run_as_admin
        || result.created_process_id == Some(0)
        || (result.created_process_elevated.is_some() && result.created_process_id.is_none())
        || (result.created_process_elevated == Some(true) && !result.requested_admin)
        || (!accepted
            && (result.created_process_id.is_some() || result.created_process_elevated.is_some()))
        || (accepted && (result.failure_reason.is_some() || result.diagnostic.is_some()))
        || completed.facts.iter().any(|fact| fact.changed != accepted)
        || (accepted && completed.facts.len() != 1)
        || (binding.request().args.is_empty()
            != (result.argument_delivery == ArgumentDelivery::NotRequested))
        || (accepted && result.argument_delivery == ArgumentDelivery::Unsupported)
        || result
            .diagnostic
            .as_ref()
            .is_some_and(|diagnostic| !diagnostic.is_bounded())
        || result.observations.len() > 16
        || result.observations.iter().any(|observation| {
            observation.observed_at_unix_ms == 0 || observation.process_id == Some(0)
        })
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::application_launch::*;
    use desk_agent_protocol::computer_use::ComputerActionStepFact;
    fn fixture() -> (LaunchApprovalBinding, ComputerActionCompleted) {
        let binding = LaunchApprovalBinding::prepare(
            LaunchApprovalSubject {
                actor_id: "owner".into(),
                device_id: "device".into(),
                session_id: "session".into(),
                input_revision: 1,
                policy_revision: 1,
                readiness_revision: 1,
            },
            LaunchApplicationRequest {
                target: ApplicationTarget {
                    kind: ApplicationTargetKind::Executable,
                    value: "/app".into(),
                },
                args: vec![],
                cwd: None,
                run_as_admin: false,
            },
            ResolvedApplicationIdentity {
                canonical_target: "/app".into(),
                identity_digest: "hash".into(),
                resolved_cwd: None,
                user_identity: "owner".into(),
                session_identity: "os-session".into(),
            },
        )
        .unwrap();
        let completion = ComputerActionCompleted {
            work_id: "1".into(),
            action_request_id: "call".into(),
            execution_generation: "dispatch".into(),
            result: ComputerActionResultClass::ChangedButUnverified,
            facts: vec![ComputerActionStepFact {
                index: 0,
                changed: true,
                verified: false,
                summary: "Launch accepted".into(),
            }],
            message: None,
            output: Some(ComputerActionOutput::ApplicationLaunch(
                LaunchApplicationResult {
                    diagnostic: None,
                    dispatch_id: "dispatch".into(),
                    launch_outcome: LaunchOutcome::LaunchAccepted,
                    argument_delivery: ArgumentDelivery::NotRequested,
                    failure_reason: None,
                    requested_admin: false,
                    created_process_id: Some(42),
                    created_process_elevated: Some(false),
                    observations: vec![],
                },
            )),
        };
        (binding, completion)
    }
    #[test]
    fn rejects_fabricated_readiness_privilege_and_other_dispatch_receipts() {
        let (binding, valid) = fixture();
        validate(&binding, &valid).unwrap();
        let mut changed = valid.clone();
        changed.result = ComputerActionResultClass::Verified;
        assert!(validate(&binding, &changed).is_err());
        let mut changed = valid.clone();
        changed.facts[0].verified = true;
        assert!(validate(&binding, &changed).is_err());
        for field in ["dispatch_id", "requested_admin", "created_process_elevated"] {
            let mut json = serde_json::to_value(&valid).unwrap();
            json["output"]["value"][field] = if field == "dispatch_id" {
                serde_json::json!("another-dispatch")
            } else {
                serde_json::json!(true)
            };
            let changed = serde_json::from_value(json).unwrap();
            assert!(validate(&binding, &changed).is_err());
        }
    }
    #[test]
    fn failed_receipt_rejects_unbounded_native_messages() {
        let (binding, mut completion) = fixture();
        completion.result = ComputerActionResultClass::DefinitelyNotStarted;
        completion.facts.clear();
        let Some(ComputerActionOutput::ApplicationLaunch(result)) = &mut completion.output else {
            unreachable!()
        };
        result.launch_outcome = LaunchOutcome::LaunchFailed;
        result.created_process_id = None;
        result.created_process_elevated = None;
        result.diagnostic = Some(
            desk_agent_protocol::native_diagnostic::NativeDiagnostic::new(
                desk_agent_protocol::native_diagnostic::DiagnosticStage::ProcessCreation,
                "spawn",
                "win32",
                Some(740),
                "Elevation required",
            ),
        );
        validate(&binding, &completion).unwrap();
        let Some(ComputerActionOutput::ApplicationLaunch(result)) = &mut completion.output else {
            unreachable!()
        };
        result.diagnostic.as_mut().unwrap().message = "x".repeat(1025);
        assert!(validate(&binding, &completion).is_err());
    }
    #[test]
    fn unknown_cannot_be_relabelled_as_definitely_not_started() {
        let (binding, mut completion) = fixture();
        completion.result = ComputerActionResultClass::OutcomeUnknown;
        completion.facts[0].changed = false;
        let Some(ComputerActionOutput::ApplicationLaunch(output)) = completion.output.as_mut()
        else {
            unreachable!()
        };
        output.launch_outcome = LaunchOutcome::OutcomeUnknown;
        output.created_process_id = None;
        output.created_process_elevated = None;
        validate(&binding, &completion).unwrap();
        completion.result = ComputerActionResultClass::DefinitelyNotStarted;
        assert!(validate(&binding, &completion).is_err());
    }
}
