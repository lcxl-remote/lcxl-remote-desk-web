//! Versioned UTC scheduling rules shared by both central runtimes.

pub mod contract;
pub mod management;
pub mod policy;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const SCHEDULE_SCHEMA_VERSION: u16 = 1;
pub const MAX_SCHEDULE_SPEC_BYTES: usize = 4096;

/// Control-end draft intent. Identity, status, grants and leases are server-owned.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ScheduleDraft {
    pub client_create_key: String,
    pub kind: ScheduledTaskKind,
    pub target_device_id: String,
    pub title: String,
    pub prompt: String,
    pub locale: Option<String>,
    pub model_id: Option<i32>,
    pub spec: ScheduleSpec,
    /// Present when a local-time editor confirmed a server conversion; absent for direct UTC authoring.
    pub time_confirmation: Option<ScheduleTimeConfirmation>,
    pub source_conversation_id: Option<String>,
    pub requirement_revision: Option<u64>,
    pub creation_source: ScheduleCreationSource,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleCreationSource {
    Manual,
    AiProposal,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSpec {
    pub schema_version: u16,
    pub rule: ScheduleRule,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleRule {
    Once {
        at: String,
    },
    Interval {
        every_seconds: u32,
        anchor_at: String,
    },
    Daily {
        utc_time: String,
    },
    Weekly {
        weekdays: Vec<u8>,
        utc_time: String,
    },
}

/// Rechecked by the central runtime when committing a local-time edit.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ScheduleTimeConfirmation {
    pub input: ScheduleTimeConversion,
    pub conversion_version: String,
}

/// Editing metadata is never part of the persisted UTC recurrence rule.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ScheduleTimeConversion {
    pub timezone: String,
    pub reference_date: String,
    pub local_time: String,
    pub fold: Option<ScheduleTimeFold>,
    pub rule: LocalScheduleRule,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleTimeFold {
    Earlier,
    Later,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalScheduleRule {
    Once,
    Interval { every_seconds: u32 },
    Daily,
    Weekly { weekdays: Vec<u8> },
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
pub struct ScheduleTimeConverted {
    pub spec: ScheduleSpec,
    pub offset_seconds: i32,
    pub conversion_version: String,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskKind {
    ConversationResume,
    FreshTask,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledTaskStatus {
    Draft,
    Rehearsing,
    AwaitingAuthorization,
    Active,
    Triggered,
    Paused,
    Completed,
    Deleted,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    SchemaRead,
    SchemaWrite,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SchedulePauseReason {
    User,
    ConsecutiveFailures,
    UnknownSideEffect,
    AuthorizationInvalid,
    ScheduleUpgradeRequired,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledRunStatus {
    Queued,
    WaitingDevice,
    Running,
    AwaitingPermission,
    Succeeded,
    Failed,
    Missed,
    SkippedOverlap,
    Cancelled,
    Superseded,
    OutcomeUnknown,
}

impl ScheduledRunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Failed
                | Self::Missed
                | Self::SkippedOverlap
                | Self::Cancelled
                | Self::Superseded
                | Self::OutcomeUnknown
        )
    }
}
