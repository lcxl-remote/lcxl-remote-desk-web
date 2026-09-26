//! An admitted output operation retains its writer through native cleanup.
use crate::{
    model::settings::SharedSettings,
    worker::agent::{
        computer_use_broker::ComputerUseBroker, computer_use_writer::retain_writer_lease,
        linux_desktop,
    },
};
use desk_agent_protocol::computer_use::{
    ComputerActionCompleted, ComputerActionKind, ComputerActionResultClass,
    SealedComputerActionPlan,
};
use desk_wayland_portal::{PortalError, PortalInputFailure, PortalInputReceipt};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

fn rejected(message: impl Into<String>) -> PortalInputFailure {
    PortalInputFailure {
        error: PortalError::Backend(message.into()),
        possibly_started: false,
    }
}

pub(crate) async fn execute(
    broker: Arc<ComputerUseBroker>,
    settings: Arc<SharedSettings>,
    plan: SealedComputerActionPlan,
    control_generation: Option<u64>,
) -> ComputerActionCompleted {
    let lease = retain_writer_lease(broker.clone(), plan.execution_generation.clone());
    let result = async {
        broker
            .require_linux_input_control(control_generation)
            .map_err(|error| rejected(error.message))?;
        let [step] = plan.actions.as_slice() else {
            return Err(rejected("Exactly one output action is required"));
        };
        let ComputerActionKind::WaylandOutputInput(action) = &step.action else {
            return Err(rejected("Expected an output input action"));
        };
        let initial = settings.read().await;
        let ceiling = initial.computer_use.clone();
        if !initial.collection_policy.allow_screen {
            return Err(rejected("Screen observation is disabled"));
        }
        drop(initial);
        broker
            .preflight_wayland_output(&step.target, action, &ceiling)
            .map_err(|error| rejected(error.message))?;
        let frame = broker
            .resolve_wayland_output(&step.target, action)
            .map_err(|error| rejected(error.message))?;
        let identity = linux_desktop::resolve()
            .await
            .map_err(|error| rejected(error.to_string()))?;
        if identity.binding() != frame.identity {
            return Err(rejected("Desktop identity changed"));
        }
        let portal = broker
            .portal()
            .ok_or_else(|| rejected("Portal unavailable"))?;
        let (session, sender) = portal
            .try_borrow_input()
            .map_err(|error| rejected(error.to_string()))?;
        if !Arc::ptr_eq(&session, &frame.session) {
            return Err(rejected("Portal session changed"));
        }
        let events = super::events(
            action,
            session
                .stream()
                .size
                .ok_or_else(|| rejected("Stream geometry is missing"))?,
        )
        .map_err(|error| rejected(error.to_string()))?;
        let target = step.target.clone();
        let action = action.clone();
        let generation = plan.execution_generation.clone();
        sender
            .submit_guarded_batch(
                events,
                Duration::from_millis(u64::from(plan.timeout_ms)),
                CancellationToken::new(),
                move || {
                    // Stored by the queue, not the async waiter. Even an abandoned
                    // receipt retains ownership until dispatch/cleanup actually ends.
                    let _lease = &lease;
                    broker
                        .require_linux_input_control(control_generation)
                        .map_err(|error| PortalError::Backend(error.message))?;
                    broker
                        .require_writer_lease(&generation)
                        .map_err(|error| PortalError::Backend(error.message))?;
                    let current = settings
                        .try_read()
                        .map_err(|_| PortalError::Backend("Settings update in progress".into()))?;
                    if current.computer_use != ceiling || !current.collection_policy.allow_screen {
                        return Err(PortalError::Backend("Device policy changed".into()));
                    }
                    broker
                        .preflight_wayland_output(&target, &action, &current.computer_use)
                        .map_err(|error| PortalError::Backend(error.message))
                },
            )
            .await
    }
    .await;
    let (result_class, message) = classify(result);
    ComputerActionCompleted {
        work_id: plan.work_id,
        action_request_id: plan.action_request_id,
        execution_generation: plan.execution_generation,
        result: result_class,
        facts: Vec::new(),
        message: Some(message),
        output: None,
    }
}

fn classify(
    result: Result<PortalInputReceipt, PortalInputFailure>,
) -> (ComputerActionResultClass, String) {
    match result {
        // Mutter replies after queueing virtual input and can acknowledge an
        // absolute motion it dropped before stream coordinates were ready.
        // Neither dispatch completion nor a changed UI is proven by this reply.
        Ok(_) => (
            ComputerActionResultClass::OutcomeUnknown,
            "Portal acknowledged the input request; processing and effects are unknown. Observe current state; do not replay automatically".into(),
        ),
        Err(failure) => (
            if failure.possibly_started {
                ComputerActionResultClass::OutcomeUnknown
            } else {
                ComputerActionResultClass::DefinitelyNotStarted
            },
            failure.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_portal_reply_is_not_evidence_of_a_changed_desktop() {
        let (result, message) = classify(Ok(PortalInputReceipt {
            completed_at: std::time::Instant::now(),
        }));
        assert_eq!(result, ComputerActionResultClass::OutcomeUnknown);
        assert!(message.contains("do not replay automatically"));
    }

    #[test]
    fn only_a_failure_before_submission_proves_not_started() {
        for possibly_started in [false, true] {
            let (result, _) = classify(Err(PortalInputFailure {
                error: PortalError::Backend("fixture failure".into()),
                possibly_started,
            }));
            assert_eq!(
                result,
                if possibly_started {
                    ComputerActionResultClass::OutcomeUnknown
                } else {
                    ComputerActionResultClass::DefinitelyNotStarted
                }
            );
        }
    }
}
