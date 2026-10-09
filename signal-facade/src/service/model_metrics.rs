//! Shared bounded query semantics and content-free REST projections.

pub mod collector;
pub mod unassociated;

use chrono::{DateTime, Utc};
use desk_diagnose_core::model_observability::{
    aggregate::{Histogram, RATES, Rate, Totals},
    *,
};
use serde::{Serialize, de::DeserializeOwned};

use crate::model::model_metrics::*;

pub fn tag<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

pub fn timestamp(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| "unknown".into())
}

/// Unknown identities remain absent in model choices and group drill-downs.
pub fn group_identity(
    attribution: Option<&Attribution>,
) -> (Option<String>, Option<String>, Option<String>) {
    let Some(attribution) = attribution.filter(|value| value.model_identity_known()) else {
        return (None, None, None);
    };
    (
        Some(attribution.provider_id.clone()),
        Some(attribution.model_id.clone()),
        (!attribution.model_name.is_empty()).then(|| attribution.model_name.clone()),
    )
}

fn parse_tag<T: DeserializeOwned>(value: &str) -> Result<T, &'static str> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|_| "unsupported metrics filter")
}

/// Record selectors never participate in aggregate quality denominators.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CallFilters {
    pub kind: Option<MetricRecordKind>,
    pub outcome: Option<String>,
    pub min_duration_ms: Option<u32>,
    pub latency: Option<MetricLatency>,
    pub permission: Option<PermissionOutcome>,
    pub dispatched: Option<bool>,
}

impl CallFilters {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedQuery {
    pub requested_from_ms: i64,
    pub requested_to_ms: i64,
    pub from_ms: i64,
    pub to_ms: i64,
    pub granularity_ms: i64,
    pub granularity: Granularity,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub surface: Option<Surface>,
    pub purpose: Option<Purpose>,
    pub origin: Option<Origin>,
    pub tool: Option<String>,
    pub error: Option<ErrorSelector>,
    pub contract_revision: Option<String>,
    pub include_probe: bool,
    pub cursor: Option<String>,
    pub limit: u32,
    pub group_sort: Option<MetricGroupSort>,
    pub calls: CallFilters,
}

impl ResolvedQuery {
    pub fn resolve(query: &MetricsQuery, now_ms: i64) -> Result<Self, &'static str> {
        let parse_time = |value: &str| {
            DateTime::parse_from_rfc3339(value)
                .map(|value| value.timestamp_millis())
                .map_err(|_| "invalid metrics time")
        };
        let to = query
            .to
            .as_deref()
            .map(parse_time)
            .transpose()?
            .unwrap_or(now_ms)
            .min(now_ms);
        let from = query
            .from
            .as_deref()
            .map(parse_time)
            .transpose()?
            .unwrap_or(to.saturating_sub(86_400_000));
        if from < 0 || to <= from || to - from > 90 * 86_400_000 {
            return Err("metrics range must be positive and at most 90 days");
        }
        for value in [
            &query.provider_id,
            &query.model_id,
            &query.tool,
            &query.error,
            &query.contract_revision,
        ]
        .into_iter()
        .flatten()
        {
            if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
                return Err("invalid metrics filter length");
            }
        }
        if query.cursor.as_ref().is_some_and(|value| value.len() > 320) {
            return Err("invalid metrics cursor");
        }
        if query.cursor.is_some() && (query.from.is_none() || query.to.is_none()) {
            return Err("metrics pagination requires a fixed time range");
        }
        let granularity = query.granularity.unwrap_or_default();
        let width = match granularity {
            Granularity::FiveMinutes => 300_000,
            Granularity::Hour => 3_600_000,
        };
        let limit = query.limit.unwrap_or(50);
        if limit == 0 || limit > 200 {
            return Err("metrics page limit must be between 1 and 200");
        }
        let error = query
            .error
            .as_deref()
            .map(str::parse::<ErrorSelector>)
            .transpose()?;
        Ok(Self {
            requested_from_ms: from,
            requested_to_ms: to,
            from_ms: from.div_euclid(width) * width,
            to_ms: ((to - 1).div_euclid(width) + 1) * width,
            granularity_ms: width,
            granularity,
            provider_id: query.provider_id.clone(),
            model_id: query.model_id.clone(),
            surface: query.surface.as_deref().map(parse_tag).transpose()?,
            purpose: query.purpose.as_deref().map(parse_tag).transpose()?,
            origin: query.origin.as_deref().map(parse_tag).transpose()?,
            tool: query.tool.clone(),
            error,
            contract_revision: query.contract_revision.clone(),
            include_probe: query.include_probe.unwrap_or(false)
                || query.purpose.as_deref() == Some("probe"),
            cursor: query.cursor.clone(),
            limit,
            group_sort: query.group_sort,
            calls: CallFilters {
                kind: query.record_kind,
                outcome: query.outcome.clone(),
                min_duration_ms: query.min_duration_ms,
                latency: query.latency,
                permission: query.permission.as_deref().map(parse_tag).transpose()?,
                dispatched: query.dispatched,
            },
        })
    }

    pub fn has_dimensions(&self) -> bool {
        self.provider_id.is_some()
            || self.model_id.is_some()
            || self.surface.is_some()
            || self.purpose.is_some()
            || self.origin.is_some()
            || self.contract_revision.is_some()
            || self.tool.is_some()
    }

    pub fn matches(&self, attribution: &Attribution, tool: Option<&str>) -> bool {
        (self.include_probe || attribution.purpose != Purpose::Probe)
            && self
                .provider_id
                .as_ref()
                .is_none_or(|value| value == &attribution.provider_id)
            && self
                .model_id
                .as_ref()
                .is_none_or(|value| value == &attribution.model_id)
            && self
                .surface
                .is_none_or(|value| value == attribution.surface)
            && self
                .purpose
                .is_none_or(|value| value == attribution.purpose)
            && self.origin.is_none_or(|value| value == attribution.origin)
            && self
                .contract_revision
                .as_ref()
                .is_none_or(|value| value == &attribution.contract_revision)
            && self.tool.as_deref().is_none_or(|value| Some(value) == tool)
    }

    pub fn coverage(
        &self,
        now_ms: i64,
        available_from: i64,
        partial: bool,
        manager: bool,
    ) -> QueryCoverage {
        QueryCoverage {
            requested_from: timestamp(self.requested_from_ms),
            requested_to: timestamp(self.requested_to_ms),
            effective_from: timestamp(self.from_ms),
            effective_to: timestamp(self.to_ms),
            as_of: timestamp(now_ms),
            available_from: timestamp(available_from),
            not_collected_before: (self.from_ms < available_from)
                .then(|| timestamp(available_from)),
            trimmed_before: None,
            sample_status: if self.requested_to_ms <= available_from {
                SampleStatus::NotCollected
            } else if partial || self.from_ms < available_from {
                SampleStatus::Partial
            } else {
                SampleStatus::Complete
            },
            cohort_basis: "model_call_start;operation_dispatch;runtime_event;usage_metered".into(),
            granularity: self.granularity,
            usage_source: if manager {
                "business_ledger"
            } else {
                "observations"
            }
            .into(),
            usage_filter_dimensions: if manager {
                vec!["provider_id".into(), "model_id".into(), "purpose".into()]
            } else {
                vec![
                    "provider_id".into(),
                    "model_id".into(),
                    "purpose".into(),
                    "surface".into(),
                    "origin".into(),
                    "contract_revision".into(),
                ]
            },
            live_pagination: true,
        }
    }

    pub fn filters(&self) -> Vec<MetricFilter> {
        let mut filters = vec![MetricFilter::Time];
        for (present, name) in [
            (self.provider_id.is_some(), MetricFilter::ProviderId),
            (self.model_id.is_some(), MetricFilter::ModelId),
            (self.surface.is_some(), MetricFilter::Surface),
            (self.purpose.is_some(), MetricFilter::Purpose),
            (self.origin.is_some(), MetricFilter::Origin),
            (self.tool.is_some(), MetricFilter::Tool),
            (self.error.is_some(), MetricFilter::Error),
            (
                self.contract_revision.is_some(),
                MetricFilter::ContractRevision,
            ),
        ] {
            if present {
                filters.push(name);
            }
        }
        filters.push(MetricFilter::IncludeProbe);
        filters
    }

    pub fn validate_call_filters(&self) -> Result<(), &'static str> {
        if self.group_sort.is_some() {
            return Err("group sorting requires a grouping endpoint");
        }
        let kind = self.record_kind();
        if (self.tool.is_some()
            && !matches!(kind, MetricRecordKind::Tool | MetricRecordKind::Operation))
            || matches!(self.error, Some(ErrorSelector::Input(_))) && kind != MetricRecordKind::Tool
            || matches!(self.error, Some(ErrorSelector::Request(_)))
                && !matches!(kind, MetricRecordKind::Call | MetricRecordKind::Attempt)
            || matches!(self.error, Some(ErrorSelector::Output(_)))
                && kind != MetricRecordKind::Call
            || self.calls.permission.is_some() && kind != MetricRecordKind::Tool
            || self.calls.dispatched.is_some() && kind != MetricRecordKind::Operation
            || self.calls.latency == Some(MetricLatency::FirstContent)
                && !matches!(kind, MetricRecordKind::Call | MetricRecordKind::Attempt)
            || self
                .calls
                .min_duration_ms
                .is_some_and(|value| value > 86_400_000)
        {
            return Err("record filters are not applicable to the selected observation type");
        }
        if let Some(outcome) = self.calls.outcome.as_deref() {
            let valid = match kind {
                MetricRecordKind::Call | MetricRecordKind::Attempt => {
                    outcome == "request_error" || parse_tag::<RequestOutcome>(outcome).is_ok()
                }
                MetricRecordKind::Tool => parse_tag::<InputConclusion>(outcome).is_ok(),
                MetricRecordKind::Operation => parse_tag::<OperationOutcome>(outcome).is_ok(),
                MetricRecordKind::Runtime => false,
            };
            if !valid {
                return Err("unsupported observation outcome");
            }
        }
        Ok(())
    }

    pub fn record_kind(&self) -> MetricRecordKind {
        self.calls.kind.unwrap_or(
            if self.tool.is_some() || matches!(self.error, Some(ErrorSelector::Input(_))) {
                MetricRecordKind::Tool
            } else {
                MetricRecordKind::Call
            },
        )
    }

    pub fn validate_aggregate_filters(&self) -> Result<(), &'static str> {
        if !self.calls.is_empty() || self.cursor.is_some() {
            return Err("record filters and cursors require the calls endpoint");
        }
        Ok(())
    }

    pub fn validate_group_sort(&self, tools: bool) -> Result<(), &'static str> {
        self.validate_aggregate_filters()?;
        let Some(sort) = self.group_sort else {
            return Ok(());
        };
        if tools
            && matches!(
                sort,
                MetricGroupSort::RequestErrors | MetricGroupSort::RequestFailureRate
            )
        {
            return Err("tool groups do not contain request outcomes");
        }
        let rate = match sort {
            MetricGroupSort::RequestFailureRate => Some(Rate::RequestFailure),
            MetricGroupSort::InputRejectionRate => Some(Rate::InputRejection),
            _ => None,
        };
        if rate
            .zip(self.error)
            .is_some_and(|(rate, error)| !error.supports(rate))
        {
            return Err("group sort is not applicable to the selected error domain");
        }
        Ok(())
    }

    fn call_cursor_context(&self, manager: bool) -> Result<String, &'static str> {
        use sha2::{Digest, Sha256};
        let value = serde_json::to_vec(&(
            manager,
            self.requested_from_ms,
            self.requested_to_ms,
            self.granularity,
            &self.provider_id,
            &self.model_id,
            self.surface,
            self.purpose,
            self.origin,
            &self.tool,
            self.error,
            &self.contract_revision,
            self.include_probe,
            self.limit,
            &self.calls,
        ))
        .map_err(|_| "invalid metrics cursor context")?;
        Ok(format!("{:x}", Sha256::digest(value)))
    }

    pub fn call_cursor(
        &self,
        manager: bool,
        received_before: i64,
        time: i64,
        id: &str,
    ) -> Result<String, &'static str> {
        Ok(format!(
            "{}~{received_before}~{time}~{id}",
            self.call_cursor_context(manager)?
        ))
    }

    pub fn call_boundary(
        &self,
        manager: bool,
        now: i64,
    ) -> Result<Option<(i64, String, i64)>, &'static str> {
        let Some(cursor) = &self.cursor else {
            return Ok(None);
        };
        let mut parts = cursor.split('~');
        let context = parts.next().ok_or("invalid metrics cursor")?;
        let received_before = parts
            .next()
            .ok_or("invalid metrics cursor")?
            .parse::<i64>()
            .map_err(|_| "invalid metrics cursor")?;
        let time = parts
            .next()
            .ok_or("invalid metrics cursor")?
            .parse::<i64>()
            .map_err(|_| "invalid metrics cursor")?;
        let id = parts.next().ok_or("invalid metrics cursor")?;
        if context != self.call_cursor_context(manager)?
            || parts.next().is_some()
            || received_before < 0
            || received_before > now
            || time < self.from_ms
            || time >= self.to_ms
            || time > received_before
            || id.is_empty()
            || id.len() > 192
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:".contains(&byte))
        {
            return Err("invalid metrics cursor");
        }
        Ok(Some((time, id.into(), received_before)))
    }
}

pub fn latency(value: &Histogram) -> LatencySummary {
    LatencySummary {
        count: value.count.to_string(),
        average_ms: (value.count > 0).then(|| value.sum_ms as f64 / value.count as f64),
        min_ms: value.min_ms,
        max_ms: value.max_ms,
        p50_ms: value.percentile(50),
        p95_ms: value.percentile(95),
        p99_ms: value.percentile(99),
        overflow_count: value.buckets[13].to_string(),
        extrema_unavailable: value.extrema_unavailable,
        estimated_percentiles: true,
    }
}

/// Descending comparison over exact counters; no-sample ratios sort last.
pub fn compare_groups(
    left: &Totals,
    right: &Totals,
    sort: MetricGroupSort,
    error: Option<ErrorSelector>,
    tools: bool,
) -> std::cmp::Ordering {
    use desk_diagnose_core::model_observability::aggregate::Count;
    let left = left.contribution();
    let right = right.contribution();
    let rate = match sort {
        MetricGroupSort::RequestFailureRate => Some(Rate::RequestFailure),
        MetricGroupSort::InputRejectionRate => Some(Rate::InputRejection),
        _ => None,
    };
    if let Some(rate) = rate {
        let pair = |value: &desk_diagnose_core::model_observability::aggregate::Contribution| {
            value
                .rate(rate)
                .filter(|(numerator, denominator)| *denominator > 0 && numerator <= denominator)
                .map(|(numerator, denominator)| {
                    (
                        error
                            .filter(|error| error.supports(rate))
                            .map_or(numerator, |error| value.error_numerator(rate, error)),
                        denominator,
                    )
                })
        };
        return match (pair(&left), pair(&right)) {
            (Some((a, b)), Some((c, d))) => {
                (u128::from(c) * u128::from(b)).cmp(&(u128::from(a) * u128::from(d)))
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
    }
    let count = match sort {
        MetricGroupSort::Calls => {
            if tools {
                Count::Tools
            } else {
                Count::Calls
            }
        }
        MetricGroupSort::RequestErrors => Count::RequestErrors,
        MetricGroupSort::InputRejected => Count::InputRejected,
        _ => unreachable!(),
    };
    right.get(count).cmp(&left.get(count))
}

pub fn summary(totals: &Totals, query: &ResolvedQuery, partial: bool) -> MetricsSummary {
    let mut paths: Vec<_> = totals
        .schema_errors
        .iter()
        .filter(|(_, count)| **count > 0)
        .collect();
    paths.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let other_schema_errors = paths
        .iter()
        .skip(10)
        .try_fold(totals.schema_errors_other, |sum, (_, count)| {
            sum.checked_add(**count)
        });
    let contribution = totals.contribution();
    let rates = RATES
        .into_iter()
        .map(|rate| {
            let pair = contribution
                .rate(rate)
                .map(|(numerator, denominator)| {
                    let numerator = query
                        .error
                        .filter(|error| error.supports(rate))
                        .map_or(numerator, |error| contribution.error_numerator(rate, error));
                    (numerator, denominator)
                })
                .filter(|(numerator, denominator)| numerator <= denominator);
            let tool_unsupported = query.tool.is_some()
                && matches!(
                    rate,
                    Rate::RequestFailure
                        | Rate::AttemptFailure
                        | Rate::OutputRejection
                        | Rate::BasicUsageCoverage
                        | Rate::CacheReadShare
                );
            let error_unsupported = query.error.is_some_and(|error| !error.supports(rate));
            let mut unsupported_filters = Vec::new();
            if tool_unsupported {
                unsupported_filters.push(MetricFilter::Tool);
            }
            if error_unsupported {
                unsupported_filters.push(MetricFilter::Error);
            }
            let applicable = unsupported_filters.is_empty();
            let applied_filters = query
                .filters()
                .into_iter()
                .filter(|filter| !unsupported_filters.contains(filter))
                .collect();
            let numerator = pair.map(|(numerator, _)| numerator);
            let denominator = pair.map(|(_, denominator)| denominator);
            let sample_status = if !applicable {
                SampleStatus::NotApplicable
            } else if pair.is_none() {
                SampleStatus::Unavailable
            } else if denominator == Some(0) {
                SampleStatus::NoSamples
            } else if partial {
                SampleStatus::Partial
            } else {
                SampleStatus::Complete
            };
            let reason = if tool_unsupported {
                Some(MetricRateReason::UnsupportedToolFilter)
            } else if error_unsupported {
                Some(MetricRateReason::UnsupportedErrorDomain)
            } else if pair.is_none() {
                Some(MetricRateReason::ArithmeticUnavailable)
            } else {
                None
            };
            MetricRate {
                key: tag(&rate),
                numerator: applicable
                    .then_some(numerator)
                    .flatten()
                    .map(|value| value.to_string()),
                denominator: applicable
                    .then_some(denominator)
                    .flatten()
                    .map(|value| value.to_string()),
                value: if applicable {
                    numerator
                        .zip(denominator)
                        .filter(|(_, denominator)| *denominator > 0)
                        .map(|(numerator, denominator)| numerator as f64 / denominator as f64)
                } else {
                    None
                },
                definition_version: DEFINITION_VERSION,
                sample_status,
                applied_filters,
                unsupported_filters,
                denominator_scope: denominator_scope(rate).into(),
                numerator_error: query
                    .error
                    .filter(|error| error.supports(rate))
                    .map(|error| error.identifier()),
                reason,
            }
        })
        .collect();
    MetricsSummary {
        errors: totals
            .error_counts
            .iter()
            .flat_map(|(metric, errors)| {
                errors
                    .iter()
                    .filter(|(_, count)| **count > 0)
                    .map(|(error, count)| MetricErrorCount {
                        metric: tag(metric),
                        error: error.identifier(),
                        count: count.to_string(),
                    })
            })
            .collect(),
        schema_paths: paths
            .into_iter()
            .take(10)
            .map(|(path, count)| MetricSchemaPath {
                path: path.clone(),
                count: count.to_string(),
            })
            .collect(),
        other_schema_errors: other_schema_errors.map(|value| value.to_string()),
        schema_paths_limited: totals.schema_errors_other > 0,
        stages: totals
            .stage_counts
            .iter()
            .flat_map(|(stage, outcomes)| {
                outcomes.iter().map(|(outcome, count)| MetricStageCount {
                    stage: tag(stage),
                    outcome: tag(outcome),
                    count: count.to_string(),
                })
            })
            .collect(),
        quantities: totals
            .quantities
            .iter()
            .map(|(key, value)| MetricQuantity {
                key: tag(key),
                sample_count: value.count.to_string(),
                sum: value.sum.to_string(),
            })
            .collect(),
        counts: totals
            .counts
            .iter()
            .map(|(key, value)| MetricCount {
                key: tag(key),
                count: value.to_string(),
            })
            .collect(),
        rates,
        duration: latency(&totals.duration),
        first_content: latency(&totals.first_content),
        other_duration: totals
            .other_duration
            .iter()
            .map(|(kind, value)| NamedLatency {
                kind: tag(kind),
                summary: latency(value),
            })
            .collect(),
    }
}

pub fn observed_summary(
    totals: &Totals,
    query: &ResolvedQuery,
    partial: bool,
    available_from: i64,
) -> MetricsSummary {
    let mut result = summary(totals, query, partial || query.from_ms < available_from);
    if query.requested_to_ms <= available_from {
        for rate in &mut result.rates {
            if rate.sample_status != SampleStatus::NotApplicable {
                rate.sample_status = SampleStatus::NotCollected;
                rate.numerator = None;
                rate.denominator = None;
                rate.value = None;
            }
        }
    }
    result
}

fn denominator_scope(rate: Rate) -> &'static str {
    match rate {
        Rate::RequestFailure => "returned_or_known_request_error",
        Rate::AttemptFailure => "returned_or_known_attempt_error",
        Rate::OutputRejection => "accepted_or_rejected_output",
        Rate::ToolFormatFailure => "format_passed_or_rejected",
        Rate::ReferenceFailure => "reference_passed_or_failed",
        Rate::InputRejection => "input_accepted_or_rejected",
        Rate::NextInputAcceptance => "linked_next_input_accepted_or_rejected",
        Rate::ExecutionVerification => "dispatched_operation_with_terminal_result",
        Rate::BasicUsageCoverage => "returned_model_calls",
        Rate::CacheReadShare => "paired_input_and_cache_read_tokens",
        Rate::CorrectionLinkCoverage => "feedback_followed_by_comparable_response",
        Rate::CorrectionConclusionCoverage => "uniquely_linked_next_input",
    }
}

pub fn record(event: &ObservationEvent, detail_trimmed: bool) -> ObservationRecord {
    let attribution = &event.attribution;
    let mut result = ObservationRecord {
        id: event.object_id.clone(),
        call_id: event.call_id.clone(),
        tool_observation_id: None,
        kind: String::new(),
        started_at: timestamp(event.started_at_ms),
        updated_at: timestamp(event.occurred_at_ms),
        provider_id: attribution.provider_id.clone(),
        model_id: attribution.model_id.clone(),
        model_name: attribution.model_name.clone(),
        purpose: tag(&attribution.purpose),
        surface: tag(&attribution.surface),
        origin: tag(&attribution.origin),
        configuration_scope: tag(&attribution.configuration_scope),
        configuration_revision: attribution.configuration_revision.clone(),
        contract_revision: attribution.contract_revision.clone(),
        protocol: tag(&attribution.protocol),
        outcome: "unknown".into(),
        not_started_reason: None,
        output: None,
        tool: None,
        ordinal: None,
        input_conclusion: None,
        input_issue: None,
        schema_path: None,
        stages: vec![],
        permission: None,
        correction_of: None,
        correction_status: None,
        correction_input: None,
        correction_group_root: None,
        correction_group: None,
        correction_group_unavailable: false,
        duration_ms: None,
        headers_ms: None,
        first_content_ms: None,
        http_status: None,
        input_tokens: None,
        output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        usage_complete: None,
        generated_tool_count: None,
        tool_count: None,
        input_rejected_count: None,
        tool_counts_status: SampleStatus::NotApplicable,
        dispatched: None,
        detail_trimmed,
    };
    match &event.payload {
        ObservationPayload::Call(call) => {
            result.kind = "call".into();
            result.outcome = tag(&call.outcome);
            result.output = Some(tag(&call.output));
            result.not_started_reason = call.not_started_reason.map(|reason| tag(&reason));
            result.duration_ms = call.timing.duration_ms;
            result.headers_ms = call.timing.headers_ms;
            result.first_content_ms = call.timing.first_content_ms;
            result.http_status = call.http_status;
            result.usage_complete = Some(call.usage.basic_complete());
            result.generated_tool_count = call.generated_tool_count.map(|value| value.to_string());
            result.tool_counts_status = SampleStatus::Unknown;
            let usage = call.usage.normalized;
            result.input_tokens = usage.input_tokens.map(|v| v.to_string());
            result.output_tokens = usage.output_tokens.map(|v| v.to_string());
            result.cache_read_tokens = usage.cache_read_tokens.map(|v| v.to_string());
            result.cache_write_tokens = usage.cache_write_tokens.map(|v| v.to_string());
        }
        ObservationPayload::Attempt(attempt) => {
            result.kind = "attempt".into();
            result.ordinal = Some(attempt.ordinal);
            result.outcome = tag(&attempt.outcome);
            result.duration_ms = attempt.timing.duration_ms;
            result.headers_ms = attempt.timing.headers_ms;
            result.first_content_ms = attempt.timing.first_content_ms;
            result.http_status = attempt.http_status;
            result.started_at = timestamp(attempt.outbound_at_ms);
        }
        ObservationPayload::Tool(tool) => {
            result.kind = "tool".into();
            result.tool = Some(tool.tool_key.clone());
            result.ordinal = Some(tool.ordinal);
            result.outcome = tag(&tool.conclusion);
            result.input_conclusion = Some(tag(&tool.conclusion));
            result.input_issue = Some(tag(&tool.issue));
            result.schema_path = tool.schema_path.clone();
            result.permission = Some(tag(&tool.permission));
            result.correction_of = tool.correction_of.clone();
            result.correction_status = Some(tag(&tool.correction_status));
            result.correction_input = tool.correction_input.map(|value| tag(&value));
            result.stages = tool
                .stages
                .iter()
                .map(|(stage, outcome)| StageResult {
                    stage: tag(stage),
                    outcome: tag(outcome),
                })
                .collect();
            result.duration_ms = tool.stage_duration_ms;
        }
        ObservationPayload::Operation(op) => {
            result.kind = "operation".into();
            result.outcome = tag(&op.outcome);
            result.dispatched = op.dispatched;
            result.duration_ms = op.duration_ms;
            result.tool_observation_id = op.tool_observation_id.clone();
            result.tool = op.tool_key.clone();
            result.ordinal = Some(op.ordinal);
        }
        ObservationPayload::Runtime(runtime) => {
            result.kind = "runtime".into();
            result.outcome = tag(&runtime.definition);
            result.duration_ms = runtime.duration_ms;
        }
    }
    result
}

pub fn runtime_filters(
    filters: &MetricsQuery,
) -> Result<(Option<RuntimeCategory>, Option<runtime::RuntimeDefinition>), &'static str> {
    for value in [&filters.category, &filters.definition]
        .into_iter()
        .flatten()
    {
        if value.len() > 128 {
            return Err("invalid runtime filter");
        }
    }
    let category = filters
        .category
        .as_deref()
        .map(parse_tag::<RuntimeCategory>)
        .transpose()?;
    let definition = filters
        .definition
        .as_deref()
        .map(parse_tag::<runtime::RuntimeDefinition>)
        .transpose()?;
    if category
        .zip(definition)
        .is_some_and(|(category, definition)| category != definition.category())
    {
        return Err("runtime category does not match the registered definition");
    }
    Ok((category, definition))
}

pub fn runtime_labels(
    labels: &runtime::RuntimeLabels,
) -> std::collections::BTreeMap<String, String> {
    serde_json::to_value(labels)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::String(value) => Some((key, value)),
            serde_json::Value::Bool(value) => Some((key, value.to_string())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::model_observability::aggregate::Count;
    #[test]
    fn record_conditions_are_closed_calls_only_and_bound_to_the_cursor() {
        let now = 1_800_000_000_000;
        let mut input = MetricsQuery {
            from: Some(timestamp(now - 3_600_000)),
            to: Some(timestamp(now)),
            record_kind: Some(MetricRecordKind::Tool),
            outcome: Some("rejected".into()),
            ..Default::default()
        };
        let query = ResolvedQuery::resolve(&input, now).unwrap();
        assert!(query.validate_call_filters().is_ok());
        assert!(query.validate_aggregate_filters().is_err());
        assert!(query.validate_group_sort(true).is_err());
        input.cursor = Some(query.call_cursor(false, now, now - 1, "input-1").unwrap());
        assert!(
            ResolvedQuery::resolve(&input, now)
                .unwrap()
                .call_boundary(false, now)
                .is_ok()
        );
        input.outcome = Some("accepted".into());
        assert!(
            ResolvedQuery::resolve(&input, now)
                .unwrap()
                .call_boundary(false, now)
                .is_err()
        );
        input.cursor = None;
        for (kind, outcome) in [
            (MetricRecordKind::Call, "rejected"),
            (MetricRecordKind::Tool, "http_error"),
            (MetricRecordKind::Operation, "transport_error"),
            (MetricRecordKind::Runtime, "request_error"),
        ] {
            input.record_kind = Some(kind);
            input.outcome = Some(outcome.into());
            assert!(
                ResolvedQuery::resolve(&input, now)
                    .unwrap()
                    .validate_call_filters()
                    .is_err()
            );
        }
        input.record_kind = Some(MetricRecordKind::Call);
        input.outcome = Some("request_error".into());
        input.min_duration_ms = Some(5_000);
        assert!(
            ResolvedQuery::resolve(&input, now)
                .unwrap()
                .validate_call_filters()
                .is_ok()
        );
        input.permission = Some("approved".into());
        assert!(
            ResolvedQuery::resolve(&input, now)
                .unwrap()
                .validate_call_filters()
                .is_err()
        );
        input.permission = Some("raw-untrusted-status".into());
        assert!(ResolvedQuery::resolve(&input, now).is_err());
        input.permission = None;
        input.min_duration_ms = Some(86_400_001);
        assert!(
            ResolvedQuery::resolve(&input, now)
                .unwrap()
                .validate_call_filters()
                .is_err()
        );
    }

    #[test]
    fn call_cursor_binds_role_filters_range_and_reception_boundary() {
        let now = 1_800_000_000_000;
        let input = MetricsQuery {
            from: Some(timestamp(now - 3_600_000)),
            to: Some(timestamp(now)),
            tool: Some("read_file".into()),
            limit: Some(1),
            ..Default::default()
        };
        let first = ResolvedQuery::resolve(&input, now).unwrap();
        let mut next = input.clone();
        next.cursor = Some(
            first
                .call_cursor(false, now, now - 1_000, "source.tool.0")
                .unwrap(),
        );
        let resolved = ResolvedQuery::resolve(&next, now + 5_000).unwrap();
        assert_eq!(
            resolved.call_boundary(false, now + 5_000).unwrap(),
            Some((now - 1_000, "source.tool.0".into(), now))
        );
        assert!(resolved.call_boundary(true, now + 5_000).is_err());
        let mut changed = next.clone();
        changed.tool = Some("read_image".into());
        assert!(
            ResolvedQuery::resolve(&changed, now + 5_000)
                .unwrap()
                .call_boundary(false, now + 5_000)
                .is_err()
        );
        changed = next.clone();
        changed.from = Some(timestamp(now - 7_200_000));
        assert!(
            ResolvedQuery::resolve(&changed, now + 5_000)
                .unwrap()
                .call_boundary(false, now + 5_000)
                .is_err()
        );
        changed = next.clone();
        changed.cursor = Some(
            first
                .call_cursor(false, now + 10_000, now - 1_000, "source.tool.0")
                .unwrap(),
        );
        assert!(
            ResolvedQuery::resolve(&changed, now + 5_000)
                .unwrap()
                .call_boundary(false, now + 5_000)
                .is_err()
        );
        changed = next;
        changed.to = None;
        assert!(ResolvedQuery::resolve(&changed, now + 5_000).is_err());
    }

    #[test]
    fn group_rate_sort_uses_exact_ratio_and_places_no_samples_last() {
        let make = |errors, returned| {
            let mut totals = Totals::default();
            totals.counts.insert(Count::RequestErrors, errors);
            totals.counts.insert(Count::Returned, returned);
            totals
        };
        let left = make(9_007_199_254_740_993, 1);
        let right = make(9_007_199_254_740_992, 1);
        assert_eq!(
            compare_groups(
                &left,
                &right,
                MetricGroupSort::RequestFailureRate,
                None,
                false
            ),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_groups(
                &left,
                &Totals::default(),
                MetricGroupSort::RequestFailureRate,
                None,
                false
            ),
            std::cmp::Ordering::Less
        );
        let mut totals = Totals::default();
        for index in 0..12 {
            totals
                .schema_errors
                .insert(format!("$.field_{index}"), index + 1);
        }
        totals.schema_errors_other = 3;
        let query = ResolvedQuery::resolve(&MetricsQuery::default(), 1_800_000_000_000).unwrap();
        let summary = summary(&totals, &query, false);
        assert_eq!(summary.schema_paths.len(), 10);
        assert_eq!(summary.schema_paths[0].count, "12");
        assert_eq!(summary.other_schema_errors.as_deref(), Some("6"));
        assert!(summary.schema_paths_limited);
        let mut sorted = query;
        sorted.group_sort = Some(MetricGroupSort::RequestFailureRate);
        assert!(sorted.validate_group_sort(false).is_ok());
        assert!(sorted.validate_group_sort(true).is_err());
        assert!(sorted.validate_call_filters().is_err());
        sorted.error = Some(ErrorSelector::Input(InputIssue::Type));
        assert!(sorted.validate_group_sort(false).is_err());
    }
    #[test]
    fn error_filter_changes_only_the_failure_numerator() {
        let query = ResolvedQuery::resolve(
            &MetricsQuery {
                error: Some("request.http_error".into()),
                ..Default::default()
            },
            1_800_000_000_000,
        )
        .unwrap();
        let mut totals = Totals::default();
        totals.counts.extend([
            (Count::Returned, 3),
            (Count::RequestErrors, 3),
            (Count::HttpErrors, 2),
        ]);
        totals.error_counts.insert(
            Rate::RequestFailure,
            std::collections::BTreeMap::from([(
                ErrorSelector::Request(RequestOutcome::HttpError),
                2,
            )]),
        );
        let summary = summary(&totals, &query, false);
        let rate = summary
            .rates
            .iter()
            .find(|rate| rate.key == "request_failure")
            .unwrap();
        assert_eq!(
            (rate.numerator.as_deref(), rate.denominator.as_deref()),
            (Some("2"), Some("6"))
        );
        assert_eq!(rate.value, Some(2.0 / 6.0));
        assert_eq!(rate.numerator_error.as_deref(), Some("request.http_error"));
        assert_eq!(rate.denominator_scope, "returned_or_known_request_error");
        assert!(rate.applied_filters.contains(&MetricFilter::Error));
        assert!(rate.unsupported_filters.is_empty());
    }
    #[test]
    fn selectors_are_bounded_and_tool_filtered_requests_are_not_applicable() {
        assert!(
            ResolvedQuery::resolve(
                &MetricsQuery {
                    purpose: Some("invented".into()),
                    ..Default::default()
                },
                1_800_000_000_000
            )
            .is_err()
        );
        let query = ResolvedQuery::resolve(
            &MetricsQuery {
                tool: Some("read_context".into()),
                ..Default::default()
            },
            1_800_000_000_000,
        )
        .unwrap();
        let value = summary(&Totals::default(), &query, false);
        assert_eq!(value.rates[0].sample_status, SampleStatus::NotApplicable);
        assert_eq!(value.rates[0].value, None);
        assert_eq!(value.rates[0].numerator, None);
        assert_eq!(
            value.rates[0].reason,
            Some(MetricRateReason::UnsupportedToolFilter)
        );
        assert_eq!(value.rates[0].unsupported_filters, vec![MetricFilter::Tool]);
        assert!(!value.rates[0].applied_filters.contains(&MetricFilter::Tool));
    }

    #[test]
    fn input_error_filters_keep_format_and_input_denominators_and_exclude_other_domains() {
        let query = ResolvedQuery::resolve(
            &MetricsQuery {
                error: Some("input.invalid_json".into()),
                ..Default::default()
            },
            1_800_000_000_000,
        )
        .unwrap();
        let mut totals = Totals::default();
        totals.counts.extend([
            (Count::FormatPassed, 5),
            (Count::FormatRejected, 3),
            (Count::InputAccepted, 5),
            (Count::InputRejected, 4),
        ]);
        let error = ErrorSelector::Input(InputIssue::InvalidJson);
        for rate in [Rate::ToolFormatFailure, Rate::InputRejection] {
            totals
                .error_counts
                .insert(rate, std::collections::BTreeMap::from([(error, 2)]));
        }
        let result = summary(&totals, &query, false);
        for (key, denominator) in [("tool_format_failure", "8"), ("input_rejection", "9")] {
            let rate = result.rates.iter().find(|rate| rate.key == key).unwrap();
            assert_eq!(
                (rate.numerator.as_deref(), rate.denominator.as_deref()),
                (Some("2"), Some(denominator))
            );
        }
        for key in [
            "request_failure",
            "output_rejection",
            "reference_failure",
            "next_input_acceptance",
            "execution_verification",
        ] {
            let rate = result.rates.iter().find(|rate| rate.key == key).unwrap();
            assert_eq!(rate.sample_status, SampleStatus::NotApplicable);
            assert_eq!(rate.value, None);
            assert!(rate.unsupported_filters.contains(&MetricFilter::Error));
        }
    }

    #[test]
    fn attempt_and_output_selectors_do_not_mix_similarly_named_input_issues() {
        let mut totals = Totals::default();
        totals.counts.extend([
            (Count::AttemptReturned, 3),
            (Count::AttemptErrors, 2),
            (Count::OutputAccepted, 3),
            (Count::OutputRejected, 2),
        ]);
        totals.error_counts.insert(
            Rate::AttemptFailure,
            std::collections::BTreeMap::from([(
                ErrorSelector::Request(RequestOutcome::Timeout),
                1,
            )]),
        );
        totals.error_counts.insert(
            Rate::OutputRejection,
            std::collections::BTreeMap::from([(
                ErrorSelector::Output(OutputOutcome::InvalidProtocol),
                1,
            )]),
        );
        for (selector, key) in [
            ("request.timeout", "attempt_failure"),
            ("output.invalid_protocol", "output_rejection"),
        ] {
            let query = ResolvedQuery::resolve(
                &MetricsQuery {
                    error: Some(selector.into()),
                    ..Default::default()
                },
                1_800_000_000_000,
            )
            .unwrap();
            let result = summary(&totals, &query, false);
            let rate = result.rates.iter().find(|rate| rate.key == key).unwrap();
            assert_eq!(
                (rate.numerator.as_deref(), rate.denominator.as_deref()),
                (Some("1"), Some("5"))
            );
        }
    }

    #[test]
    fn overflow_and_impossible_numerators_are_unavailable_instead_of_zero() {
        let query = ResolvedQuery::resolve(&MetricsQuery::default(), 1_800_000_000_000).unwrap();
        let mut totals = Totals::default();
        totals
            .counts
            .extend([(Count::Returned, u64::MAX), (Count::RequestErrors, 1)]);
        let result = summary(&totals, &query, false);
        let rate = &result.rates[0];
        assert_eq!(rate.sample_status, SampleStatus::Unavailable);
        assert_eq!(rate.numerator, None);
        assert_eq!(rate.denominator, None);
        totals
            .counts
            .extend([(Count::Returned, 2), (Count::UsageComplete, 3)]);
        let result = summary(&totals, &query, false);
        let rate = result
            .rates
            .iter()
            .find(|rate| rate.key == "basic_usage_coverage")
            .unwrap();
        assert_eq!(rate.reason, Some(MetricRateReason::ArithmeticUnavailable));
    }

    #[test]
    fn detail_filters_require_the_matching_object_domain() {
        let request = ResolvedQuery::resolve(
            &MetricsQuery {
                tool: Some("read_file".into()),
                error: Some("request.timeout".into()),
                ..Default::default()
            },
            1_800_000_000_000,
        )
        .unwrap();
        assert!(request.validate_call_filters().is_err());
        let input = ResolvedQuery::resolve(
            &MetricsQuery {
                tool: Some("read_file".into()),
                error: Some("input.reference_expired".into()),
                ..Default::default()
            },
            1_800_000_000_000,
        )
        .unwrap();
        assert!(input.validate_call_filters().is_ok());
    }

    #[test]
    fn periods_before_activation_are_not_collected_instead_of_zero_errors() {
        let now = 1_800_000_000_000;
        let query = ResolvedQuery::resolve(
            &MetricsQuery {
                to: Some(timestamp(now - 1_000)),
                ..Default::default()
            },
            now,
        )
        .unwrap();
        let coverage = query.coverage(now, now, false, false);
        assert_eq!(coverage.sample_status, SampleStatus::NotCollected);
        assert_eq!(coverage.not_collected_before, Some(timestamp(now)));
        let result = observed_summary(&Totals::default(), &query, false, now);
        for rate in result.rates {
            assert_eq!(rate.sample_status, SampleStatus::NotCollected);
            assert_eq!(
                (rate.numerator, rate.denominator, rate.value),
                (None, None, None)
            );
        }
        let mut current = query.clone();
        current.requested_to_ms = now + 1;
        assert_eq!(
            current.coverage(now, now, false, false).sample_status,
            SampleStatus::Partial
        );
    }
}
