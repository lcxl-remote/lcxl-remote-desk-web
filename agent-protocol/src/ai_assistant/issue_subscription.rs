//! Personal workbench issue subscription settings and intake status.

use crate::external_tools::IssuePlatform;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueWorkKind {
    Analysis,
    DeviceInvestigation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssueSubscriptionSave {
    /// Omit for a new subscription.
    pub subscription_id: Option<String>,
    pub expected_revision: u64,
    pub workbench_id: String,
    pub connection_ref: String,
    pub owner: String,
    pub repository: String,
    pub issue_number: Option<u64>,
    pub allowed_sender_ids: Vec<u64>,
    pub automation_sender_ids: Vec<u64>,
    pub work_kind: IssueWorkKind,
    pub auto_reply: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssueSubscriptionEnable {
    pub expected_revision: u64,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IssueSubscriptionView {
    pub subscription_id: String,
    pub revision: u64,
    pub workbench_id: String,
    pub connection_ref: String,
    pub platform: IssuePlatform,
    pub owner: String,
    pub repository: String,
    pub issue_number: Option<u64>,
    pub allowed_sender_ids: Vec<u64>,
    pub automation_sender_ids: Vec<u64>,
    pub work_kind: IssueWorkKind,
    pub auto_reply: bool,
    pub enabled: bool,
    pub last_checked_at_ms: Option<i64>,
    pub source_error: Option<String>,
    pub latest_intake: Option<IssueSubscriptionIntake>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IssueSubscriptionIntake {
    pub comment_id: i64,
    pub issue_number: Option<i64>,
    pub sender_id: Option<i64>,
    pub observed_at_ms: i64,
    pub subscription_revision: i64,
    pub state: String,
    pub reason: String,
    pub request_id: Option<String>,
    pub authorization_change: Option<IssueAuthorizationChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueSourceAccess {
    Open,
    Paused,
    Withdrawn,
}

/// Recorded when an existing request's effective authorization changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssueAuthorizationChange {
    pub revision: u64,
    pub changed_at_ms: i64,
    pub source_access: IssueSourceAccess,
    pub device_access: bool,
    pub auto_reply: bool,
}
