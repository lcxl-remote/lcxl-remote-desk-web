//! Personal-owner management intents. No request can supply execution authority.
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleManagementRequest {
    ListResumeSources {
        target_device_id: String,
        offset: u32,
        limit: u32,
    },
    ConvertTime {
        input: ScheduleTimeConversion,
    },
    Search {
        after: Option<String>,
        limit: u32,
        kind: Option<ScheduledTaskKind>,
        status: Option<ScheduledTaskStatus>,
        title: Option<String>,
        target_device_id: Option<String>,
        attention_only: bool,
    },
    List {
        after: Option<String>,
        limit: u32,
    },
    Get {
        schedule_id: String,
    },
    /// Decide only the original directory request in one isolated occurrence.
    DecideRunDirectory {
        schedule_id: String,
        run_id: String,
        directory_request_id: String,
        expected_scope_revision: u64,
        approve: bool,
        client_request_key: String,
    },
    RevokeRunDirectory {
        schedule_id: String,
        run_id: String,
        directory_request_id: String,
        expected_scope_revision: u64,
        client_request_key: String,
    },
    DisposeRunOutcome {
        schedule_id: String,
        run_id: String,
        expected_revision: i64,
        client_request_key: String,
        work_id: i64,
        execution_id: String,
        note: String,
    },
    AcknowledgeRunOutcome {
        schedule_id: String,
        run_id: String,
        expected_revision: i64,
        client_request_key: String,
        note: String,
    },
    ResumeTask {
        schedule_id: String,
        expected_revision: i64,
    },
    RunTaskNow {
        schedule_id: String,
        expected_revision: i64,
        client_request_key: String,
    },
    /// Cancellation targets one immutable occurrence and never a later run.
    CancelTaskRun {
        run_id: String,
    },
    RevokeTaskAuthorization {
        schedule_id: String,
        expected_revision: i64,
    },
    GetTaskContract {
        schedule_id: String,
    },
    GenerateTaskContract {
        schedule_id: String,
        expected_revision: i64,
    },
    SaveTaskContract {
        expected_revision: i64,
        contract: Box<contract::TaskContract>,
    },
    /// Explicit owner confirmation of the stored contract and successful guided run.
    PublishTask {
        schedule_id: String,
        expected_revision: i64,
        contract_revision: i64,
        contract_sha256: String,
        rehearsal_run_id: String,
        /// Exclusive authorization expiry as UTC Unix milliseconds.
        expires_at: Option<i64>,
        client_publish_key: String,
    },
    ListRuns {
        schedule_id: String,
        before: Option<String>,
        limit: u32,
    },
    CreateDraft {
        draft: ScheduleDraft,
    },
    ActivateConversationResume {
        schedule_id: String,
        expected_revision: i64,
    },
    ReserveRehearsal {
        schedule_id: String,
        expected_revision: i64,
        client_request_key: String,
    },
    GetRehearsal {
        rehearsal_id: String,
    },
    GetTaskRehearsal {
        schedule_id: String,
    },
    GetRehearsalPermissions {
        rehearsal_id: String,
    },
    CancelPendingRehearsal {
        rehearsal_id: String,
        expected_revision: i64,
    },
    Rename {
        schedule_id: String,
        expected_revision: i64,
        title: String,
    },
    SetFailureThreshold {
        schedule_id: String,
        expected_revision: i64,
        failure_threshold: u32,
    },
    ChangePrompt {
        schedule_id: String,
        expected_revision: i64,
        prompt: String,
    },
    ChangeTime {
        schedule_id: String,
        expected_revision: i64,
        spec: ScheduleSpec,
        time_confirmation: Option<ScheduleTimeConfirmation>,
    },
    Pause {
        schedule_id: String,
        expected_revision: i64,
    },
    Delete {
        schedule_id: String,
        expected_revision: i64,
    },
}

/// Deliberate projection: never serialize a database row or its authority snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleView {
    pub upcoming_runs: Vec<String>,
    pub schedule_id: String,
    /// Manager public device handle or OSS stable device audience; absent when
    /// the original target is no longer accessible. Never a manager database ID.
    pub target_device_id: Option<String>,
    pub kind: ScheduledTaskKind,
    pub title: String,
    pub prompt: String,
    pub status: ScheduledTaskStatus,
    pub revision: i64,
    pub spec: ScheduleSpec,
    pub next_run_at: Option<String>,
    pub active_run_id: Option<String>,
    pub pause_reasons: Vec<SchedulePauseReason>,
    pub consecutive_failures: u32,
    pub failure_threshold: u32,
    pub created_at: String,
    pub updated_at: String,
}

/// Only public conversation intent and frozen task text are exposed to the owner.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RehearsalView {
    pub rehearsal_id: String,
    pub schedule_id: String,
    pub task_revision: i64,
    pub target_device_id: Option<String>,
    pub client_conversation_id: String,
    pub initial_message_id: String,
    pub prompt: String,
    pub locale: Option<String>,
    pub model_id: Option<i32>,
    pub status: RehearsalStatus,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalStatus {
    Failed,
    Pending,
    Running,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleManagementResponse {
    ResumeSources {
        sources: Vec<ResumeConversationSource>,
        next_offset: Option<u32>,
    },
    TaskContract {
        authorization: Option<TaskAuthorizationView>,
        previous_contract: Option<Box<contract::TaskContract>>,
        task: Box<ScheduleView>,
        task_revision: u64,
        prompt_sha256: String,
        /// Public device reference; the opaque digest binds the server's stored contract.
        contract: Option<Box<contract::TaskContract>>,
        contract_sha256: Option<String>,
    },
    Runs {
        schedule_id: String,
        runs: Vec<ScheduledRunView>,
        next_cursor: Option<String>,
    },
    TaskRehearsal {
        task: Box<ScheduleView>,
        rehearsal: Option<RehearsalView>,
    },
    RehearsalPermissions {
        rehearsal_id: String,
        observations: Vec<RehearsalPermissionObservation>,
        unconfirmed_tool_call_ids: Vec<String>,
        unclassified_tool_call_ids: Vec<String>,
    },
    Rehearsal {
        task: Box<ScheduleView>,
        rehearsal: RehearsalView,
    },
    ConvertedTime {
        conversion: ScheduleTimeConverted,
        upcoming_runs: Vec<String>,
    },
    SearchResults {
        tasks: Vec<ScheduleView>,
        next_cursor: Option<String>,
        #[serde(rename = "total")]
        total_count: u64,
        attention_count: u64,
    },
    List {
        tasks: Vec<ScheduleView>,
        next_cursor: Option<String>,
    },
    Task {
        task: ScheduleView,
    },
}

/// Public occurrence metadata. Internal dispatch, session and authority keys are omitted.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduledRunView {
    pub run_id: String,
    pub status: ScheduledRunStatus,
    pub source: ScheduledRunSource,
    pub scheduled_at: Option<String>,
    pub requested_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// Receipt recovery is historical evidence, not successful task completion or resumption.
    pub receipts_reconciled_at: Option<String>,
    pub outcome_reviewed_at: Option<String>,
    pub cancel_requested_at: Option<String>,
    pub missed_count: i64,
    pub issue: Option<ScheduledRunIssue>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledRunSource {
    Calendar,
    Manual,
}

/// Closed public reason categories; provider messages and internal error text stay private.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduledRunIssue {
    Agent { error: crate::AgentErrorKind },
    Misfire,
    DeviceOfflineTimeout,
    QueueTimeout,
    BudgetPolicyExceeded,
    ExecutorInterrupted,
    OutcomeUnknown,
    Unavailable,
}

/// Historical successful scope, not a grant or a statement of future necessity.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RehearsalPermissionObservation {
    pub tool_call_id: String,
    pub provider_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub tool_schema_version: u16,
    pub effect: crate::capability_provider::CapabilityEffect,
    pub risk_tier: crate::capability_grant::CapabilityRiskTier,
    pub resources: Vec<String>,
    pub operations: Vec<String>,
    pub export_destinations: Vec<crate::data_lineage::DestinationIdentity>,
    pub approval_source: RehearsalApprovalSource,
    pub completed_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RehearsalApprovalSource {
    PolicyAuto,
    UserDecision,
}

#[cfg(test)]
mod tests {
    #[test]
    fn run_history_wire_round_trip() {
        let value = serde_json::json!({"result":"runs", "schedule_id":"task", "runs":[{
            "run_id":"run", "status":"outcome_unknown", "source":"calendar",
            "scheduled_at":"2026-10-01T12:00:00.000Z", "requested_at":"2026-10-01T12:00:00.000Z",
            "started_at":null, "finished_at":null, "cancel_requested_at":null, "receipts_reconciled_at":null, "outcome_reviewed_at":null, "missed_count":0, "issue":{"kind":"outcome_unknown"}
        }], "next_cursor":"run"});
        for issue in [
            serde_json::Value::Null,
            serde_json::json!({"kind":"outcome_unknown"}),
            serde_json::json!({"kind":"agent", "error":"timeout"}),
            serde_json::json!({"kind":"unavailable"}),
        ] {
            let mut value = value.clone();
            value["runs"][0]["issue"] = issue;
            let response: ScheduleManagementResponse =
                serde_json::from_value(value.clone()).unwrap();
            let wire = wincode::serialize(&response).unwrap();
            let decoded: ScheduleManagementResponse = wincode::deserialize(&wire).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        }
    }

    use super::*;

    #[test]
    fn publication_confirmation_round_trips_and_rejects_client_authority() {
        let value = serde_json::json!({
            "operation": "publish_task", "schedule_id": "task", "expected_revision": 3,
            "contract_revision": 1, "contract_sha256": "a".repeat(64),
            "rehearsal_run_id": "guided-run", "expires_at": null,
            "client_publish_key": "confirmation"
        });
        let request: ScheduleManagementRequest = serde_json::from_value(value.clone()).unwrap();
        let wire = wincode::serialize(&request).unwrap();
        let decoded: ScheduleManagementRequest = wincode::deserialize(&wire).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        let mut forged = value;
        forged["authorization_revision"] = serde_json::json!(1);
        assert!(serde_json::from_value::<ScheduleManagementRequest>(forged).is_err());
    }

    #[test]
    fn rehearsal_response_round_trips_without_an_extra_task_wrapper() {
        let value = serde_json::json!({
            "result": "rehearsal",
            "task": {
                "schedule_id": "schedule", "target_device_id": "public-device",
                "kind": "fresh_task", "title": "Task", "prompt": "Review status",
                "status": "rehearsing", "revision": 2,
                "spec": {"schema_version": 1, "rule": {"kind": "once", "at": "2026-10-01T12:00:00Z"}},
                "upcoming_runs": [], "next_run_at": null, "active_run_id": null, "pause_reasons": [], "consecutive_failures": 0, "failure_threshold": 3,
                "created_at": "2026-09-01T00:00:00Z", "updated_at": "2026-09-01T00:00:00Z"
            },
            "rehearsal": {
                "rehearsal_id": "rehearsal", "schedule_id": "schedule", "task_revision": 1,
                "target_device_id": "public-device", "client_conversation_id": "rehearsal_client",
                "initial_message_id": "rehearsal:rehearsal:input", "prompt": "Review status",
                "locale": null, "model_id": 3, "status": "pending",
                "started_at": null, "finished_at": null
            }
        });
        let response: ScheduleManagementResponse = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&response).unwrap(), value);
        let wire = wincode::serialize(&response).unwrap();
        let decoded: ScheduleManagementResponse = wincode::deserialize(&wire).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    }
}

/// Owner-visible selection metadata, never a raw storage key or execution grant.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResumeConversationSource {
    pub client_conversation_id: String,
    pub target_device_id: String,
    pub title: String,
    pub requirement_revision: u64,
}

/// Historical owner approval metadata; current dispatch policy is always rechecked.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskAuthorizationView {
    pub authorization_revision: u64,
    pub task_revision: u64,
    pub contract_revision: u64,
    pub approved_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
}
