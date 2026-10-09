//! Deterministic contribution arithmetic shared by both observation stores.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::*;

closed_enum!(Count {
    Calls,
    NotStarted,
    Pending,
    Returned,
    RequestErrors,
    HttpErrors,
    ProviderErrors,
    TransportErrors,
    Timeouts,
    StreamErrors,
    Cancelled,
    Incomplete,
    Attempts,
    AttemptReturned,
    AttemptErrors,
    AttemptCancelled,
    AttemptIncomplete,
    OutputAccepted,
    OutputRejected,
    OutputTruncated,
    ContextLimit,
    PolicyRejected,
    Tools,
    FormatPassed,
    FormatRejected,
    FormatUnavailable,
    ReferencePassed,
    ReferenceFailed,
    ReferenceAttempted,
    InputAccepted,
    InputRejected,
    InputUnknown,
    PermissionWaiting,
    PermissionApproved,
    PermissionNarrowed,
    PermissionDenied,
    PermissionRevoked,
    PermissionPolicyRejected,
    PermissionUnavailable,
    PermissionExpired,
    PermissionCancelled,
    CorrectionResponded,
    CorrectionLinked,
    CorrectionAccepted,
    CorrectionRejected,
    CorrectionAwaiting,
    CorrectionAmbiguous,
    CorrectionNotComparable,
    CorrectionSwitched,
    CorrectionNoResponse,
    CorrectionGroups,
    CorrectionGroupAwaitingResponse,
    CorrectionGroupAwaitingValidation,
    CorrectionGroupInputAccepted,
    CorrectionGroupInputRejected,
    CorrectionGroupAmbiguous,
    CorrectionGroupAwaitingOutputCheck,
    CorrectionGroupOutputAccepted,
    CorrectionGroupOutputRejected,
    CorrectionGroupNotComparable,
    CorrectionGroupSwitched,
    CorrectionGroupNoResponse,
    CorrectionGroupUnavailable,
    CorrectionGroupToolInput,
    CorrectionGroupProtocol,
    CorrectionGroupSchedule,
    CorrectionGroupApproval,
    OperationsDispatched,
    OperationsDispatchUnknown,
    OperationsNotDispatched,
    OperationsRejected,
    OperationsVerified,
    OperationsAccepted,
    OperationsChangedUnverified,
    OperationsFailed,
    OperationsUnknown,
    OperationsCancelled,
    UsageComplete,
    UsagePartial,
    CachePairedRead,
    CachePairedInput,
    MeteredCalls,
    InputTokens,
    OutputTokens,
    CacheReadTokens,
    CacheWriteTokens,
    RuntimeEvents,
    RuntimeValue
});

closed_enum!(Rate {
    RequestFailure,
    AttemptFailure,
    OutputRejection,
    ToolFormatFailure,
    ReferenceFailure,
    InputRejection,
    NextInputAcceptance,
    ExecutionVerification,
    BasicUsageCoverage,
    CacheReadShare,
    CorrectionLinkCoverage,
    CorrectionConclusionCoverage
});

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Contribution {
    pub kind: ObjectKind,
    pub counts: BTreeMap<Count, u64>,
    pub error_counts: BTreeMap<Rate, BTreeMap<ErrorSelector, u64>>,
    pub quantities: BTreeMap<runtime::RuntimeQuantity, QuantityTotal>,
    pub duration_ms: Option<u64>,
    pub first_content_ms: Option<u64>,
    pub schema_error_path: Option<String>,
    pub stages: BTreeMap<Stage, StageOutcome>,
}

pub const MAX_SCHEMA_PATHS: usize = 64;
pub const TOOL_STAGES: [Stage; 9] = [
    Stage::Exposure,
    Stage::Protocol,
    Stage::Json,
    Stage::Schema,
    Stage::Reference,
    Stage::Preflight,
    Stage::Permission,
    Stage::Dispatch,
    Stage::Completion,
];

impl Contribution {
    pub fn get(&self, count: Count) -> u64 {
        self.counts.get(&count).copied().unwrap_or_default()
    }
    fn add(&mut self, count: Count, amount: u64) {
        self.counts.insert(count, amount);
    }
    fn error(&mut self, rate: Rate, selector: ErrorSelector) {
        if selector == ErrorSelector::Input(InputIssue::None) {
            return;
        }
        self.error_counts
            .entry(rate)
            .or_default()
            .insert(selector, 1);
    }
    pub fn error_numerator(&self, rate: Rate, selector: ErrorSelector) -> u64 {
        self.error_counts
            .get(&rate)
            .and_then(|counts| counts.get(&selector))
            .copied()
            .unwrap_or_default()
    }

    pub fn rate(&self, rate: Rate) -> Option<(u64, u64)> {
        let get = |key| self.get(key);
        let pair = match rate {
            Rate::RequestFailure => (
                get(Count::RequestErrors),
                get(Count::Returned).checked_add(get(Count::RequestErrors))?,
            ),
            Rate::AttemptFailure => (
                get(Count::AttemptErrors),
                get(Count::AttemptReturned).checked_add(get(Count::AttemptErrors))?,
            ),
            Rate::OutputRejection => (
                get(Count::OutputRejected),
                get(Count::OutputAccepted).checked_add(get(Count::OutputRejected))?,
            ),
            Rate::ToolFormatFailure => (
                get(Count::FormatRejected),
                get(Count::FormatPassed).checked_add(get(Count::FormatRejected))?,
            ),
            Rate::ReferenceFailure => (
                get(Count::ReferenceFailed),
                get(Count::ReferencePassed).checked_add(get(Count::ReferenceFailed))?,
            ),
            Rate::InputRejection => (
                get(Count::InputRejected),
                get(Count::InputAccepted).checked_add(get(Count::InputRejected))?,
            ),
            Rate::NextInputAcceptance => (
                get(Count::CorrectionAccepted),
                get(Count::CorrectionAccepted).checked_add(get(Count::CorrectionRejected))?,
            ),
            Rate::ExecutionVerification => (
                get(Count::OperationsVerified),
                [
                    Count::OperationsVerified,
                    Count::OperationsAccepted,
                    Count::OperationsChangedUnverified,
                    Count::OperationsFailed,
                    Count::OperationsUnknown,
                    Count::OperationsCancelled,
                    Count::OperationsRejected,
                ]
                .into_iter()
                .try_fold(0u64, |sum, key| sum.checked_add(get(key)))?,
            ),
            Rate::BasicUsageCoverage => (get(Count::UsageComplete), get(Count::Returned)),
            Rate::CacheReadShare => (get(Count::CachePairedRead), get(Count::CachePairedInput)),
            Rate::CorrectionLinkCoverage => (
                get(Count::CorrectionLinked),
                get(Count::CorrectionResponded),
            ),
            Rate::CorrectionConclusionCoverage => (
                get(Count::CorrectionAccepted).checked_add(get(Count::CorrectionRejected))?,
                get(Count::CorrectionLinked),
            ),
        };
        Some(pair)
    }
}

pub const RATES: [Rate; 12] = [
    Rate::RequestFailure,
    Rate::AttemptFailure,
    Rate::OutputRejection,
    Rate::ToolFormatFailure,
    Rate::ReferenceFailure,
    Rate::InputRejection,
    Rate::NextInputAcceptance,
    Rate::ExecutionVerification,
    Rate::BasicUsageCoverage,
    Rate::CacheReadShare,
    Rate::CorrectionLinkCoverage,
    Rate::CorrectionConclusionCoverage,
];

pub fn known_request_error(outcome: RequestOutcome) -> bool {
    matches!(
        outcome,
        RequestOutcome::HttpError
            | RequestOutcome::ProviderError
            | RequestOutcome::TransportError
            | RequestOutcome::Timeout
            | RequestOutcome::StreamError
    )
}

pub fn contribution(payload: &ObservationPayload, include_token_totals: bool) -> Contribution {
    let mut c = Contribution {
        kind: payload.kind(),
        ..Default::default()
    };
    match payload {
        ObservationPayload::Call(call) => {
            c.add(Count::Calls, 1);
            c.add(
                match call.outcome {
                    RequestOutcome::NotStarted => Count::NotStarted,
                    RequestOutcome::Pending => Count::Pending,
                    RequestOutcome::Returned => Count::Returned,
                    RequestOutcome::HttpError => Count::HttpErrors,
                    RequestOutcome::ProviderError => Count::ProviderErrors,
                    RequestOutcome::TransportError => Count::TransportErrors,
                    RequestOutcome::Timeout => Count::Timeouts,
                    RequestOutcome::StreamError => Count::StreamErrors,
                    RequestOutcome::Cancelled => Count::Cancelled,
                    RequestOutcome::ObservationIncomplete => Count::Incomplete,
                },
                1,
            );
            if known_request_error(call.outcome) {
                c.add(Count::RequestErrors, 1);
                c.error(Rate::RequestFailure, ErrorSelector::Request(call.outcome));
            }
            match call.output {
                OutputOutcome::Accepted => c.add(Count::OutputAccepted, 1),
                OutputOutcome::InvalidProtocol
                | OutputOutcome::InvalidStructuredOutput
                | OutputOutcome::EmptyResponse => {
                    c.add(Count::OutputRejected, 1);
                    c.error(Rate::OutputRejection, ErrorSelector::Output(call.output));
                }
                OutputOutcome::OutputTruncated => c.add(Count::OutputTruncated, 1),
                OutputOutcome::ContextLimit => c.add(Count::ContextLimit, 1),
                OutputOutcome::PolicyRejected => c.add(Count::PolicyRejected, 1),
                OutputOutcome::NotEvaluated => {}
            }
            if call.outcome == RequestOutcome::Returned {
                c.add(
                    if call.usage.basic_complete() {
                        Count::UsageComplete
                    } else {
                        Count::UsagePartial
                    },
                    1,
                );
                if let Some((read, input)) = call.usage.cache_pair() {
                    c.add(Count::CachePairedRead, read);
                    c.add(Count::CachePairedInput, input);
                }
            }
            if include_token_totals && call.metered_at_ms.is_some() {
                c.add(Count::MeteredCalls, 1);
                for (key, value) in [
                    (Count::InputTokens, call.usage.normalized.input_tokens),
                    (Count::OutputTokens, call.usage.normalized.output_tokens),
                    (
                        Count::CacheReadTokens,
                        call.usage.normalized.cache_read_tokens,
                    ),
                    (
                        Count::CacheWriteTokens,
                        call.usage.normalized.cache_write_tokens,
                    ),
                ] {
                    if let Some(value) = value.and_then(|v| u64::try_from(v).ok()) {
                        c.add(key, value);
                    }
                }
            }
            c.duration_ms = call.timing.duration_ms;
            c.first_content_ms = call.timing.first_content_ms;
        }
        ObservationPayload::Attempt(attempt) => {
            c.add(Count::Attempts, 1);
            if known_request_error(attempt.outcome) {
                c.add(Count::AttemptErrors, 1);
                c.error(
                    Rate::AttemptFailure,
                    ErrorSelector::Request(attempt.outcome),
                );
            }
            match attempt.outcome {
                RequestOutcome::Returned => c.add(Count::AttemptReturned, 1),
                RequestOutcome::Cancelled => c.add(Count::AttemptCancelled, 1),
                RequestOutcome::ObservationIncomplete => c.add(Count::AttemptIncomplete, 1),
                _ => {}
            }
            c.duration_ms = attempt.timing.duration_ms;
            c.first_content_ms = attempt.timing.first_content_ms;
        }
        ObservationPayload::Tool(tool) => {
            c.duration_ms = tool.stage_duration_ms;
            c.stages = TOOL_STAGES
                .into_iter()
                .map(|stage| {
                    (
                        stage,
                        tool.stages
                            .get(&stage)
                            .copied()
                            .unwrap_or(StageOutcome::NotReached),
                    )
                })
                .collect();
            if tool.conclusion == InputConclusion::Rejected
                && tool.stages.get(&Stage::Schema) == Some(&StageOutcome::Failed)
            {
                c.schema_error_path = tool.schema_path.clone();
            }
            c.add(Count::Tools, 1);
            let get = |stage| {
                tool.stages
                    .get(&stage)
                    .copied()
                    .unwrap_or(StageOutcome::NotReached)
            };
            if tool.issue == InputIssue::SchemaUnavailable {
                c.add(Count::FormatUnavailable, 1);
            } else if [Stage::Exposure, Stage::Protocol, Stage::Json, Stage::Schema]
                .into_iter()
                .any(|stage| get(stage) == StageOutcome::Failed)
            {
                c.add(Count::FormatRejected, 1);
            } else if get(Stage::Schema) == StageOutcome::Passed {
                c.add(Count::FormatPassed, 1);
            }
            if c.get(Count::FormatRejected) > 0 {
                c.error(Rate::ToolFormatFailure, ErrorSelector::Input(tool.issue));
            }
            match get(Stage::Reference) {
                StageOutcome::Passed => c.add(Count::ReferencePassed, 1),
                StageOutcome::Failed => {
                    c.add(Count::ReferenceFailed, 1);
                    c.error(Rate::ReferenceFailure, ErrorSelector::Input(tool.issue));
                }
                StageOutcome::Attempted => c.add(Count::ReferenceAttempted, 1),
                _ => {}
            }
            c.add(
                match tool.conclusion {
                    InputConclusion::Accepted => Count::InputAccepted,
                    InputConclusion::Rejected => Count::InputRejected,
                    InputConclusion::Unknown => Count::InputUnknown,
                },
                1,
            );
            if tool.conclusion == InputConclusion::Rejected {
                c.error(Rate::InputRejection, ErrorSelector::Input(tool.issue));
            }
            match tool.permission {
                PermissionOutcome::Waiting => c.add(Count::PermissionWaiting, 1),
                PermissionOutcome::Approved => c.add(Count::PermissionApproved, 1),
                PermissionOutcome::Narrowed => c.add(Count::PermissionNarrowed, 1),
                PermissionOutcome::Denied => c.add(Count::PermissionDenied, 1),
                PermissionOutcome::Revoked => c.add(Count::PermissionRevoked, 1),
                PermissionOutcome::PolicyRejected => c.add(Count::PermissionPolicyRejected, 1),
                PermissionOutcome::Unavailable => c.add(Count::PermissionUnavailable, 1),
                PermissionOutcome::Expired => c.add(Count::PermissionExpired, 1),
                PermissionOutcome::Cancelled => c.add(Count::PermissionCancelled, 1),
                PermissionOutcome::NotReached => {}
            }
            if matches!(
                tool.correction_status,
                CorrectionStatus::Linked | CorrectionStatus::Ambiguous
            ) {
                c.add(Count::CorrectionResponded, 1);
            }
            if tool.correction_status == CorrectionStatus::Linked {
                c.add(Count::CorrectionLinked, 1);
            }
            match tool.correction_status {
                CorrectionStatus::AwaitingResponse => c.add(Count::CorrectionAwaiting, 1),
                CorrectionStatus::Ambiguous => c.add(Count::CorrectionAmbiguous, 1),
                CorrectionStatus::NotComparable => c.add(Count::CorrectionNotComparable, 1),
                CorrectionStatus::Switched => c.add(Count::CorrectionSwitched, 1),
                CorrectionStatus::NoResponse => c.add(Count::CorrectionNoResponse, 1),
                _ => {}
            }
            if tool.correction_status == CorrectionStatus::Linked {
                match tool.correction_input {
                    Some(InputConclusion::Accepted) => c.add(Count::CorrectionAccepted, 1),
                    Some(InputConclusion::Rejected) => c.add(Count::CorrectionRejected, 1),
                    _ => {}
                }
            }
        }
        ObservationPayload::Operation(op) if op.dispatched == Some(true) => {
            c.add(Count::OperationsDispatched, 1);
            let key = match op.outcome {
                OperationOutcome::Verified => Some(Count::OperationsVerified),
                OperationOutcome::Accepted => Some(Count::OperationsAccepted),
                OperationOutcome::ChangedUnverified => Some(Count::OperationsChangedUnverified),
                OperationOutcome::Failed => Some(Count::OperationsFailed),
                OperationOutcome::Unknown => Some(Count::OperationsUnknown),
                OperationOutcome::Cancelled => Some(Count::OperationsCancelled),
                OperationOutcome::Rejected => Some(Count::OperationsRejected),
                OperationOutcome::Pending => None,
            };
            if let Some(key) = key {
                c.add(key, 1);
            }
            c.duration_ms = op.duration_ms;
        }
        ObservationPayload::Operation(op) => {
            c.add(
                if op.dispatched.is_none() {
                    Count::OperationsDispatchUnknown
                } else {
                    Count::OperationsNotDispatched
                },
                1,
            );
        }
        ObservationPayload::Runtime(runtime) => {
            c.add(Count::RuntimeEvents, 1);
            c.add(Count::RuntimeValue, runtime.value);
            c.duration_ms = runtime.duration_ms;
            c.quantities = runtime
                .quantities
                .iter()
                .map(|(key, value)| {
                    (
                        *key,
                        QuantityTotal {
                            count: 1,
                            sum: *value,
                        },
                    )
                })
                .collect();
        }
    }
    c
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub counts: BTreeMap<Count, u64>,
    pub error_counts: BTreeMap<Rate, BTreeMap<ErrorSelector, u64>>,
    pub quantities: BTreeMap<runtime::RuntimeQuantity, QuantityTotal>,
    pub duration: Histogram,
    pub first_content: Histogram,
    pub other_duration: BTreeMap<ObjectKind, Histogram>,
    pub other_first_content: BTreeMap<ObjectKind, Histogram>,
    pub schema_errors: BTreeMap<String, u64>,
    pub schema_errors_other: u64,
    pub stage_counts: BTreeMap<Stage, BTreeMap<StageOutcome, u64>>,
}

impl Totals {
    pub fn change(&mut self, old: &Contribution, new: &Contribution) -> Option<()> {
        let mut next = self.clone();
        if let Some(path) = &old.schema_error_path {
            if let Some(count) = next.schema_errors.get_mut(path) {
                *count = count.checked_sub(1)?;
            } else {
                next.schema_errors_other = next.schema_errors_other.checked_sub(1)?;
            }
        }
        if let Some(path) = &new.schema_error_path {
            next.add_schema_errors(path, 1)?;
        }
        for (stage, outcome) in &old.stages {
            let count = next
                .stage_counts
                .entry(*stage)
                .or_default()
                .entry(*outcome)
                .or_default();
            *count = count.checked_sub(1)?;
        }
        for (stage, outcome) in &new.stages {
            let count = next
                .stage_counts
                .entry(*stage)
                .or_default()
                .entry(*outcome)
                .or_default();
            *count = count.checked_add(1)?;
        }
        for key in old.counts.keys().chain(new.counts.keys()) {
            let value = self
                .counts
                .get(key)
                .copied()
                .unwrap_or_default()
                .checked_sub(old.get(*key))?
                .checked_add(new.get(*key))?;
            next.counts.insert(*key, value);
        }
        for (rate, errors) in &old.error_counts {
            let target = next.error_counts.entry(*rate).or_default();
            for (error, count) in errors {
                let current = target.entry(*error).or_default();
                *current = current.checked_sub(*count)?;
            }
        }
        for (rate, errors) in &new.error_counts {
            let target = next.error_counts.entry(*rate).or_default();
            for (error, count) in errors {
                let current = target.entry(*error).or_default();
                *current = current.checked_add(*count)?;
            }
        }
        for key in old.quantities.keys().chain(new.quantities.keys()) {
            let current = self.quantities.get(key).copied().unwrap_or_default();
            let previous = old.quantities.get(key).copied().unwrap_or_default();
            let incoming = new.quantities.get(key).copied().unwrap_or_default();
            next.quantities.insert(
                *key,
                QuantityTotal {
                    count: current
                        .count
                        .checked_sub(previous.count)?
                        .checked_add(incoming.count)?,
                    sum: current
                        .sum
                        .checked_sub(previous.sum)?
                        .checked_add(incoming.sum)?,
                },
            );
        }
        if new.kind == ObjectKind::Call {
            next.duration.change(old.duration_ms, new.duration_ms)?;
            next.first_content
                .change(old.first_content_ms, new.first_content_ms)?;
        } else {
            next.other_duration
                .entry(new.kind)
                .or_default()
                .change(old.duration_ms, new.duration_ms)?;
            next.other_first_content
                .entry(new.kind)
                .or_default()
                .change(old.first_content_ms, new.first_content_ms)?;
        }
        *self = next;
        Some(())
    }

    pub fn sum(&mut self, other: &Self) -> Option<()> {
        let mut next = self.clone();
        for (path, count) in &other.schema_errors {
            next.add_schema_errors(path, *count)?;
        }
        next.schema_errors_other = next
            .schema_errors_other
            .checked_add(other.schema_errors_other)?;
        for (stage, outcomes) in &other.stage_counts {
            for (outcome, count) in outcomes {
                let current = next
                    .stage_counts
                    .entry(*stage)
                    .or_default()
                    .entry(*outcome)
                    .or_default();
                *current = current.checked_add(*count)?;
            }
        }
        for (key, value) in &other.counts {
            let total = next.counts.entry(*key).or_default();
            *total = total.checked_add(*value)?;
        }
        for (rate, errors) in &other.error_counts {
            let target = next.error_counts.entry(*rate).or_default();
            for (error, count) in errors {
                let current = target.entry(*error).or_default();
                *current = current.checked_add(*count)?;
            }
        }
        for (key, value) in &other.quantities {
            let current = next.quantities.entry(*key).or_default();
            current.count = current.count.checked_add(value.count)?;
            current.sum = current.sum.checked_add(value.sum)?;
        }
        next.duration.sum(&other.duration)?;
        next.first_content.sum(&other.first_content)?;
        for (kind, histogram) in &other.other_duration {
            next.other_duration
                .entry(*kind)
                .or_default()
                .sum(histogram)?;
        }
        for (kind, histogram) in &other.other_first_content {
            next.other_first_content
                .entry(*kind)
                .or_default()
                .sum(histogram)?;
        }
        *self = next;
        Some(())
    }

    pub fn contribution(&self) -> Contribution {
        Contribution {
            kind: ObjectKind::Call,
            counts: self.counts.clone(),
            error_counts: self.error_counts.clone(),
            quantities: self.quantities.clone(),
            duration_ms: None,
            first_content_ms: None,
            schema_error_path: None,
            stages: BTreeMap::new(),
        }
    }

    fn add_schema_errors(&mut self, path: &str, count: u64) -> Option<()> {
        // Retain zero-valued keys so later reversals preserve the original overflow assignment.
        if self.schema_errors.contains_key(path) || self.schema_errors.len() < MAX_SCHEMA_PATHS {
            let total = self.schema_errors.entry(path.into()).or_default();
            *total = total.checked_add(count)?;
        } else {
            self.schema_errors_other = self.schema_errors_other.checked_add(count)?;
        }
        Some(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QuantityTotal {
    pub count: u64,
    pub sum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Histogram {
    pub buckets: [u64; 14],
    pub count: u64,
    pub sum_ms: u64,
    pub min_ms: Option<u64>,
    pub max_ms: Option<u64>,
    pub extrema_unavailable: bool,
}

impl Histogram {
    fn change(&mut self, old: Option<u64>, new: Option<u64>) -> Option<()> {
        if old == new {
            return Some(());
        }
        if let Some(value) = old {
            let bucket = LATENCY_BUCKETS_MS
                .iter()
                .position(|limit| value <= *limit)?;
            self.buckets[bucket] = self.buckets[bucket].checked_sub(1)?;
            self.count = self.count.checked_sub(1)?;
            self.sum_ms = self.sum_ms.checked_sub(value)?;
            if self.min_ms == Some(value) || self.max_ms == Some(value) {
                self.min_ms = None;
                self.max_ms = None;
                self.extrema_unavailable = true;
            }
        }
        if let Some(value) = new {
            let bucket = LATENCY_BUCKETS_MS
                .iter()
                .position(|limit| value <= *limit)?;
            self.buckets[bucket] = self.buckets[bucket].checked_add(1)?;
            self.count = self.count.checked_add(1)?;
            self.sum_ms = self.sum_ms.checked_add(value)?;
            if !self.extrema_unavailable {
                self.min_ms = Some(self.min_ms.map_or(value, |min| min.min(value)));
                self.max_ms = Some(self.max_ms.map_or(value, |max| max.max(value)));
            }
        }
        Some(())
    }

    fn sum(&mut self, other: &Self) -> Option<()> {
        for (value, increment) in self.buckets.iter_mut().zip(other.buckets) {
            *value = value.checked_add(increment)?;
        }
        self.count = self.count.checked_add(other.count)?;
        self.sum_ms = self.sum_ms.checked_add(other.sum_ms)?;
        self.extrema_unavailable |= other.extrema_unavailable;
        if self.extrema_unavailable {
            self.min_ms = None;
            self.max_ms = None;
        } else {
            self.min_ms = self.min_ms.into_iter().chain(other.min_ms).min();
            self.max_ms = self.max_ms.into_iter().chain(other.max_ms).max();
        }
        Some(())
    }

    pub fn percentile(&self, percentile: u32) -> Option<u64> {
        if self.count == 0 || percentile > 100 {
            return None;
        }
        let rank = ((u128::from(self.count) * u128::from(percentile)).div_ceil(100)).max(1);
        let mut running = 0u128;
        for (limit, count) in LATENCY_BUCKETS_MS.iter().zip(self.buckets) {
            running += u128::from(count);
            if running >= rank {
                return (*limit != u64::MAX).then_some(*limit);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeResult {
    Applied,
    Duplicate,
    Conflict,
}

/// Merge independent lifecycle axes without letting late start events undo facts.
pub fn merge(current: &mut ObservationEvent, incoming: &ObservationEvent) -> MergeResult {
    let mut candidate = current.clone();
    let result = merge_candidate(&mut candidate, incoming);
    if result == MergeResult::Applied {
        *current = candidate;
    }
    result
}

fn merge_candidate(current: &mut ObservationEvent, incoming: &ObservationEvent) -> MergeResult {
    if current.object_id != incoming.object_id
        || current.attribution != incoming.attribution
        || current.started_at_ms != incoming.started_at_ms
    {
        return MergeResult::Conflict;
    }
    if current.payload == incoming.payload {
        return MergeResult::Duplicate;
    }
    if incoming.phase == ObservationPhase::Started {
        return MergeResult::Duplicate;
    }
    let compatible_terminal = |old, new| {
        old == new
            || matches!(
                old,
                RequestOutcome::NotStarted
                    | RequestOutcome::Pending
                    | RequestOutcome::ObservationIncomplete
            )
    };
    match (&mut current.payload, &incoming.payload) {
        (ObservationPayload::Call(old), ObservationPayload::Call(new)) => match incoming.phase {
            ObservationPhase::Output => {
                if old.output != OutputOutcome::NotEvaluated && old.output != new.output {
                    return MergeResult::Conflict;
                }
                old.output = new.output;
                old.timing.output_check_ms = new.timing.output_check_ms;
            }
            ObservationPhase::Metered => {
                let Some(metered) = new.metered_at_ms else {
                    return MergeResult::Conflict;
                };
                if old
                    .metered_at_ms
                    .is_some_and(|previous| previous != metered)
                {
                    return MergeResult::Conflict;
                }
                old.metered_at_ms = Some(metered);
            }
            _ => {
                if new.outcome == RequestOutcome::Pending
                    && !matches!(
                        old.outcome,
                        RequestOutcome::NotStarted | RequestOutcome::Pending
                    )
                {
                    return MergeResult::Duplicate;
                }
                if !compatible_terminal(old.outcome, new.outcome) {
                    return MergeResult::Conflict;
                }
                let output = old.output;
                let metered = old.metered_at_ms;
                *old = new.clone();
                if old.output == OutputOutcome::NotEvaluated {
                    old.output = output;
                }
                if old.metered_at_ms.is_none() {
                    old.metered_at_ms = metered;
                }
            }
        },
        (ObservationPayload::Attempt(old), ObservationPayload::Attempt(new)) => {
            if new.outcome == RequestOutcome::Pending
                && !matches!(
                    old.outcome,
                    RequestOutcome::NotStarted | RequestOutcome::Pending
                )
            {
                return MergeResult::Duplicate;
            }
            if !compatible_terminal(old.outcome, new.outcome) {
                return MergeResult::Conflict;
            }
            *old = new.clone();
        }
        (ObservationPayload::Tool(old), ObservationPayload::Tool(new)) => {
            if old.ordinal != new.ordinal || old.tool_key != new.tool_key {
                return MergeResult::Conflict;
            }
            if incoming.sequence < current.sequence {
                return MergeResult::Duplicate;
            }
            // Ordinary progress snapshots may still carry an earlier approval.
            // A denied or narrowed original input cannot be widened by replay.
            let replayed_approval = matches!(
                old.permission,
                PermissionOutcome::Denied
                    | PermissionOutcome::Revoked
                    | PermissionOutcome::PolicyRejected
                    | PermissionOutcome::Unavailable
                    | PermissionOutcome::Expired
                    | PermissionOutcome::Cancelled
            ) && matches!(
                new.permission,
                PermissionOutcome::Approved | PermissionOutcome::Narrowed
            ) || old.permission == PermissionOutcome::Narrowed
                && new.permission == PermissionOutcome::Approved;
            for (stage, result) in &new.stages {
                if let Some(previous) = old.stages.get(stage)
                    && matches!(previous, StageOutcome::Passed | StageOutcome::Failed)
                    && !matches!(*stage, Stage::Permission | Stage::Dispatch)
                    && previous != result
                    && matches!(result, StageOutcome::Passed | StageOutcome::Failed)
                {
                    return MergeResult::Conflict;
                }
            }
            for (stage, result) in &new.stages {
                if replayed_approval && *stage == Stage::Permission {
                    continue;
                }
                let previous = old
                    .stages
                    .get(stage)
                    .copied()
                    .unwrap_or(StageOutcome::NotReached);
                if *result != StageOutcome::NotReached
                    && !(matches!(
                        previous,
                        StageOutcome::Passed | StageOutcome::Failed | StageOutcome::NotApplicable
                    ) && *result == StageOutcome::Attempted)
                {
                    old.stages.insert(*stage, *result);
                }
            }
            if new.conclusion != InputConclusion::Unknown {
                if old.conclusion != InputConclusion::Unknown && old.conclusion != new.conclusion {
                    return MergeResult::Conflict;
                }
                old.conclusion = new.conclusion;
                old.issue = new.issue;
                old.schema_path = new.schema_path.clone();
            }
            if new.issue == InputIssue::SchemaUnavailable && old.issue == InputIssue::None {
                old.issue = new.issue;
                old.schema_path = new.schema_path.clone();
            }
            if !replayed_approval
                && new.permission != PermissionOutcome::NotReached
                && !(matches!(
                    old.permission,
                    PermissionOutcome::Approved
                        | PermissionOutcome::Narrowed
                        | PermissionOutcome::Denied
                        | PermissionOutcome::Revoked
                        | PermissionOutcome::PolicyRejected
                        | PermissionOutcome::Unavailable
                        | PermissionOutcome::Expired
                        | PermissionOutcome::Cancelled
                ) && new.permission == PermissionOutcome::Waiting)
            {
                old.permission = new.permission;
            }
            if new.correction_status != CorrectionStatus::Uncorrelated {
                match (old.correction_status, new.correction_status) {
                    (CorrectionStatus::Uncorrelated | CorrectionStatus::AwaitingResponse, _) => {
                        old.correction_status = new.correction_status
                    }
                    (_, CorrectionStatus::AwaitingResponse) => {}
                    (previous, incoming) if previous == incoming => {}
                    _ => return MergeResult::Conflict,
                }
            }
            if let Some(conclusion) = new.correction_input {
                if old.correction_input.is_some_and(|previous| {
                    previous != InputConclusion::Unknown && previous != conclusion
                }) {
                    return MergeResult::Conflict;
                }
                if conclusion != InputConclusion::Unknown {
                    old.correction_input = Some(conclusion);
                }
            }
            if let Some(parent) = &new.correction_of {
                if old
                    .correction_of
                    .as_ref()
                    .is_some_and(|previous| previous != parent)
                {
                    return MergeResult::Conflict;
                }
                old.correction_of = Some(parent.clone());
            }
            old.stage_duration_ms = new.stage_duration_ms.or(old.stage_duration_ms);
        }
        (ObservationPayload::Operation(old), ObservationPayload::Operation(new)) => {
            if old.ordinal != new.ordinal
                || old
                    .tool_observation_id
                    .as_ref()
                    .zip(new.tool_observation_id.as_ref())
                    .is_some_and(|(old, new)| old != new)
                || old
                    .tool_key
                    .as_ref()
                    .zip(new.tool_key.as_ref())
                    .is_some_and(|(old, new)| old != new)
            {
                return MergeResult::Conflict;
            }
            if new.outcome == OperationOutcome::Pending && old.outcome != OperationOutcome::Pending
            {
                return MergeResult::Duplicate;
            }
            if incoming.phase == ObservationPhase::Progress
                && !matches!(
                    old.outcome,
                    OperationOutcome::Pending | OperationOutcome::Unknown
                )
            {
                return MergeResult::Duplicate;
            }
            if !matches!(
                old.outcome,
                OperationOutcome::Pending | OperationOutcome::Unknown
            ) && old.outcome != new.outcome
            {
                return MergeResult::Conflict;
            }
            if old
                .dispatched
                .zip(new.dispatched)
                .is_some_and(|(old, new)| old != new)
            {
                return MergeResult::Conflict;
            }
            old.dispatched = new.dispatched.or(old.dispatched);
            old.outcome = new.outcome;
            old.duration_ms = new.duration_ms.or(old.duration_ms);
            if old.tool_observation_id.is_none() {
                old.tool_observation_id = new.tool_observation_id.clone();
            }
            if old.tool_key.is_none() {
                old.tool_key = new.tool_key.clone();
            }
        }
        _ => return MergeResult::Conflict,
    }
    current.occurred_at_ms = current.occurred_at_ms.max(incoming.occurred_at_ms);
    current.sequence = current.sequence.max(incoming.sequence);
    current.phase = incoming.phase;
    MergeResult::Applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_path_budget_keeps_reversal_assignment_and_exact_other_count() {
        let mut totals = Totals::default();
        let empty = Contribution::default();
        for index in 0..MAX_SCHEMA_PATHS {
            let input = Contribution {
                schema_error_path: Some(format!("$.field_{index}")),
                ..Default::default()
            };
            totals.change(&empty, &input).unwrap();
        }
        let overflow = Contribution {
            schema_error_path: Some("$.overflow".into()),
            ..Default::default()
        };
        totals.change(&empty, &overflow).unwrap();
        assert_eq!(totals.schema_errors.len(), MAX_SCHEMA_PATHS);
        assert_eq!(totals.schema_errors_other, 1);
        let recorded = Contribution {
            schema_error_path: Some("$.field_0".into()),
            ..Default::default()
        };
        totals.change(&recorded, &empty).unwrap();
        totals.change(&overflow, &empty).unwrap();
        assert_eq!(totals.schema_errors_other, 0);
        assert_eq!(totals.schema_errors["$.field_0"], 0);
        totals.change(&empty, &overflow).unwrap();
        assert_eq!(totals.schema_errors_other, 1);
        assert!(!totals.schema_errors.contains_key("$.overflow"));
        let mut sum = Totals::default();
        sum.sum(&totals).unwrap();
        sum.sum(&totals).unwrap();
        assert_eq!(sum.schema_errors_other, 2);
        assert_eq!(sum.schema_errors.values().sum::<u64>(), 126);
    }

    #[test]
    fn stage_replacement_and_input_duration_remain_separate_from_permission_and_execution() {
        let mut observed = correction_event(CorrectionStatus::Uncorrelated, None);
        let ObservationPayload::Tool(tool) = &mut observed.payload else {
            unreachable!();
        };
        tool.schema_path = Some("$.items[].count".into());
        tool.stages.insert(Stage::Schema, StageOutcome::Failed);
        tool.stage_duration_ms = Some(12);
        let rejected = contribution(&observed.payload, false);
        let mut totals = Totals::default();
        totals.change(&Contribution::default(), &rejected).unwrap();
        let ObservationPayload::Tool(tool) = &mut observed.payload else {
            unreachable!();
        };
        tool.permission = PermissionOutcome::Denied;
        let denied = contribution(&observed.payload, false);
        totals.change(&rejected, &denied).unwrap();
        assert_eq!(totals.schema_errors["$.items[].count"], 1);
        assert_eq!(
            totals.stage_counts[&Stage::Schema][&StageOutcome::Failed],
            1
        );
        assert_eq!(totals.other_duration[&ObjectKind::Tool].count, 1);
        assert_eq!(totals.other_duration[&ObjectKind::Tool].sum_ms, 12);
        assert_eq!(totals.contribution().get(Count::InputRejected), 1);
        assert_eq!(totals.contribution().get(Count::PermissionDenied), 1);
        assert_eq!(totals.contribution().get(Count::OperationsDispatched), 0);
        assert!(totals.duration.count == 0);
        assert!(safe_schema_path("$.items[].*"));
        for path in [
            "secret",
            "$.[12]",
            "$.items[12].count",
            "$.file/path",
            "$.https://example.invalid",
            "$.items\nsecret",
        ] {
            assert!(!safe_schema_path(path));
        }
    }

    fn correction_event(
        status: CorrectionStatus,
        input: Option<InputConclusion>,
    ) -> ObservationEvent {
        ObservationEvent {
            schema_version: EVENT_SCHEMA_VERSION,
            event_id: "call.tool.0.stage".into(),
            object_id: "call.tool.0".into(),
            call_id: Some("call".into()),
            phase: ObservationPhase::Stage,
            sequence: 10,
            started_at_ms: 1_000,
            occurred_at_ms: 2_000,
            relation: None,
            attribution: Attribution {
                provider_id: "provider".into(),
                model_id: "model".into(),
                model_name: "Model".into(),
                configuration_revision: "1".into(),
                contract_revision: "1".into(),
                surface: Surface::Assistant,
                purpose: Purpose::Agent,
                origin: Origin::User,
                configuration_scope: ConfigurationScope::Local,
                protocol: Protocol::OpenAiChatCompletions,
            },
            payload: ObservationPayload::Tool(ToolSnapshot {
                ordinal: 0,
                tool_key: "server_tool".into(),
                stages: BTreeMap::new(),
                conclusion: InputConclusion::Rejected,
                issue: InputIssue::Type,
                schema_path: None,
                permission: PermissionOutcome::NotReached,
                correction_of: None,
                correction_status: status,
                correction_input: input,
                argument_bytes: 1,
                stage_duration_ms: None,
            }),
        }
    }

    #[test]
    fn correction_ratios_belong_to_original_feedback_and_exclude_switched_scopes() {
        let original = correction_event(CorrectionStatus::Linked, Some(InputConclusion::Accepted));
        let contribution = contribution(&original.payload, false);
        assert_eq!(contribution.rate(Rate::NextInputAcceptance), Some((1, 1)));
        assert_eq!(
            contribution.rate(Rate::CorrectionConclusionCoverage),
            Some((1, 1))
        );
        assert_eq!(contribution.get(Count::InputRejected), 1);
        let mut next = correction_event(CorrectionStatus::Uncorrelated, None);
        let ObservationPayload::Tool(tool) = &mut next.payload else {
            unreachable!();
        };
        tool.correction_of = Some(original.object_id);
        tool.conclusion = InputConclusion::Accepted;
        assert_eq!(
            super::contribution(&next.payload, false).get(Count::CorrectionAccepted),
            0
        );
        for status in [
            CorrectionStatus::Switched,
            CorrectionStatus::NotComparable,
            CorrectionStatus::NoResponse,
        ] {
            let value = super::contribution(&correction_event(status, None).payload, false);
            assert_eq!(value.rate(Rate::CorrectionLinkCoverage), Some((0, 0)));
        }
    }

    #[test]
    fn ordinary_progress_cannot_reopen_a_linked_opportunity_or_change_a_known_verdict() {
        let mut current =
            correction_event(CorrectionStatus::Linked, Some(InputConclusion::Accepted));
        let mut replay = correction_event(CorrectionStatus::AwaitingResponse, None);
        replay.sequence = 11;
        assert_ne!(merge(&mut current, &replay), MergeResult::Conflict);
        let ObservationPayload::Tool(tool) = &current.payload else {
            unreachable!();
        };
        assert_eq!(tool.correction_status, CorrectionStatus::Linked);
        assert_eq!(tool.correction_input, Some(InputConclusion::Accepted));
        let before = current.clone();
        let mut conflicting =
            correction_event(CorrectionStatus::Linked, Some(InputConclusion::Rejected));
        conflicting.sequence = 12;
        assert_eq!(merge(&mut current, &conflicting), MergeResult::Conflict);
        assert_eq!(current, before);
    }

    #[test]
    fn qualified_denominator_excludes_unstarted_cancelled_and_incomplete() {
        let mut totals = Totals::default();
        for (outcome, n) in [
            (RequestOutcome::NotStarted, 2),
            (RequestOutcome::Returned, 3),
            (RequestOutcome::HttpError, 2),
            (RequestOutcome::Timeout, 1),
            (RequestOutcome::Cancelled, 1),
            (RequestOutcome::ObservationIncomplete, 1),
        ] {
            for _ in 0..n {
                let payload = ObservationPayload::Call(CallSnapshot {
                    outcome,
                    ..Default::default()
                });
                totals
                    .change(&Contribution::default(), &contribution(&payload, false))
                    .unwrap();
            }
        }
        assert_eq!(
            totals.contribution().rate(Rate::RequestFailure),
            Some((3, 6))
        );
        assert_eq!(totals.contribution().get(Count::Calls), 10);
        assert_eq!(
            Contribution::default().rate(Rate::RequestFailure),
            Some((0, 0))
        );
    }

    #[test]
    fn late_terminal_retracts_incomplete_and_extrema_are_honest() {
        let mut totals = Totals::default();
        let first = contribution(
            &ObservationPayload::Call(CallSnapshot {
                outcome: RequestOutcome::ObservationIncomplete,
                timing: Timing {
                    duration_ms: Some(100),
                    ..Default::default()
                },
                ..Default::default()
            }),
            false,
        );
        let terminal = contribution(
            &ObservationPayload::Call(CallSnapshot {
                outcome: RequestOutcome::Returned,
                timing: Timing {
                    duration_ms: Some(500),
                    ..Default::default()
                },
                ..Default::default()
            }),
            false,
        );
        totals.change(&Contribution::default(), &first).unwrap();
        totals.change(&first, &terminal).unwrap();
        assert_eq!(totals.contribution().get(Count::Calls), 1);
        assert_eq!(totals.contribution().get(Count::Incomplete), 0);
        assert_eq!(totals.duration.count, 1);
        assert_eq!(totals.duration.percentile(95), Some(500));
        assert!(totals.duration.extrema_unavailable);
    }

    #[test]
    fn overflow_leaves_existing_totals_untouched() {
        let mut totals = Totals::default();
        totals.counts.insert(Count::Calls, u64::MAX);
        let before = totals.clone();
        let next = contribution(&ObservationPayload::Call(CallSnapshot::default()), false);
        assert_eq!(totals.change(&Contribution::default(), &next), None);
        assert_eq!(totals, before);
    }

    #[test]
    fn error_numerators_follow_atomic_lifecycle_replacements() {
        let error = ErrorSelector::Request(RequestOutcome::Timeout);
        let incomplete = contribution(
            &ObservationPayload::Call(CallSnapshot {
                outcome: RequestOutcome::ObservationIncomplete,
                ..Default::default()
            }),
            false,
        );
        let failed = contribution(
            &ObservationPayload::Call(CallSnapshot {
                outcome: RequestOutcome::Timeout,
                ..Default::default()
            }),
            false,
        );
        let mut totals = Totals::default();
        totals
            .change(&Contribution::default(), &incomplete)
            .unwrap();
        totals.change(&incomplete, &failed).unwrap();
        assert_eq!(totals.contribution().get(Count::Calls), 1);
        assert_eq!(
            totals
                .contribution()
                .error_numerator(Rate::RequestFailure, error),
            1
        );
        let mut combined = Totals::default();
        combined.sum(&totals).unwrap();
        combined.sum(&totals).unwrap();
        assert_eq!(
            combined
                .contribution()
                .error_numerator(Rate::RequestFailure, error),
            2
        );
        combined.change(&failed, &Contribution::default()).unwrap();
        assert_eq!(
            combined
                .contribution()
                .error_numerator(Rate::RequestFailure, error),
            1
        );
        let before = combined.clone();
        let mut underflow = failed.clone();
        underflow
            .error_counts
            .get_mut(&Rate::RequestFailure)
            .unwrap()
            .insert(error, 2);
        assert_eq!(combined.change(&underflow, &Contribution::default()), None);
        assert_eq!(combined, before);
    }

    #[test]
    fn input_and_output_protocol_errors_have_independent_qualified_counts() {
        let mut totals = Totals::default();
        let output = contribution(
            &ObservationPayload::Call(CallSnapshot {
                outcome: RequestOutcome::Returned,
                output: OutputOutcome::InvalidProtocol,
                ..Default::default()
            }),
            false,
        );
        let input = contribution(
            &ObservationPayload::Tool(ToolSnapshot {
                ordinal: 0,
                tool_key: "read_file".into(),
                stages: BTreeMap::from([(Stage::Protocol, StageOutcome::Failed)]),
                conclusion: InputConclusion::Rejected,
                issue: InputIssue::InvalidProtocol,
                schema_path: None,
                permission: PermissionOutcome::NotReached,
                correction_of: None,
                correction_status: CorrectionStatus::Uncorrelated,
                correction_input: None,
                argument_bytes: 0,
                stage_duration_ms: None,
            }),
            false,
        );
        totals.change(&Contribution::default(), &output).unwrap();
        totals.change(&Contribution::default(), &input).unwrap();
        let result = totals.contribution();
        assert_eq!(
            result.error_numerator(
                Rate::OutputRejection,
                ErrorSelector::Output(OutputOutcome::InvalidProtocol)
            ),
            1
        );
        assert_eq!(
            result.error_numerator(
                Rate::ToolFormatFailure,
                ErrorSelector::Input(InputIssue::InvalidProtocol)
            ),
            1
        );
        assert_eq!(
            result.error_numerator(
                Rate::OutputRejection,
                ErrorSelector::Input(InputIssue::InvalidProtocol)
            ),
            0
        );
    }

    #[test]
    fn permission_outcomes_have_mutually_exclusive_counts_and_never_reject_input() {
        let cases = [
            (PermissionOutcome::Waiting, Count::PermissionWaiting),
            (PermissionOutcome::Approved, Count::PermissionApproved),
            (PermissionOutcome::Narrowed, Count::PermissionNarrowed),
            (PermissionOutcome::Denied, Count::PermissionDenied),
            (PermissionOutcome::Revoked, Count::PermissionRevoked),
            (
                PermissionOutcome::PolicyRejected,
                Count::PermissionPolicyRejected,
            ),
            (PermissionOutcome::Unavailable, Count::PermissionUnavailable),
            (PermissionOutcome::Expired, Count::PermissionExpired),
            (PermissionOutcome::Cancelled, Count::PermissionCancelled),
        ];
        let mut event = correction_event(CorrectionStatus::Uncorrelated, None);
        let ObservationPayload::Tool(tool) = &mut event.payload else {
            unreachable!();
        };
        tool.conclusion = InputConclusion::Accepted;
        tool.issue = InputIssue::None;
        for (permission, expected) in cases {
            let ObservationPayload::Tool(tool) = &mut event.payload else {
                unreachable!();
            };
            tool.permission = permission;
            let observed = contribution(&event.payload, false);
            for (_, count) in cases {
                assert_eq!(observed.get(count), u64::from(count == expected));
            }
            assert_eq!(observed.get(Count::Tools), 1);
            assert_eq!(observed.get(Count::InputAccepted), 1);
            assert_eq!(observed.get(Count::InputRejected), 0);
            assert_eq!(observed.get(Count::OperationsDispatched), 0);
        }
        let ObservationPayload::Tool(tool) = &mut event.payload else {
            unreachable!();
        };
        tool.permission = PermissionOutcome::NotReached;
        assert_eq!(
            cases
                .into_iter()
                .map(|(_, count)| contribution(&event.payload, false).get(count))
                .sum::<u64>(),
            0
        );
    }

    #[test]
    fn terminal_permission_replaces_waiting_and_replayed_snapshots_cannot_widen_it() {
        for (permission, key) in [
            (PermissionOutcome::Narrowed, Count::PermissionNarrowed),
            (PermissionOutcome::Denied, Count::PermissionDenied),
            (PermissionOutcome::Revoked, Count::PermissionRevoked),
            (
                PermissionOutcome::PolicyRejected,
                Count::PermissionPolicyRejected,
            ),
            (PermissionOutcome::Unavailable, Count::PermissionUnavailable),
            (PermissionOutcome::Expired, Count::PermissionExpired),
            (PermissionOutcome::Cancelled, Count::PermissionCancelled),
        ] {
            let mut current = correction_event(CorrectionStatus::Uncorrelated, None);
            let ObservationPayload::Tool(tool) = &mut current.payload else {
                unreachable!();
            };
            tool.conclusion = InputConclusion::Accepted;
            tool.issue = InputIssue::None;
            tool.permission = PermissionOutcome::Waiting;
            let mut totals = Totals::default();
            let before = contribution(&current.payload, false);
            totals.change(&Contribution::default(), &before).unwrap();
            let mut terminal = current.clone();
            terminal.sequence += 1;
            let ObservationPayload::Tool(tool) = &mut terminal.payload else {
                unreachable!();
            };
            tool.permission = permission;
            assert_eq!(merge(&mut current, &terminal), MergeResult::Applied);
            let after = contribution(&current.payload, false);
            totals.change(&before, &after).unwrap();
            for replayed in [PermissionOutcome::Waiting, PermissionOutcome::Approved] {
                let mut replay = current.clone();
                replay.sequence += 1;
                let ObservationPayload::Tool(tool) = &mut replay.payload else {
                    unreachable!();
                };
                tool.permission = replayed;
                let before = contribution(&current.payload, false);
                assert_ne!(merge(&mut current, &replay), MergeResult::Conflict);
                totals
                    .change(&before, &contribution(&current.payload, false))
                    .unwrap();
            }
            let observed = totals.contribution();
            assert_eq!(observed.get(key), 1);
            assert_eq!(observed.get(Count::PermissionWaiting), 0);
            assert_eq!(observed.get(Count::PermissionApproved), 0);
            assert_eq!(observed.get(Count::Tools), 1);
            assert_eq!(observed.get(Count::InputAccepted), 1);
            assert_eq!(observed.get(Count::InputRejected), 0);
        }
    }
}
