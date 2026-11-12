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
        source_conversation_id: Option<String>,
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
    ListRuns {
        schedule_id: String,
        before: Option<String>,
        limit: u32,
    },
    /// Owner creation. The task is enabled immediately; there is no draft state.
    CreateTask {
        task: ScheduleDraft,
    },
    /// Decide a pending AI proposal. Approval enables the task as proposed.
    DecideScheduleProposal {
        schedule_id: String,
        expected_revision: i64,
        approve: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
// One wire response per request; boxing the task view would only add indirection.
#[allow(clippy::large_enum_variant)]
pub enum ScheduleManagementResponse {
    ResumeSources {
        sources: Vec<ResumeConversationSource>,
        next_offset: Option<u32>,
    },
    Runs {
        schedule_id: String,
        runs: Vec<ScheduledRunView>,
        next_cursor: Option<String>,
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
    ExecutorInterrupted,
    OutcomeUnknown,
    Unavailable,
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
    fn proposal_decision_round_trips_and_rejects_extra_authority() {
        let value = serde_json::json!({
            "operation": "decide_schedule_proposal", "schedule_id": "task",
            "expected_revision": 3, "approve": true
        });
        let request: ScheduleManagementRequest = serde_json::from_value(value.clone()).unwrap();
        let wire = wincode::serialize(&request).unwrap();
        let decoded: ScheduleManagementRequest = wincode::deserialize(&wire).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        let mut forged = value;
        forged["decided_by"] = serde_json::json!("ai");
        assert!(serde_json::from_value::<ScheduleManagementRequest>(forged).is_err());
    }
}

/// Owner-visible original session identity, never an execution grant.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResumeConversationSource {
    pub conversation_id: String,
    pub target_device_id: String,
    pub title: String,
    pub requirement_revision: u64,
}
