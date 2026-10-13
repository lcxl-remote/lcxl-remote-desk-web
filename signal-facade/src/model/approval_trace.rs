//! Administrator-only readable approval records and content-free aggregates.
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalTraceSettings {
    pub revision: String,
    pub enabled: bool,
    pub retention_days: u32,
    pub storage_budget_bytes: String,
    pub max_trace_bytes: u32,
}
impl ApprovalTraceSettings {
    pub fn defaults(manager: bool) -> Self {
        Self {
            revision: "1".into(),
            enabled: true,
            retention_days: 7,
            storage_budget_bytes: (if manager {
                2_147_483_648u64
            } else {
                268_435_456
            })
            .to_string(),
            max_trace_bytes: 16_777_216,
        }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        let revision = self
            .revision
            .parse::<u64>()
            .map_err(|_| "invalid revision")?;
        let bytes = self
            .storage_budget_bytes
            .parse::<u64>()
            .map_err(|_| "invalid storage budget")?;
        if revision == 0
            || revision.to_string() != self.revision
            || bytes.to_string() != self.storage_budget_bytes
            || !(1..=3650).contains(&self.retention_days)
            || !(16_777_216..=17_179_869_184).contains(&bytes)
            || !(262_144..=67_108_864).contains(&self.max_trace_bytes)
            || u64::from(self.max_trace_bytes) > bytes
        {
            return Err("invalid approval trace settings");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ApprovalReviewsQuery {
    pub session: String,
    pub include_children: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ApprovalReviewQuery {
    pub session: String,
    pub id: String,
}
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ApprovalPartsQuery {
    pub session: String,
    pub id: String,
    pub kind: String,
    pub after: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApprovalReviewDto {
    pub id: String,
    pub session: String,
    pub root_session: Option<String>,
    pub session_label: Option<String>,
    pub task: Option<desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentSummary>,
    pub decision: Option<serde_json::Value>,
    pub grant_states: Vec<String>,
    pub execution_state: Option<String>,
    pub timeline: Vec<ApprovalTraceStageDto>,
    pub business_finished_at_ms: Option<i64>,
    pub session_kind: String,
    pub source_kind: String,
    pub source_id: String,
    pub status: String,
    pub reason: Option<String>,
    pub model_name: Option<String>,
    pub protocol: Option<String>,
    pub model_revision: String,
    pub output_limit: Option<String>,
    pub created_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub stop_reason: Option<String>,
    pub usage: Option<serde_json::Value>,
    pub recording_state: String,
    pub captured_bytes: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApprovalTraceStageDto {
    pub stage: String,
    pub origin: String,
    pub at_ms: i64,
}
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApprovalReviewsDto {
    pub reviews: Vec<ApprovalReviewDto>,
    pub next_cursor: Option<String>,
}
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApprovalTracePartDto {
    pub sequence: u32,
    pub kind: String,
    pub text: String,
    pub at_ms: i64,
}
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApprovalTracePartsDto {
    pub parts: Vec<ApprovalTracePartDto>,
    pub next_after: Option<u32>,
    pub recording_state: String,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ApprovalMetricsQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub granularity: Option<String>,
    pub model_id: Option<String>,
    pub provider_id: Option<String>,
    pub source_kind: Option<String>,
    pub session_kind: Option<String>,
    pub reason: Option<String>,
    pub outcome: Option<String>,
}
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApprovalRateDto {
    pub approved: String,
    pub denied: String,
    pub fault: String,
    pub denominator: String,
    pub approval_rate: Option<f64>,
    pub denial_rate: Option<f64>,
    pub fault_rate: Option<f64>,
}
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApprovalMetricCount {
    pub kind: String,
    pub count: String,
}
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApprovalMetricGroup {
    pub key: String,
    pub count: String,
    pub share: Option<f64>,
    pub rates: ApprovalRateDto,
    pub waiting: super::model_metrics::LatencySummary,
}
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApprovalMetricPoint {
    pub bucket: String,
    pub rates: ApprovalRateDto,
    pub count: String,
}
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApprovalMetricsOverviewDto {
    pub rates: ApprovalRateDto,
    pub counts: Vec<ApprovalMetricCount>,
    pub session_kinds: Vec<ApprovalMetricGroup>,
    pub timeseries: Vec<ApprovalMetricPoint>,
    pub waiting: super::model_metrics::LatencySummary,
    pub before_model: super::model_metrics::LatencySummary,
    pub pending_age: super::model_metrics::LatencySummary,
    pub unknown_waiting: String,
    pub as_of_ms: i64,
    pub coverage: super::model_metrics::QueryCoverage,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budgets_are_storage_only_and_validated() {
        let mut settings = ApprovalTraceSettings::defaults(false);
        assert!(settings.validate().is_ok());
        settings.max_trace_bytes = 0;
        assert!(settings.validate().is_err());
    }
}
