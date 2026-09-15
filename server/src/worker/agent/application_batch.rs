//! Sequential native dispatch with a compact first-failure receipt.
use desk_agent_protocol::AgentError;
use desk_agent_protocol::computer_use::{
    ComputerActionKind, ComputerActionResultClass, ComputerActionStep,
};
use serde_json::json;

pub(crate) fn supports(steps: &[ComputerActionStep]) -> bool {
    !steps.is_empty()
        && steps.iter().all(|step| {
            matches!(
                step.action,
                ComputerActionKind::UiInApplication { .. }
                    | ComputerActionKind::BackgroundInput { .. }
            )
        })
}

pub(crate) fn execute(
    steps: &[ComputerActionStep],
    mut action: impl FnMut(&ComputerActionStep) -> Result<Option<serde_json::Value>, (AgentError, bool)>,
) -> (ComputerActionResultClass, String) {
    let mut last_scroll = None;
    for (index, step) in steps.iter().enumerate() {
        let result = action(step);
        if let Ok(Some(receipt)) = &result {
            last_scroll = Some(receipt.clone());
        }
        if let Err((error, may_have_effect)) = result {
            tracing::warn!(step_number=index+1,total_steps=steps.len(),error_kind=?error.kind,may_have_effect,"application batch stopped at step");
            let class = if may_have_effect {
                ComputerActionResultClass::OutcomeUnknown
            } else if index == 0 {
                ComputerActionResultClass::DefinitelyNotStarted
            } else {
                ComputerActionResultClass::PartiallyApplied
            };
            return (class, json!({
                "status":"stopped_on_error", "failed_step_number":index+1,
                "completed_steps":index,
                "effect":if may_have_effect {"may_have_effect"} else {"no_effect"},
                "error":{"kind":error.kind,"code":error.error_code,"message":error.message},
                "recovery":"Earlier steps completed native dispatch; later steps were not executed. Read the current UI/window screenshot before replanning unfinished work. Do not replay the whole batch. Other authorized writes remain available; no user acknowledgement is required."
            }).to_string());
        }
    }
    let mut receipt = json!({"status":"completed","completed_steps":steps.len(),"next":"Read the target UI/window screenshot and compare identifiable content before claiming success. If unchanged, reconsider the target position before increasing scroll distance."});
    if let Some(scroll) = last_scroll {
        receipt["last_scroll"] = scroll;
    }
    (
        ComputerActionResultClass::ChangedButUnverified,
        receipt.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentErrorKind, computer_use::*};
    fn steps() -> Vec<ComputerActionStep> {
        let app = ObjectRef {
            token: "app".into(),
            snapshot_id: "native".into(),
            object_kind: ObjectKind::Application,
            expires_at: "".into(),
        };
        (0..3)
            .map(|n| ComputerActionStep {
                target: ObjectRef {
                    token: format!("control-{n}"),
                    object_kind: ObjectKind::UiElement,
                    ..app.clone()
                },
                action: ComputerActionKind::UiInApplication {
                    application: app.clone(),
                    action: UiSemanticAction::Invoke,
                },
                before_summary: "observed".into(),
                after_intent: "invoke".into(),
                verification: "read later".into(),
            })
            .collect()
    }
    #[test]
    fn only_last_dispatched_scroll_is_returned_without_step_history() {
        let mut index = 0;
        let (_, message) = execute(&steps(), |_| {
            index += 1;
            Ok(if index < 3 {
                Some(
                    json!({"position":{"x":index * 100,"y":300},"horizontal_pixels":0,"vertical_pixels":-400}),
                )
            } else {
                None
            })
        });
        let value: serde_json::Value = serde_json::from_str(&message).unwrap();
        assert_eq!(value["last_scroll"]["position"]["x"], 200);
        assert_eq!(value["completed_steps"], 3);
        assert!(value.get("application_state_verified").is_none());
        assert!(value.get("steps").is_none());
    }
    #[test]
    fn successful_batch_has_only_count_and_no_steps() {
        let mut calls = 0;
        let (_, message) = execute(&steps(), |_| {
            calls += 1;
            Ok(None)
        });
        let value: serde_json::Value = serde_json::from_str(&message).unwrap();
        assert_eq!(calls, 3);
        assert_eq!(value["completed_steps"], 3);
        assert!(value.get("steps").is_none());
    }
    #[test]
    fn first_failure_stops_remaining_steps_without_replay() {
        for may_have_effect in [false, true] {
            let mut calls = 0;
            let (class, message) = execute(&steps(), |_| {
                calls += 1;
                if calls == 2 {
                    Err((
                        AgentError {
                            kind: AgentErrorKind::TargetOffline,
                            message: "window closed".into(),
                            retryable: true,
                            safe_for_model: true,
                            error_code: None,
                        },
                        may_have_effect,
                    ))
                } else {
                    Ok(None)
                }
            });
            let value: serde_json::Value = serde_json::from_str(&message).unwrap();
            assert_eq!(calls, 2);
            assert_eq!(
                class,
                if may_have_effect {
                    ComputerActionResultClass::OutcomeUnknown
                } else {
                    ComputerActionResultClass::PartiallyApplied
                }
            );
            assert_eq!(value["completed_steps"], 1);
            assert_eq!(value["failed_step_number"], 2);
            assert_eq!(value["error"]["message"], "window closed");
            assert!(value.get("steps").is_none());
        }
    }

    #[test]
    fn first_step_rejection_has_no_completed_dispatches() {
        let (class, message) = execute(&steps(), |_| {
            Err((
                AgentError {
                    kind: AgentErrorKind::Cancelled,
                    message: "lease revoked".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                },
                false,
            ))
        });
        assert_eq!(class, ComputerActionResultClass::DefinitelyNotStarted);
        let value: serde_json::Value = serde_json::from_str(&message).unwrap();
        assert_eq!(value["completed_steps"], 0);
    }

    #[test]
    fn native_failures_are_accepted_by_the_shared_center_projection() {
        let steps = steps();
        for failed_step in 1..=steps.len() {
            for may_have_effect in [false, true] {
                let mut calls = 0;
                let (class, message) = execute(&steps, |_| {
                    calls += 1;
                    if calls == failed_step {
                        Err((
                            AgentError {
                                kind: AgentErrorKind::Cancelled,
                                message: "native dispatch stopped".into(),
                                retryable: false,
                                safe_for_model: true,
                                error_code: None,
                            },
                            may_have_effect,
                        ))
                    } else {
                        Ok(None)
                    }
                });
                let completed = ComputerActionCompleted {
                    work_id: "work".into(),
                    action_request_id: "action".into(),
                    execution_generation: "generation".into(),
                    result: class,
                    facts: vec![],
                    output: None,
                    message: Some(message.clone()),
                };
                let (failed, receipt) = desk_diagnose_core::application_batch::completion_receipt(
                    &completed,
                    Some(steps.len()),
                )
                .unwrap()
                .unwrap();
                assert!(failed);
                assert_eq!(calls, failed_step);
                assert_eq!(receipt, message);
            }
        }
    }
}
