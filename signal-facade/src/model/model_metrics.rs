//! Current model-observation API. Counts and revisions are decimal strings.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    FiveMinutes,
    #[default]
    Hour,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, IntoParams, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricsQuery {
    /// Runtime endpoint only.
    pub category: Option<String>,
    /// Runtime endpoint only: a closed registered definition.
    pub definition: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub granularity: Option<Granularity>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub surface: Option<String>,
    pub purpose: Option<String>,
    pub origin: Option<String>,
    pub tool: Option<String>,
    /// Closed domain and category, for example request.http_error or input.invalid_json.
    pub error: Option<String>,
    pub contract_revision: Option<String>,
    pub include_probe: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    /// Model/tool grouping endpoints only; applied before Top N selection.
    pub group_sort: Option<MetricGroupSort>,
    /// Calls endpoint only; selects one closed observation type.
    pub record_kind: Option<MetricRecordKind>,
    /// Calls endpoint only; a type-specific conclusion or request_error.
    pub outcome: Option<String>,
    /// Calls endpoint only; inclusive observed latency threshold.
    pub min_duration_ms: Option<u32>,
    /// Calls endpoint only; defaults to duration when a threshold is provided.
    pub latency: Option<MetricLatency>,
    /// Calls endpoint only; a closed tool permission conclusion.
    pub permission: Option<String>,
    /// Calls endpoint only; an observed native operation dispatch conclusion.
    pub dispatched: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetricRecordKind {
    Call,
    Attempt,
    Tool,
    Operation,
    Runtime,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetricLatency {
    #[default]
    Duration,
    FirstContent,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetricGroupSort {
    #[default]
    Calls,
    RequestErrors,
    RequestFailureRate,
    InputRejected,
    InputRejectionRate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SampleStatus {
    Complete,
    Partial,
    NoSamples,
    NotCollected,
    Unknown,
    Unavailable,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Ready,
    Initializing,
    Disabled,
    CacheExpired,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricsSettings {
    pub revision: String,
    pub enabled: bool,
    pub detail_days: u32,
    pub five_minute_days: u32,
    pub hourly_days: u32,
    pub mutable_days: u32,
    pub detail_row_budget: u32,
    pub event_row_budget: u32,
    pub compact_row_budget: u32,
    pub rollup_row_budget: u32,
    pub series_per_bucket: u32,
    pub storage_budget_bytes: String,
}

impl MetricsSettings {
    pub fn defaults(manager: bool) -> Self {
        Self {
            revision: "1".into(),
            enabled: true,
            detail_days: 7,
            five_minute_days: 7,
            hourly_days: 90,
            mutable_days: 7,
            detail_row_budget: if manager { 1_000_000 } else { 100_000 },
            event_row_budget: if manager { 200_000 } else { 20_000 },
            compact_row_budget: if manager { 1_000_000 } else { 100_000 },
            rollup_row_budget: if manager { 250_000 } else { 25_000 },
            series_per_bucket: 256,
            storage_budget_bytes: if manager {
                2_147_483_648u64
            } else {
                268_435_456u64
            }
            .to_string(),
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let revision = self
            .revision
            .parse::<i64>()
            .map_err(|_| "invalid revision")?;
        let bytes = self
            .storage_budget_bytes
            .parse::<u64>()
            .map_err(|_| "invalid storage budget")?;
        if revision <= 0
            || revision == i64::MAX
            || self.revision != revision.to_string()
            || self.storage_budget_bytes != bytes.to_string()
            || !(1..=90).contains(&self.detail_days)
            || !(1..=30).contains(&self.five_minute_days)
            || !(1..=365).contains(&self.hourly_days)
            || self.detail_days > self.hourly_days
            || self.five_minute_days > self.hourly_days
            || self.mutable_days == 0
            || self.mutable_days > self.detail_days
            || self.mutable_days > self.hourly_days
            || !(1_000..=5_000_000).contains(&self.detail_row_budget)
            || !(1_000..=1_000_000).contains(&self.event_row_budget)
            || self.compact_row_budget < self.detail_row_budget
            || self.compact_row_budget > 5_000_000
            || !(1_000..=1_000_000).contains(&self.rollup_row_budget)
            || !(16..=1_024).contains(&self.series_per_bucket)
            || !(16_777_216..=17_179_869_184).contains(&bytes)
        {
            return Err("metrics settings are outside the supported bounds");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CoverageGap {
    pub from: String,
    pub to: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsStatus {
    pub state: ComponentState,
    pub enabled: Option<bool>,
    pub schema_version: u32,
    pub definition_version: u32,
    pub available_from: Option<String>,
    pub settings_revision: Option<String>,
    pub settings_effective: bool,
    pub last_persisted: Option<String>,
    pub last_aggregated: Option<String>,
    pub as_of: String,
    pub backlog: Option<String>,
    pub oldest_pending: Option<String>,
    pub dropped_events: Option<String>,
    pub discarded_events: Option<String>,
    /// Retained facts excluded from all model/input/operation denominators.
    pub unassociated_records: Option<String>,
    pub instrumented_surfaces: Vec<String>,
    pub unsupported_surfaces: Vec<String>,
    pub gaps: Vec<CoverageGap>,
    pub retention: Option<MetricsSettings>,
    pub storage: Option<MetricsStorage>,
    pub reason: Option<String>,
}

impl MetricsStatus {
    pub fn unavailable(state: ComponentState, now_ms: i64) -> Self {
        Self {
            state,
            enabled: None,
            schema_version: desk_diagnose_core::model_observability::EVENT_SCHEMA_VERSION,
            definition_version: desk_diagnose_core::model_observability::DEFINITION_VERSION,
            available_from: None,
            settings_revision: None,
            settings_effective: false,
            last_persisted: None,
            last_aggregated: None,
            as_of: crate::service::model_metrics::timestamp(now_ms),
            backlog: None,
            oldest_pending: None,
            dropped_events: None,
            discarded_events: None,
            unassociated_records: None,
            instrumented_surfaces: Vec::new(),
            unsupported_surfaces: Vec::new(),
            gaps: Vec::new(),
            retention: None,
            storage: None,
            reason: Some(
                if state == ComponentState::Initializing {
                    "initializing"
                } else {
                    "storage_unavailable"
                }
                .into(),
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsStorage {
    /// Conservative quota charge, separate from physical database allocation.
    pub charged_bytes: String,
    pub budget_bytes: String,
    pub reserved_bytes: String,
    pub physical_allocated_bytes: Option<String>,
    pub physical_sampled_at: Option<String>,
    pub cleanup_active: bool,
    pub rows: Vec<MetricsStorageRows>,
    pub trimmed_details: String,
    pub dropped_pending_events: String,
    pub frozen_before: Option<String>,
    pub rollup_trim_before: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetricsStorageKind {
    Event,
    Compact,
    Detail,
    Rollup,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsStorageRows {
    pub kind: MetricsStorageKind,
    pub rows: String,
    pub budget: String,
    pub cleanup_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricCount {
    pub key: String,
    pub count: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricRate {
    pub key: String,
    pub numerator: Option<String>,
    pub denominator: Option<String>,
    pub value: Option<f64>,
    pub definition_version: u32,
    pub sample_status: SampleStatus,
    pub applied_filters: Vec<MetricFilter>,
    pub unsupported_filters: Vec<MetricFilter>,
    pub denominator_scope: String,
    pub numerator_error: Option<String>,
    pub reason: Option<MetricRateReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetricFilter {
    Time,
    ProviderId,
    ModelId,
    Surface,
    Purpose,
    Origin,
    Tool,
    Error,
    ContractRevision,
    IncludeProbe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetricRateReason {
    UnsupportedToolFilter,
    UnsupportedErrorDomain,
    ArithmeticUnavailable,
    RetentionTrim,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LatencySummary {
    pub count: String,
    pub average_ms: Option<f64>,
    pub min_ms: Option<u64>,
    pub max_ms: Option<u64>,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
    pub p99_ms: Option<u64>,
    pub overflow_count: String,
    pub extrema_unavailable: bool,
    pub estimated_percentiles: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsSummary {
    pub quantities: Vec<MetricQuantity>,
    pub counts: Vec<MetricCount>,
    pub rates: Vec<MetricRate>,
    pub duration: LatencySummary,
    pub first_content: LatencySummary,
    pub other_duration: Vec<NamedLatency>,
    pub errors: Vec<MetricErrorCount>,
    pub schema_paths: Vec<MetricSchemaPath>,
    pub other_schema_errors: Option<String>,
    pub schema_paths_limited: bool,
    pub stages: Vec<MetricStageCount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricErrorCount {
    pub metric: String,
    pub error: String,
    pub count: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricSchemaPath {
    pub path: String,
    pub count: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricStageCount {
    pub stage: String,
    pub outcome: String,
    pub count: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NamedLatency {
    pub kind: String,
    pub summary: LatencySummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueryCoverage {
    pub requested_from: String,
    pub requested_to: String,
    pub effective_from: String,
    pub effective_to: String,
    pub as_of: String,
    pub available_from: String,
    pub not_collected_before: Option<String>,
    pub trimmed_before: Option<String>,
    pub sample_status: SampleStatus,
    pub cohort_basis: String,
    pub granularity: Granularity,
    pub usage_source: String,
    pub usage_filter_dimensions: Vec<String>,
    pub live_pagination: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsOverview {
    pub coverage: QueryCoverage,
    pub summary: MetricsSummary,
    pub previous: Option<MetricsSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricSeriesPoint {
    pub bucket: String,
    pub summary: MetricsSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsSeries {
    pub coverage: QueryCoverage,
    pub points: Vec<MetricSeriesPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsGroup {
    pub key: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub tool: Option<String>,
    pub summary: MetricsSummary,
    pub associated_models: Vec<MetricAssociatedModel>,
    pub other_model_count: String,
    pub configurations: Vec<MetricConfigurationSample>,
    pub configurations_limited: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricConfigurationSample {
    pub provider_id: String,
    pub model_id: String,
    pub model_name: String,
    /// Observed revisions in this query cohort, never current catalog values.
    pub revisions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricAssociatedModel {
    pub provider_id: String,
    pub model_id: String,
    pub model_name: String,
    pub tool_inputs: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsGroups {
    pub coverage: QueryCoverage,
    pub groups: Vec<MetricsGroup>,
    pub other: Option<MetricsSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ObservationRecord {
    pub id: String,
    pub call_id: Option<String>,
    pub tool_observation_id: Option<String>,
    pub kind: String,
    pub started_at: String,
    pub updated_at: String,
    pub provider_id: String,
    pub model_id: String,
    pub model_name: String,
    pub purpose: String,
    pub surface: String,
    pub origin: String,
    pub configuration_scope: String,
    pub configuration_revision: String,
    pub contract_revision: String,
    pub protocol: String,
    pub outcome: String,
    pub not_started_reason: Option<String>,
    pub output: Option<String>,
    pub tool: Option<String>,
    pub ordinal: Option<u32>,
    pub input_conclusion: Option<String>,
    pub input_issue: Option<String>,
    pub schema_path: Option<String>,
    pub stages: Vec<StageResult>,
    pub permission: Option<String>,
    pub correction_of: Option<String>,
    pub correction_status: Option<String>,
    pub correction_input: Option<String>,
    pub correction_group_root: Option<String>,
    pub correction_group: Option<CorrectionGroupRecord>,
    pub correction_group_unavailable: bool,
    pub duration_ms: Option<u64>,
    pub headers_ms: Option<u64>,
    pub first_content_ms: Option<u64>,
    pub http_status: Option<u16>,
    pub input_tokens: Option<String>,
    pub output_tokens: Option<String>,
    pub cache_read_tokens: Option<String>,
    pub cache_write_tokens: Option<String>,
    pub usage_complete: Option<bool>,
    /// Adapter-observed tool generation count, absent if not observed.
    pub generated_tool_count: Option<String>,
    /// Collected current input objects, derived from retained compact state.
    pub tool_count: Option<String>,
    pub input_rejected_count: Option<String>,
    pub tool_counts_status: SampleStatus,
    pub dispatched: Option<bool>,
    pub detail_trimmed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StageResult {
    pub stage: String,
    pub outcome: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CorrectionGroupRecord {
    pub root_id: String,
    pub last_input_id: String,
    pub category: String,
    pub reason: Option<String>,
    pub outcome: String,
    pub linked_attempts: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsCalls {
    pub coverage: QueryCoverage,
    pub records: Vec<ObservationRecord>,
    pub next_cursor: Option<String>,
    /// Immutable first-page reception cutoff; terminal states remain live.
    pub received_before: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsCallDetail {
    pub call: ObservationRecord,
    pub related: Vec<ObservationRecord>,
    pub related_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssociationGapKind {
    Attribution,
    OperationStart,
    CorrectionPrerequisite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssociationGapState {
    Waiting,
    Unavailable,
    Conflict,
    OutsideWindow,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, IntoParams, ToSchema)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct UnassociatedQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UnassociatedModel {
    pub provider_id: String,
    pub model_id: String,
    pub model_name: String,
    pub purpose: String,
    pub surface: String,
    pub origin: String,
    pub configuration_scope: String,
    pub configuration_revision: String,
    pub contract_revision: String,
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UnassociatedRecord {
    pub id: String,
    pub kind: String,
    pub phase: String,
    /// No reception timestamp is substituted for an unknown original start.
    pub started_at: Option<String>,
    pub received_at: String,
    pub occurred_at: String,
    pub updated_at: String,
    pub missing: AssociationGapKind,
    pub state: AssociationGapState,
    pub original_model: Option<UnassociatedModel>,
    pub call_id: Option<String>,
    pub tool_observation_id: Option<String>,
    pub tool: Option<String>,
    pub ordinal: Option<u32>,
    pub permission: Option<String>,
    pub fact_outcome: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsUnassociated {
    pub coverage: QueryCoverage,
    pub records: Vec<UnassociatedRecord>,
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retention_budget_and_decimal_revision_are_validated() {
        let mut settings = MetricsSettings::defaults(true);
        assert!(settings.validate().is_ok());
        settings.revision = "9007199254740993".into();
        assert!(settings.validate().is_ok());
        settings.mutable_days = 8;
        assert!(settings.validate().is_err());
        settings.mutable_days = 7;
        settings.storage_budget_bytes = "-1".into();
        assert!(settings.validate().is_err());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricQuantity {
    pub key: String,
    pub sample_count: String,
    pub sum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsRuntimeGroup {
    pub definition: String,
    pub category: String,
    pub labels: std::collections::BTreeMap<String, String>,
    pub contract_revision: Option<String>,
    pub summary: MetricsSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MetricsRuntimeGroups {
    pub coverage: QueryCoverage,
    pub groups: Vec<MetricsRuntimeGroup>,
    pub other: Option<MetricsSummary>,
}
