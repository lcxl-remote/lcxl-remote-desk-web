//! Execute an already admitted, sealed launch through the durable native journal.
use super::{
    dispatch, execute,
    journal::{InvocationClaim, LaunchJournal},
};
use crate::model::settings::SharedSettings;
use crate::worker::agent::computer_use_broker::ComputerUseBroker;
use desk_agent_protocol::application_launch::{
    LaunchApplicationResult, LaunchApprovalBinding, LaunchFailureReason, LaunchOutcome,
};
use desk_agent_protocol::computer_use::{
    ComputerActionCompleted, ComputerActionKind, ComputerActionOutput, ComputerActionResultClass,
    ComputerActionStepFact, SealedComputerActionPlan,
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

fn journal(root: &Path, binding: &LaunchApprovalBinding) -> std::io::Result<LaunchJournal> {
    let user = format!(
        "{:x}",
        Sha256::digest(binding.identity().user_identity.as_bytes())
    );
    LaunchJournal::open(root.join("application-launch").join(user))
}

/// A duplicate may outlive its worker reference. Return a durable receipt before
/// resolving that reference, and never turn an unresolved invocation into a retry.
pub(crate) fn replay(
    root: Option<&Path>,
    plan: &SealedComputerActionPlan,
) -> Option<ComputerActionCompleted> {
    let ComputerActionKind::LaunchApplication(binding) = &plan.actions.first()?.action else {
        return None;
    };
    let root = root?;
    let existing = journal(root, binding)
        .and_then(|journal| journal.existing(&plan.execution_generation, binding.digest()));
    let result = match existing {
        Ok(Some(InvocationClaim::Recorded(result)))
            if result.dispatch_id == plan.execution_generation =>
        {
            result
        }
        Ok(None) => return None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Ok(Some(InvocationClaim::Cancelled)) => dispatch::receipt(
            binding,
            &plan.execution_generation,
            LaunchOutcome::NotDispatched,
            None,
        ),
        _ => dispatch::receipt(
            binding,
            &plan.execution_generation,
            LaunchOutcome::OutcomeUnknown,
            None,
        ),
    };
    Some(completed(plan.clone(), result))
}

pub(crate) async fn run(
    broker: Arc<ComputerUseBroker>,
    settings: Arc<SharedSettings>,
    data_root: Option<PathBuf>,
    plan: SealedComputerActionPlan,
) -> ComputerActionCompleted {
    let generation = plan.execution_generation.clone();
    let ComputerActionKind::LaunchApplication(binding) = &plan.actions[0].action else {
        unreachable!("typed launch dispatch only")
    };
    let guard_broker = broker.clone();
    let guard_binding = binding.clone();
    let guard_generation = generation.clone();
    let authority = Arc::new(move || {
        guard_broker
            .require_writer_lease(&guard_generation)
            .map_err(|_| LaunchFailureReason::PermissionDenied)?;
        let current = settings
            .0
            .try_read()
            .map_err(|_| LaunchFailureReason::PermissionDenied)?;
        guard_broker
            .preflight_launch(&guard_binding, &current.computer_use)
            .map_err(|_| LaunchFailureReason::PermissionDenied)
    });
    let journal = data_root.and_then(|root| journal(&root, binding).ok());
    let result = if let Some(journal) = journal {
        execute(
            Arc::new(journal),
            generation.clone(),
            binding.clone(),
            binding.subject().clone(),
            authority,
        )
        .await
    } else {
        dispatch::receipt(
            binding,
            &generation,
            LaunchOutcome::NotDispatched,
            Some(LaunchFailureReason::NativeFailure),
        )
    };
    broker.release_writer_lease(&generation);
    completed(plan, result)
}

fn completed(
    plan: SealedComputerActionPlan,
    result: LaunchApplicationResult,
) -> ComputerActionCompleted {
    let generation = plan.execution_generation.clone();
    let class = match result.launch_outcome {
        LaunchOutcome::LaunchAccepted => ComputerActionResultClass::ChangedButUnverified,
        LaunchOutcome::NotDispatched | LaunchOutcome::LaunchFailed => {
            ComputerActionResultClass::DefinitelyNotStarted
        }
        LaunchOutcome::OutcomeUnknown => ComputerActionResultClass::OutcomeUnknown,
    };
    ComputerActionCompleted {
        work_id: plan.work_id,
        action_request_id: plan.action_request_id,
        execution_generation: generation,
        result: class,
        facts: vec![ComputerActionStepFact {
            index: 0,
            changed: result.launch_outcome == LaunchOutcome::LaunchAccepted,
            verified: false,
            summary: format!(
                "Native launch outcome: {:?}; application readiness was not observed",
                result.launch_outcome
            ),
        }],
        message: None,
        output: Some(ComputerActionOutput::ApplicationLaunch(result)),
    }
}
