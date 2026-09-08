//! Public history reasons never carry provider or internal error text.
use desk_agent_protocol::{AgentErrorKind, schedule::management::ScheduledRunIssue};

pub fn public_issue(status: &str, error: Option<&str>) -> Option<ScheduledRunIssue> {
    use ScheduledRunIssue::*;
    if status == "outcome_unknown" {
        return Some(OutcomeUnknown);
    }
    let Some(error) = error else {
        return matches!(status, "failed" | "missed").then_some(Unavailable);
    };
    Some(match error {
        "misfire" => Misfire,
        "device_offline_timeout" => DeviceOfflineTimeout,
        "queue_timeout" => QueueTimeout,
        "budget_policy_exceeded" => BudgetPolicyExceeded,
        "executor_interrupted" => ExecutorInterrupted,
        "outcome_unknown" => OutcomeUnknown,
        value => {
            match serde_json::from_value::<AgentErrorKind>(serde_json::Value::String(value.into()))
            {
                Ok(error) => Agent { error },
                Err(_) => Unavailable,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_stored_codes_without_exporting_arbitrary_error_text() {
        use ScheduledRunIssue::*;
        for (code, expected) in [
            ("misfire", Misfire),
            ("device_offline_timeout", DeviceOfflineTimeout),
            ("queue_timeout", QueueTimeout),
            ("executor_interrupted", ExecutorInterrupted),
            ("outcome_unknown", OutcomeUnknown),
            (
                "permission_denied",
                Agent {
                    error: AgentErrorKind::PermissionDenied,
                },
            ),
            (
                "cancelled",
                Agent {
                    error: AgentErrorKind::Cancelled,
                },
            ),
            ("provider failed with secret credentials", Unavailable),
        ] {
            assert_eq!(public_issue("failed", Some(code)), Some(expected));
        }
        assert_eq!(public_issue("outcome_unknown", None), Some(OutcomeUnknown));
        assert_eq!(
            public_issue("outcome_unknown", Some("timeout")),
            Some(OutcomeUnknown)
        );
        assert_eq!(public_issue("failed", None), Some(Unavailable));
        assert_eq!(public_issue("missed", None), Some(Unavailable));
        for status in [
            "queued",
            "running",
            "succeeded",
            "cancelled",
            "superseded",
            "awaiting_permission",
        ] {
            assert_eq!(public_issue(status, None), None);
        }
    }
}
