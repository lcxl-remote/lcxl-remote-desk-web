//! Content-free projections of already authenticated native action facts.

use super::*;
use desk_agent_protocol::{
    application_launch::LaunchOutcome,
    computer_use::{ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass},
};

pub fn provider_dispatch(
    alias: ObservationAlias,
    steps: usize,
    dispatched_at: i64,
    mut submit: impl FnMut(ObservationEvent),
) {
    if !(1..=crate::application_batch::MAX_STEPS).contains(&steps) {
        return;
    }
    emit(
        &mut submit,
        ObservationEvent::deferred_tool(
            alias.clone(),
            ObservationPhase::Dispatched,
            0,
            dispatched_at,
            BTreeMap::from([
                (Stage::Preflight, StageOutcome::Passed),
                (Stage::Permission, StageOutcome::Passed),
                (Stage::Dispatch, StageOutcome::Passed),
            ]),
            PermissionOutcome::Approved,
            Some(InputConclusion::Accepted),
            InputIssue::None,
        ),
    );
    for ordinal in 0..steps {
        emit(
            &mut submit,
            ObservationEvent::deferred_operation(
                alias.clone(),
                ordinal as u32,
                ObservationPhase::Dispatched,
                0,
                dispatched_at,
                dispatched_at,
                OperationSnapshot {
                    tool_observation_id: None,
                    tool_key: None,
                    ordinal: ordinal as u32,
                    dispatched: None,
                    outcome: OperationOutcome::Pending,
                    duration_ms: None,
                },
            ),
        );
    }
}

pub fn provider_completion(
    alias: ObservationAlias,
    steps: usize,
    dispatched_at: i64,
    received_at: i64,
    result: &ComputerActionCompleted,
    batch: Option<crate::application_batch::BatchExecutionObservation>,
    submit: impl FnMut(ObservationEvent),
) {
    completion(
        alias,
        steps,
        Some(dispatched_at),
        received_at,
        result,
        batch,
        submit,
    );
}

pub fn provider_completion_from_dispatch(
    alias: ObservationAlias,
    steps: usize,
    received_at: i64,
    result: &ComputerActionCompleted,
    batch: Option<crate::application_batch::BatchExecutionObservation>,
    submit: impl FnMut(ObservationEvent),
) {
    completion(alias, steps, None, received_at, result, batch, submit);
}

fn completion(
    alias: ObservationAlias,
    steps: usize,
    dispatched_at: Option<i64>,
    received_at: i64,
    result: &ComputerActionCompleted,
    batch: Option<crate::application_batch::BatchExecutionObservation>,
    mut submit: impl FnMut(ObservationEvent),
) {
    if !(1..=crate::application_batch::MAX_STEPS).contains(&steps) {
        return;
    }
    // A later authenticated terminal result may refine an unknown observation.
    // They occupy distinct slots and cannot conflict as two terminal events.
    let phase = if result.result == ComputerActionResultClass::OutcomeUnknown {
        ObservationPhase::Progress
    } else {
        ObservationPhase::Completed
    };
    emit(
        &mut submit,
        ObservationEvent::deferred_tool(
            alias.clone(),
            phase,
            0,
            received_at,
            BTreeMap::from([(Stage::Completion, StageOutcome::Passed)]),
            PermissionOutcome::NotReached,
            None,
            InputIssue::None,
        ),
    );
    for ordinal in 0..steps {
        let (dispatched, outcome) = if steps == 1 {
            single_facts(result)
        } else {
            batch_step(ordinal as u32, batch, result.result)
        };
        let snapshot = OperationSnapshot {
            tool_observation_id: None,
            tool_key: None,
            ordinal: ordinal as u32,
            dispatched,
            outcome,
            duration_ms: None,
        };
        let event = match dispatched_at {
            Some(start) => ObservationEvent::deferred_operation(
                alias.clone(),
                ordinal as u32,
                phase,
                0,
                start,
                received_at,
                snapshot,
            ),
            None => ObservationEvent::deferred_started_operation(
                alias.clone(),
                ordinal as u32,
                phase,
                0,
                received_at,
                snapshot,
            ),
        };
        emit(&mut submit, event);
    }
}

fn emit(submit: &mut impl FnMut(ObservationEvent), event: ObservationEvent) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| submit(event)));
}

pub fn command_dispatch(
    alias: ObservationAlias,
    started_at: i64,
    mut submit: impl FnMut(ObservationEvent),
) {
    emit(
        &mut submit,
        ObservationEvent::deferred_tool(
            alias.clone(),
            ObservationPhase::Dispatched,
            0,
            started_at,
            BTreeMap::from([
                (Stage::Preflight, StageOutcome::Passed),
                (Stage::Permission, StageOutcome::Passed),
                (Stage::Dispatch, StageOutcome::Passed),
            ]),
            PermissionOutcome::Approved,
            Some(InputConclusion::Accepted),
            InputIssue::None,
        ),
    );
    // Socket handoff does not prove that the daemon handed the plan to a worker.
    emit(
        &mut submit,
        ObservationEvent::deferred_operation(
            alias,
            0,
            ObservationPhase::Dispatched,
            0,
            started_at,
            started_at,
            OperationSnapshot {
                tool_observation_id: None,
                tool_key: None,
                ordinal: 0,
                dispatched: None,
                outcome: OperationOutcome::Pending,
                duration_ms: None,
            },
        ),
    );
}

/// A failed send has no operation start. Infrastructure and readiness failures
/// do not become policy denials or model input errors.
pub fn command_not_sent(
    alias: ObservationAlias,
    received_at: i64,
    error: &desk_agent_protocol::AgentError,
    mut submit: impl FnMut(ObservationEvent),
) {
    let permission = command_permission(error);
    let mut stages = BTreeMap::from([(Stage::Dispatch, StageOutcome::Failed)]);
    if permission == PermissionOutcome::PolicyRejected {
        stages.insert(Stage::Permission, StageOutcome::Failed);
    }
    emit(
        &mut submit,
        ObservationEvent::deferred_tool(
            alias,
            ObservationPhase::Completed,
            0,
            received_at,
            stages,
            permission,
            None,
            InputIssue::None,
        ),
    );
}

/// A source-bound host ledger can prove a stop converged before any spawn.
/// This refines an existing transport observation and never creates its start.
pub fn command_cancelled_before_spawn(
    alias: ObservationAlias,
    received_at: i64,
    mut submit: impl FnMut(ObservationEvent),
) {
    emit(
        &mut submit,
        ObservationEvent::deferred_tool(
            alias.clone(),
            ObservationPhase::Completed,
            0,
            received_at,
            BTreeMap::from([(Stage::Completion, StageOutcome::Passed)]),
            PermissionOutcome::NotReached,
            None,
            InputIssue::None,
        ),
    );
    emit(
        &mut submit,
        ObservationEvent::deferred_started_operation(
            alias,
            0,
            ObservationPhase::Completed,
            0,
            received_at,
            OperationSnapshot {
                tool_observation_id: None,
                tool_key: None,
                ordinal: 0,
                dispatched: Some(false),
                outcome: OperationOutcome::Cancelled,
                duration_ms: None,
            },
        ),
    );
}

/// A routing wait cannot prove a worker handoff. Its slot is separate from the
/// connection owner's unknown reply and any later authoritative completion.
pub fn command_route_indeterminate(
    alias: ObservationAlias,
    received_at: i64,
    mut submit: impl FnMut(ObservationEvent),
) {
    emit(
        &mut submit,
        ObservationEvent::deferred_tool(
            alias.clone(),
            ObservationPhase::Progress,
            1,
            received_at,
            BTreeMap::from([(Stage::Completion, StageOutcome::Attempted)]),
            PermissionOutcome::NotReached,
            None,
            InputIssue::None,
        ),
    );
    emit(
        &mut submit,
        ObservationEvent::deferred_started_operation(
            alias,
            0,
            ObservationPhase::Progress,
            1,
            received_at,
            OperationSnapshot {
                tool_observation_id: None,
                tool_key: None,
                ordinal: 0,
                dispatched: None,
                outcome: OperationOutcome::Unknown,
                duration_ms: None,
            },
        ),
    );
}

fn command_permission(error: &desk_agent_protocol::AgentError) -> PermissionOutcome {
    match error.kind {
        desk_agent_protocol::AgentErrorKind::PermissionDenied
        | desk_agent_protocol::AgentErrorKind::RiskBlocked => PermissionOutcome::PolicyRejected,
        _ => PermissionOutcome::NotReached,
    }
}

pub fn command_completion(
    alias: ObservationAlias,
    received_at: i64,
    disposition: &desk_agent_protocol::edge_exec::EdgeExecDisposition,
    submit: impl FnMut(ObservationEvent),
) {
    use desk_agent_protocol::edge_exec::EdgeExecDisposition as Disposition;
    let (dispatched, outcome, duration, permission) = match disposition {
        Disposition::RejectedBeforeDispatch { error } => (
            Some(false),
            OperationOutcome::Rejected,
            None,
            command_permission(error),
        ),
        Disposition::HostAtCapacity { .. } => (
            Some(false),
            OperationOutcome::Rejected,
            None,
            PermissionOutcome::NotReached,
        ),
        Disposition::DispatchFailedBeforeWorker { .. } => (
            Some(false),
            OperationOutcome::Failed,
            None,
            PermissionOutcome::NotReached,
        ),
        Disposition::ExecutionStateUnknown { .. } => (
            None,
            OperationOutcome::Unknown,
            None,
            PermissionOutcome::NotReached,
        ),
        Disposition::Executed { outcome } => executed_facts(outcome),
    };
    let phase = if matches!(disposition, Disposition::ExecutionStateUnknown { .. }) {
        ObservationPhase::Progress
    } else {
        ObservationPhase::Completed
    };
    command_completion_facts(
        alias,
        received_at,
        phase,
        dispatched,
        outcome,
        duration,
        permission,
        submit,
    );
}

/// Borrows the outcome already decoded for authoritative business writeback.
pub fn command_executed(
    alias: ObservationAlias,
    received_at: i64,
    outcome: &desk_agent_protocol::AgentOutcome,
    submit: impl FnMut(ObservationEvent),
) {
    let (dispatched, outcome, duration, permission) = executed_facts(outcome);
    command_completion_facts(
        alias,
        received_at,
        ObservationPhase::Completed,
        dispatched,
        outcome,
        duration,
        permission,
        submit,
    );
}

fn executed_facts(
    outcome: &desk_agent_protocol::AgentOutcome,
) -> (
    Option<bool>,
    OperationOutcome,
    Option<u64>,
    PermissionOutcome,
) {
    use desk_agent_protocol::{AgentOutcome, OperationOutput};
    match outcome {
        AgentOutcome::Ok(OperationOutput::Exec(output)) => {
            let result =
                if !output.started {
                    OperationOutcome::Failed
                } else if output.failure.as_ref().is_some_and(|error| {
                    error.kind == desk_agent_protocol::AgentErrorKind::Cancelled
                }) {
                    OperationOutcome::Cancelled
                } else if output.outcome_unknown() {
                    OperationOutcome::Unknown
                } else if output.succeeded() {
                    OperationOutcome::Accepted
                } else {
                    OperationOutcome::Failed
                };
            (
                Some(output.started),
                result,
                output.started.then_some(u64::from(output.duration_ms)),
                PermissionOutcome::NotReached,
            )
        }
        AgentOutcome::Err(error) => (
            Some(true),
            if error.kind == desk_agent_protocol::AgentErrorKind::Cancelled {
                OperationOutcome::Cancelled
            } else {
                OperationOutcome::Failed
            },
            None,
            PermissionOutcome::NotReached,
        ),
        _ => (
            None,
            OperationOutcome::Unknown,
            None,
            PermissionOutcome::NotReached,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn command_completion_facts(
    alias: ObservationAlias,
    received_at: i64,
    phase: ObservationPhase,
    dispatched: Option<bool>,
    outcome: OperationOutcome,
    duration: Option<u64>,
    permission: PermissionOutcome,
    mut submit: impl FnMut(ObservationEvent),
) {
    let mut stages = BTreeMap::from([(
        Stage::Completion,
        if phase == ObservationPhase::Progress {
            StageOutcome::Attempted
        } else {
            StageOutcome::Passed
        },
    )]);
    if permission == PermissionOutcome::PolicyRejected {
        stages.insert(Stage::Permission, StageOutcome::Failed);
    }
    emit(
        &mut submit,
        ObservationEvent::deferred_tool(
            alias.clone(),
            phase,
            0,
            received_at,
            stages,
            permission,
            None,
            InputIssue::None,
        ),
    );
    emit(
        &mut submit,
        ObservationEvent::deferred_started_operation(
            alias,
            0,
            phase,
            0,
            received_at,
            OperationSnapshot {
                tool_observation_id: None,
                tool_key: None,
                ordinal: 0,
                dispatched,
                outcome,
                duration_ms: duration,
            },
        ),
    );
}

fn single_outcome(result: &ComputerActionCompleted) -> OperationOutcome {
    use ComputerActionResultClass as Class;
    match result.result {
        Class::Verified => OperationOutcome::Verified,
        Class::ChangedButUnverified
            if matches!(&result.output,
            Some(ComputerActionOutput::ApplicationLaunch(receipt)) if receipt.launch_outcome==LaunchOutcome::LaunchAccepted) =>
        {
            OperationOutcome::Accepted
        }
        Class::ChangedButUnverified | Class::PartiallyApplied | Class::RollbackUnsafe => {
            OperationOutcome::ChangedUnverified
        }
        Class::OutcomeUnknown => OperationOutcome::Unknown,
        Class::PausedByUser => OperationOutcome::Cancelled,
        Class::DefinitelyNotStarted | Class::StaleObservation | Class::NotReady => {
            OperationOutcome::Rejected
        }
        Class::Failed => OperationOutcome::Failed,
    }
}

fn single_facts(result: &ComputerActionCompleted) -> (Option<bool>, OperationOutcome) {
    use ComputerActionResultClass as Class;
    let dispatched = match result.result {
        Class::DefinitelyNotStarted | Class::StaleObservation | Class::NotReady => Some(false),
        Class::OutcomeUnknown | Class::PausedByUser => result
            .facts
            .iter()
            .any(|fact| fact.changed || fact.verified)
            .then_some(true),
        Class::Verified
        | Class::ChangedButUnverified
        | Class::PartiallyApplied
        | Class::RollbackUnsafe
        | Class::Failed => Some(true),
    };
    (dispatched, single_outcome(result))
}

fn batch_step(
    ordinal: u32,
    batch: Option<crate::application_batch::BatchExecutionObservation>,
    class: ComputerActionResultClass,
) -> (Option<bool>, OperationOutcome) {
    let Some(batch) = batch else {
        return (None, OperationOutcome::Unknown);
    };
    if batch.completed_steps.is_some_and(|count| ordinal < count) {
        return (Some(true), OperationOutcome::ChangedUnverified);
    }
    if let Some(failed) = batch.failed_step {
        if ordinal + 1 > failed {
            return (Some(false), OperationOutcome::Cancelled);
        }
        if ordinal + 1 == failed {
            return match class {
                ComputerActionResultClass::DefinitelyNotStarted => {
                    (Some(false), OperationOutcome::Rejected)
                }
                ComputerActionResultClass::OutcomeUnknown => {
                    (Some(true), OperationOutcome::Unknown)
                }
                _ => (Some(true), OperationOutcome::Failed),
            };
        }
    }
    (None, OperationOutcome::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn planned_steps_do_not_become_dispatched_operations_without_native_evidence() {
        let mut events = Vec::new();
        provider_dispatch(
            ObservationAlias::provider_work("123").unwrap(),
            3,
            1_000,
            |event| events.push(event),
        );
        assert_eq!(events.len(), 4);
        assert!(
            events
                .iter()
                .filter_map(|event| match &event.payload {
                    ObservationPayload::Operation(value) => Some(value),
                    _ => None,
                })
                .all(|operation| operation.dispatched.is_none())
        );
        let batch = crate::application_batch::BatchExecutionObservation {
            completed_steps: Some(1),
            failed_step: Some(2),
        };
        assert_eq!(
            batch_step(0, Some(batch), ComputerActionResultClass::PartiallyApplied),
            (Some(true), OperationOutcome::ChangedUnverified)
        );
        assert_eq!(
            batch_step(1, Some(batch), ComputerActionResultClass::PartiallyApplied),
            (Some(true), OperationOutcome::Failed)
        );
        assert_eq!(
            batch_step(2, Some(batch), ComputerActionResultClass::PartiallyApplied),
            (Some(false), OperationOutcome::Cancelled)
        );
    }
    #[test]
    fn definitely_unstarted_and_missing_receipt_fields_remain_distinct() {
        let batch = crate::application_batch::BatchExecutionObservation {
            completed_steps: Some(0),
            failed_step: Some(1),
        };
        assert_eq!(
            batch_step(
                0,
                Some(batch),
                ComputerActionResultClass::DefinitelyNotStarted
            ),
            (Some(false), OperationOutcome::Rejected)
        );
        assert_eq!(
            batch_step(
                1,
                Some(batch),
                ComputerActionResultClass::DefinitelyNotStarted
            ),
            (Some(false), OperationOutcome::Cancelled)
        );
        assert_eq!(
            batch_step(0, None, ComputerActionResultClass::Failed),
            (None, OperationOutcome::Unknown)
        );
    }

    #[test]
    fn observer_panics_cannot_abort_already_authorized_dispatch() {
        let mut attempts = 0;
        provider_dispatch(
            ObservationAlias::provider_work("123").unwrap(),
            3,
            1_000,
            |_| {
                attempts += 1;
                panic!("unavailable observation sink");
            },
        );
        assert_eq!(attempts, 4);
    }

    #[test]
    fn unknown_and_terminal_facts_use_distinct_slots_without_native_content() {
        let alias = ObservationAlias::provider_work("123").unwrap();
        let mut native = ComputerActionCompleted {
            work_id: "123".into(),
            action_request_id: "business-secret".into(),
            execution_generation: "private-generation".into(),
            result: ComputerActionResultClass::OutcomeUnknown,
            facts: vec![],
            message: Some("private receipt body".into()),
            output: None,
        };
        let mut events = Vec::new();
        provider_completion_from_dispatch(alias.clone(), 1, 2_000, &native, None, |event| {
            events.push(event)
        });
        native.result = ComputerActionResultClass::Verified;
        provider_completion_from_dispatch(alias, 1, 3_000, &native, None, |event| {
            events.push(event)
        });
        assert_eq!(events.len(), 4);
        assert!(events.iter().all(ObservationEvent::is_bounded));
        assert_eq!(events[0].phase, ObservationPhase::Progress);
        assert_eq!(events[2].phase, ObservationPhase::Completed);
        assert_eq!(events[1].object_id, events[3].object_id);
        assert_ne!(events[1].event_id, events[3].event_id);
        assert!(matches!(
            &events[1].relation,
            Some(ObservationRelation::ResolveStartedOperation(_))
        ));
        assert_eq!(events[1].started_at_ms, 0);
        let encoded = serde_json::to_string(&events).unwrap();
        for value in [
            "business-secret",
            "private-generation",
            "private receipt body",
        ] {
            assert!(!encoded.contains(value));
        }
    }

    fn command_operation(
        disposition: &desk_agent_protocol::edge_exec::EdgeExecDisposition,
    ) -> (OperationSnapshot, Vec<ObservationEvent>) {
        let mut events = Vec::new();
        command_completion(
            ObservationAlias::command_work("77").unwrap(),
            2_000,
            disposition,
            |event| events.push(event),
        );
        assert_eq!(events.len(), 2);
        let ObservationPayload::Operation(operation) = events[1].payload.clone() else {
            panic!("operation required")
        };
        (operation, events)
    }

    #[test]
    fn command_facts_distinguish_pre_worker_rejections_from_execution_and_unknown() {
        use desk_agent_protocol::{AgentErrorKind, edge_exec::EdgeExecDisposition as Disposition};
        let error = || {
            Disposition::safe_error(
                AgentErrorKind::PermissionDenied,
                "private policy body",
                false,
            )
        };
        for disposition in [
            Disposition::RejectedBeforeDispatch { error: error() },
            Disposition::HostAtCapacity { error: error() },
            Disposition::DispatchFailedBeforeWorker { error: error() },
        ] {
            let (operation, events) = command_operation(&disposition);
            assert_eq!(operation.dispatched, Some(false));
            assert!(operation.duration_ms.is_none());
            let ObservationPayload::Tool(tool) = &events[0].payload else {
                panic!("tool required")
            };
            assert_eq!(tool.conclusion, InputConclusion::Unknown);
            assert_eq!(tool.issue, InputIssue::None);
            assert!(
                !serde_json::to_string(&events)
                    .unwrap()
                    .contains("private policy body")
            );
        }
        let (operation, events) = command_operation(&Disposition::ExecutionStateUnknown {
            reason: "private reason".into(),
        });
        assert_eq!(operation.dispatched, None);
        assert_eq!(operation.outcome, OperationOutcome::Unknown);
        assert_eq!(events[1].phase, ObservationPhase::Progress);
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("private reason")
        );
    }

    #[test]
    fn a_definite_not_sent_is_zero_operations_and_only_typed_policy_errors_are_denials() {
        use desk_agent_protocol::{AgentErrorKind, edge_exec::EdgeExecDisposition};
        for (kind, permission) in [
            (
                AgentErrorKind::SessionUnavailable,
                PermissionOutcome::NotReached,
            ),
            (
                AgentErrorKind::UnsupportedCapability,
                PermissionOutcome::NotReached,
            ),
            (AgentErrorKind::Internal, PermissionOutcome::NotReached),
            (
                AgentErrorKind::TransportError,
                PermissionOutcome::NotReached,
            ),
            (
                AgentErrorKind::PermissionDenied,
                PermissionOutcome::PolicyRejected,
            ),
            (
                AgentErrorKind::RiskBlocked,
                PermissionOutcome::PolicyRejected,
            ),
        ] {
            let error = EdgeExecDisposition::safe_error(kind, "private command failure", false);
            let mut events = Vec::new();
            command_not_sent(
                ObservationAlias::command_attempt("77", 1).unwrap(),
                2_000,
                &error,
                |event| events.push(event),
            );
            assert_eq!(events.len(), 1);
            let ObservationPayload::Tool(tool) = &events[0].payload else {
                panic!("tool required")
            };
            assert_eq!(tool.permission, permission);
            assert_eq!(
                tool.stages.get(&Stage::Dispatch),
                Some(&StageOutcome::Failed)
            );
            assert_eq!(tool.conclusion, InputConclusion::Unknown);
            let (_, completed) =
                command_operation(&EdgeExecDisposition::RejectedBeforeDispatch { error });
            let ObservationPayload::Tool(tool) = &completed[0].payload else {
                panic!("tool required")
            };
            assert_eq!(tool.permission, permission);
            assert!(
                !serde_json::to_string(&events)
                    .unwrap()
                    .contains("private command failure")
            );
        }
    }

    #[test]
    fn stopping_a_started_process_and_proven_no_spawn_have_different_denominators() {
        use desk_agent_protocol::{AgentErrorKind, AgentOutcome, edge_exec::EdgeExecDisposition};
        let native = AgentOutcome::Err(EdgeExecDisposition::safe_error(
            AgentErrorKind::Cancelled,
            "private stop receipt",
            false,
        ));
        let mut stopped = Vec::new();
        command_executed(
            ObservationAlias::command_attempt("77", 1).unwrap(),
            2_000,
            &native,
            |event| stopped.push(event),
        );
        let ObservationPayload::Operation(operation) = &stopped[1].payload else {
            panic!("operation required")
        };
        assert_eq!(
            (operation.dispatched, operation.outcome),
            (Some(true), OperationOutcome::Cancelled)
        );
        let mut no_spawn = Vec::new();
        command_cancelled_before_spawn(
            ObservationAlias::command_attempt("77", 2).unwrap(),
            2_000,
            |event| no_spawn.push(event),
        );
        let ObservationPayload::Operation(operation) = &no_spawn[1].payload else {
            panic!("operation required")
        };
        assert_eq!(
            (operation.dispatched, operation.outcome),
            (Some(false), OperationOutcome::Cancelled)
        );
        assert!(matches!(
            &no_spawn[1].relation,
            Some(ObservationRelation::ResolveStartedOperation(_))
        ));
        assert_eq!(no_spawn[1].started_at_ms, 0);
        assert!(
            !serde_json::to_string(&stopped)
                .unwrap()
                .contains("private stop receipt")
        );
    }

    #[test]
    fn successful_process_exit_is_accepted_without_claiming_business_verification() {
        use desk_agent_protocol::{
            AgentOutcome, ExecOutput, ExecOutputStreams, OperationOutput,
            edge_exec::EdgeExecDisposition,
        };
        let mut output = ExecOutput {
            started: true,
            exit_code: Some(0),
            termination_signal: None,
            failure: None,
            diagnostics: vec![],
            streams: ExecOutputStreams::Split {
                stdout: "private stdout".into(),
                stderr: "private stderr".into(),
                stdout_truncated: false,
                stderr_truncated: false,
            },
            duration_ms: 25,
            redactions: vec!["private redaction".into()],
        };
        for (started, exit, outcome) in [
            (true, Some(0), OperationOutcome::Accepted),
            (true, Some(2), OperationOutcome::Failed),
            (true, None, OperationOutcome::Unknown),
            (false, None, OperationOutcome::Failed),
        ] {
            output.started = started;
            output.exit_code = exit;
            let disposition = EdgeExecDisposition::Executed {
                outcome: AgentOutcome::Ok(OperationOutput::Exec(output.clone())),
            };
            let (operation, events) = command_operation(&disposition);
            assert_eq!(operation.dispatched, Some(started));
            assert_eq!(operation.outcome, outcome);
            assert_eq!(operation.duration_ms, started.then_some(25));
            let encoded = serde_json::to_string(&events).unwrap();
            for private in ["private stdout", "private stderr", "private redaction"] {
                assert!(!encoded.contains(private));
            }
        }
        let mut events = Vec::new();
        command_dispatch(
            ObservationAlias::command_work("77").unwrap(),
            1_000,
            |event| events.push(event),
        );
        assert!(matches!(
            &events[1].payload,
            ObservationPayload::Operation(OperationSnapshot {
                dispatched: None,
                outcome: OperationOutcome::Pending,
                ..
            })
        ));
    }

    #[test]
    fn a_single_native_plan_is_not_execution_evidence_and_unstarted_receipt_stays_outside_denominator()
     {
        let mut events = Vec::new();
        let alias = ObservationAlias::provider_work("123").unwrap();
        provider_dispatch(alias.clone(), 1, 1_000, |event| events.push(event));
        let native = ComputerActionCompleted {
            work_id: "123".into(),
            action_request_id: "request".into(),
            execution_generation: "generation".into(),
            result: ComputerActionResultClass::DefinitelyNotStarted,
            facts: vec![],
            message: None,
            output: None,
        };
        provider_completion(alias, 1, 1_000, 2_000, &native, None, |event| {
            events.push(event)
        });
        assert!(matches!(
            &events[1].payload,
            ObservationPayload::Operation(OperationSnapshot {
                dispatched: None,
                outcome: OperationOutcome::Pending,
                ..
            })
        ));
        assert!(matches!(
            &events[3].payload,
            ObservationPayload::Operation(OperationSnapshot {
                dispatched: Some(false),
                outcome: OperationOutcome::Rejected,
                ..
            })
        ));
        let contribution = aggregate::contribution(&events[3].payload, false);
        assert_eq!(
            contribution.get(aggregate::Count::OperationsNotDispatched),
            1
        );
        assert_eq!(
            contribution.rate(aggregate::Rate::ExecutionVerification),
            Some((0, 0))
        );
    }
}
