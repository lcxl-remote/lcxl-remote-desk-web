//! One native submission across duplicate delivery, cancellation and worker restart.
use super::journal::{InvocationClaim, LaunchJournal};
use desk_agent_protocol::application_launch::LaunchError;
use desk_agent_protocol::application_launch::{
    ArgumentDelivery, LaunchApplicationResult, LaunchFailureReason, LaunchOutcome,
};
use desk_agent_protocol::native_diagnostic::{DiagnosticStage, NativeDiagnostic};
use desk_diagnose_core::application_launch::{
    LaunchApprovalBinding, LaunchApprovalSubject, ResolvedApplicationIdentity,
};
use std::future::Future;

pub(super) fn receipt(
    binding: &LaunchApprovalBinding,
    dispatch_id: &str,
    outcome: LaunchOutcome,
    reason: Option<LaunchFailureReason>,
) -> LaunchApplicationResult {
    LaunchApplicationResult {
        diagnostic: reason.map(|reason| {
            let mut diagnostic = LaunchError::from(reason).diagnostic;
            if reason == LaunchFailureReason::PermissionDenied {
                diagnostic.stage = desk_agent_protocol::native_diagnostic::DiagnosticStage::Authorization;
                diagnostic.operation = "verify launch authorization".into();
                diagnostic.message = "Launch authorization or its binding is no longer valid; no native target launch was submitted.".into();
            }
            diagnostic
        }),
        dispatch_id: dispatch_id.into(),
        launch_outcome: outcome,
        argument_delivery: if binding.request().args.is_empty() {
            ArgumentDelivery::NotRequested
        } else {
            ArgumentDelivery::Unknown
        },
        failure_reason: reason,
        requested_admin: binding.request().run_as_admin,
        created_process_id: None,
        created_process_elevated: None,
        observations: vec![],
    }
}

fn journal_error(
    binding: &LaunchApprovalBinding,
    dispatch_id: &str,
    operation: &str,
    error: &std::io::Error,
) -> LaunchApplicationResult {
    let mut result = receipt(binding, dispatch_id, LaunchOutcome::OutcomeUnknown, None);
    result.diagnostic = Some(NativeDiagnostic::from_io(
        DiagnosticStage::ProcessWait,
        operation,
        error,
    ));
    result
}

/// The caller supplies current worker authorization checks, never model verdicts.
/// Preparation resolves the target independently and cannot launch an application.
pub(crate) async fn dispatch<P, Prepare, Invoke>(
    journal: &LaunchJournal,
    dispatch_id: &str,
    binding: &LaunchApprovalBinding,
    subject: &LaunchApprovalSubject,
    mut check_authority: impl FnMut() -> Result<(), LaunchFailureReason>,
    prepare: impl FnOnce() -> Prepare,
    invoke: impl FnOnce(P, String) -> Invoke,
) -> LaunchApplicationResult
where
    Prepare: Future<Output = Result<(P, ResolvedApplicationIdentity), LaunchError>>,
    Invoke: Future<Output = LaunchApplicationResult>,
{
    // A deserialized binding must reproduce its own digest as well as the current
    // centrally stamped subject before it can touch a durable dispatch record.
    if binding
        .revalidate(subject, binding.request(), binding.identity())
        .is_err()
    {
        return receipt(
            binding,
            dispatch_id,
            LaunchOutcome::NotDispatched,
            Some(LaunchFailureReason::PermissionDenied),
        );
    }
    if let Err(error) = journal.prepare(dispatch_id, binding.digest()) {
        return journal_error(
            binding,
            dispatch_id,
            "prepare durable launch record",
            &error,
        );
    }
    match journal.existing(dispatch_id, binding.digest()) {
        Ok(Some(InvocationClaim::Recorded(result))) if result.dispatch_id == dispatch_id => {
            return result;
        }
        Ok(Some(InvocationClaim::Cancelled)) => {
            return receipt(binding, dispatch_id, LaunchOutcome::NotDispatched, None);
        }
        Ok(None) => {}
        Err(error) => {
            return journal_error(binding, dispatch_id, "read durable launch record", &error);
        }
        _ => return receipt(binding, dispatch_id, LaunchOutcome::OutcomeUnknown, None),
    }
    if let Err(reason) = check_authority() {
        return match journal.cancel(dispatch_id, binding.digest()) {
            Ok(InvocationClaim::Cancelled) => receipt(
                binding,
                dispatch_id,
                LaunchOutcome::NotDispatched,
                Some(reason),
            ),
            Ok(InvocationClaim::Recorded(result)) => result,
            Err(error) => {
                journal_error(binding, dispatch_id, "cancel durable launch record", &error)
            }
            _ => receipt(binding, dispatch_id, LaunchOutcome::OutcomeUnknown, None),
        };
    }
    let (prepared, identity) = match prepare().await {
        Ok(value) => value,
        Err(reason) => {
            let mut result = receipt(
                binding,
                dispatch_id,
                LaunchOutcome::NotDispatched,
                Some(reason.reason),
            );
            result.diagnostic = Some(reason.diagnostic);
            return result;
        }
    };
    if binding
        .revalidate(subject, binding.request(), &identity)
        .is_err()
    {
        return receipt(
            binding,
            dispatch_id,
            LaunchOutcome::NotDispatched,
            Some(LaunchFailureReason::IdentityChanged),
        );
    }
    match journal.claim(dispatch_id, binding.digest()) {
        Ok(InvocationClaim::Recorded(result)) => return result,
        Ok(InvocationClaim::Cancelled) => {
            return receipt(binding, dispatch_id, LaunchOutcome::NotDispatched, None);
        }
        Err(error) => {
            return journal_error(binding, dispatch_id, "claim durable launch record", &error);
        }
        Ok(InvocationClaim::OutcomeUnknown) => {
            return receipt(binding, dispatch_id, LaunchOutcome::OutcomeUnknown, None);
        }
        Ok(InvocationClaim::Invoke) => {}
    }
    let result = match check_authority() {
        Ok(()) => invoke(prepared, dispatch_id.into()).await,
        Err(reason) => receipt(
            binding,
            dispatch_id,
            LaunchOutcome::NotDispatched,
            Some(reason),
        ),
    };
    // A native receipt cannot be projected as durably resolved unless its exact
    // dispatch result survived persistence. Losing the write is an unknown outcome.
    if result.dispatch_id != dispatch_id {
        let mut unknown = receipt(binding, dispatch_id, LaunchOutcome::OutcomeUnknown, None);
        unknown.diagnostic = Some(NativeDiagnostic::new(
            DiagnosticStage::HelperCommunication,
            "validate launch receipt",
            "application",
            None,
            "Received a receipt for a different dispatch; native outcome is unknown",
        ));
        return unknown;
    }
    if let Err(error) = journal.record(dispatch_id, binding.digest(), &result) {
        return journal_error(
            binding,
            dispatch_id,
            "persist durable launch receipt",
            &error,
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::application_launch::{
        ApplicationTarget, ApplicationTargetKind, LaunchApplicationRequest,
    };
    use std::cell::Cell;

    fn binding() -> LaunchApprovalBinding {
        LaunchApprovalBinding::prepare(
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
                    value: "/application".into(),
                },
                args: vec![],
                cwd: None,
                run_as_admin: false,
            },
            ResolvedApplicationIdentity {
                canonical_target: "/application".into(),
                identity_digest: "file-identity".into(),
                resolved_cwd: Some("/home/user".into()),
                user_identity: "user".into(),
                session_identity: "session".into(),
            },
        )
        .unwrap()
    }
    #[tokio::test]
    async fn duplicate_dispatch_and_restart_invoke_native_only_once() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("application-dispatch-{}", uuid::Uuid::new_v4()));
        let journal = LaunchJournal::open(root.clone()).unwrap();
        let binding = binding();
        let calls = Cell::new(0);
        for _ in 0..2 {
            let result = dispatch(
                &journal,
                "dispatch",
                &binding,
                binding.subject(),
                || Ok(()),
                || async { Ok(((), binding.identity().clone())) },
                |(), id| {
                    calls.set(calls.get() + 1);
                    let result = receipt(&binding, &id, LaunchOutcome::LaunchAccepted, None);
                    async move { result }
                },
            )
            .await;
            assert_eq!(result.launch_outcome, LaunchOutcome::LaunchAccepted);
        }
        assert_eq!(calls.get(), 1);
        let reopened = LaunchJournal::open(root.clone()).unwrap();
        assert!(matches!(
            reopened.claim("dispatch", binding.digest()).unwrap(),
            InvocationClaim::Recorded(_)
        ));
        drop(reopened);
        drop(journal);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn cancelled_before_preparation_cannot_restart_after_authority_returns() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("application-dispatch-{}", uuid::Uuid::new_v4()));
        let journal = LaunchJournal::open(root.clone()).unwrap();
        let binding = binding();
        let preparations = Cell::new(0);
        for permitted in [false, true] {
            let result = dispatch(
                &journal,
                "dispatch",
                &binding,
                binding.subject(),
                || {
                    if permitted {
                        Ok(())
                    } else {
                        Err(LaunchFailureReason::PermissionDenied)
                    }
                },
                || async {
                    preparations.set(preparations.get() + 1);
                    Ok(((), binding.identity().clone()))
                },
                |(), _| async { panic!("cancelled launch reached native API") },
            )
            .await;
            assert_eq!(result.launch_outcome, LaunchOutcome::NotDispatched);
        }
        assert_eq!(preparations.get(), 0);
        drop(journal);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn revoked_authority_after_preparation_never_calls_native() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("application-dispatch-{}", uuid::Uuid::new_v4()));
        let journal = LaunchJournal::open(root.clone()).unwrap();
        let binding = binding();
        let checks = Cell::new(0);
        let result = dispatch(
            &journal,
            "dispatch",
            &binding,
            binding.subject(),
            || {
                checks.set(checks.get() + 1);
                if checks.get() == 1 {
                    Ok(())
                } else {
                    Err(LaunchFailureReason::PermissionDenied)
                }
            },
            || async { Ok(((), binding.identity().clone())) },
            |(), _| async { panic!("revoked launch reached native API") },
        )
        .await;
        assert_eq!(result.launch_outcome, LaunchOutcome::NotDispatched);
        drop(journal);
        std::fs::remove_dir_all(root).unwrap();
    }
}
