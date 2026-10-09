//! Resource accounting policy, separate from quality and financial metrics.

use super::{InputConclusion, ObservationPayload, OperationOutcome, OutputOutcome, RequestOutcome};

pub const RESOURCE_RESERVE_BYTES: i64 = 1_048_576;
pub const CLEANUP_BATCH_ROWS: u64 = 1_000;

#[derive(Debug, Clone, Copy)]
pub enum StorageKind {
    Event,
    Compact,
    Detail,
    Rollup,
}

/// Charges owned UTF-8 bytes plus room for row headers, indexes and page overhead.
/// This conservative charge is not a measurement of allocated database pages.
pub fn charged_bytes(columns: &[&str]) -> Option<i64> {
    columns.iter().try_fold(4_096i64, |total, column| {
        total.checked_add(i64::try_from(column.len()).ok()?.checked_mul(2)?)
    })
}

pub fn data_budget(bytes: &str) -> Option<i64> {
    bytes
        .parse::<i64>()
        .ok()?
        .checked_sub(RESOURCE_RESERVE_BYTES)
        .filter(|value| *value > 0)
}

pub fn low_water(limit: i64) -> i64 {
    limit.saturating_mul(4) / 5
}

/// Successful details are disposable first; pending and failed facts live longer.
pub fn retention_priority(payload: &ObservationPayload) -> i32 {
    match payload {
        ObservationPayload::Call(call) => match call.outcome {
            RequestOutcome::Returned if call.output == OutputOutcome::Accepted => 0,
            RequestOutcome::Pending => 1,
            _ => 2,
        },
        ObservationPayload::Attempt(attempt) => match attempt.outcome {
            RequestOutcome::Returned => 0,
            RequestOutcome::Pending => 1,
            _ => 2,
        },
        ObservationPayload::Tool(tool) => match tool.conclusion {
            InputConclusion::Accepted => 0,
            InputConclusion::Unknown => 1,
            InputConclusion::Rejected => 2,
        },
        ObservationPayload::Operation(operation) => match operation.outcome {
            OperationOutcome::Verified | OperationOutcome::Accepted => 0,
            OperationOutcome::Pending => 1,
            _ => 2,
        },
        ObservationPayload::Runtime(_) => 1,
    }
}
