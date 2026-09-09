//! Deterministic UTC rule validation and strictly-future occurrence calculation.

use chrono::{DateTime, Datelike, Duration, NaiveTime, SecondsFormat, Utc};
use desk_agent_protocol::schedule::{
    MAX_SCHEDULE_SPEC_BYTES, SCHEDULE_SCHEMA_VERSION, ScheduleRule, ScheduleSpec,
};

/// Requirements must fit the existing conversation input admission limit.
pub const MAX_SCHEDULE_PROMPT_BYTES: usize = 16 * 1024;

pub const SCHEDULE_CALC_VERSION: &str = "utc-v1";
/// Initial server policy and draft budget; deployed policy is read from storage.
pub const TASK_PUBLICATION_BUDGET: desk_agent_protocol::schedule::contract::TaskBudget =
    desk_agent_protocol::schedule::contract::TaskBudget {
        max_runs_per_utc_day: 24,
        max_calls_per_run: 64,
        max_model_tokens_per_run: 100_000,
        max_runtime_seconds: 900,
    };

pub const MIN_INTERVAL_SECONDS: u32 = 60;
pub const MAX_INTERVAL_SECONDS: u32 = 31_536_000;

pub mod contract;
pub mod due;
pub mod fresh_session;
pub mod history;
pub mod lifecycle;
pub mod model_usage;
pub mod permission_wait;
pub mod policy;
pub mod rehearsal;
pub mod source_graph;
pub mod task_prompt;
pub mod timezone;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    InvalidJson,
    UnsupportedVersion,
    InvalidTime,
    InvalidWeekdays,
    InvalidInterval,
    OutOfRange,
    InvalidDraft,
}

pub fn normalize_draft(
    draft: &desk_agent_protocol::schedule::ScheduleDraft,
) -> Result<desk_agent_protocol::schedule::ScheduleDraft, ScheduleError> {
    use desk_agent_protocol::schedule::ScheduledTaskKind;
    let valid_id = |id: &str| {
        !id.is_empty() && id.len() <= 256 && id.trim() == id && !id.chars().any(char::is_control)
    };
    if !valid_id(&draft.client_create_key)
        || !valid_id(&draft.target_device_id)
        || draft.title.trim().is_empty()
        || draft.title.len() > 240
        || draft.prompt.trim().is_empty()
        || draft.prompt.len() > MAX_SCHEDULE_PROMPT_BYTES
        || draft
            .locale
            .as_ref()
            .is_some_and(|s| s.len() > 64 || s.chars().any(char::is_control))
        || draft.model_id.is_some_and(|id| id <= 0)
        || draft
            .source_conversation_id
            .as_deref()
            .is_some_and(|s| !valid_id(s))
    {
        return Err(ScheduleError::InvalidDraft);
    }
    match draft.kind {
        ScheduledTaskKind::ConversationResume
            if draft.source_conversation_id.is_none()
                || draft
                    .requirement_revision
                    .is_none_or(|r| r == 0 || r > i64::MAX as u64)
                || !matches!(
                    draft.spec.rule,
                    ScheduleRule::Once { .. } | ScheduleRule::AfterConfirmation { .. }
                ) =>
        {
            return Err(ScheduleError::InvalidDraft);
        }
        ScheduledTaskKind::FreshTask
            if draft.requirement_revision.is_some()
                || matches!(draft.spec.rule, ScheduleRule::AfterConfirmation { .. }) =>
        {
            return Err(ScheduleError::InvalidDraft);
        }
        _ => {}
    }
    let mut normalized = draft.clone();
    normalized.spec = normalize(&draft.spec)?;
    if let Some(confirmation) = &draft.time_confirmation {
        timezone::verify_confirmation(&normalized.spec, confirmation)
            .map_err(|_| ScheduleError::InvalidTime)?;
    }
    Ok(normalized)
}

fn absolute(value: &str) -> Result<DateTime<Utc>, ScheduleError> {
    let dt = DateTime::parse_from_rfc3339(value).map_err(|_| ScheduleError::InvalidTime)?;
    if dt.timestamp_subsec_nanos() != 0 || value.contains('.') || value.contains(',') {
        return Err(ScheduleError::InvalidTime);
    }
    let utc = dt.with_timezone(&Utc);
    if !(1970..=9999).contains(&utc.year()) {
        return Err(ScheduleError::OutOfRange);
    }
    Ok(utc)
}

fn clock(value: &str) -> Result<NaiveTime, ScheduleError> {
    if value.len() != 8
        || !value.bytes().enumerate().all(|(i, b)| {
            if i == 2 || i == 5 {
                b == b':'
            } else {
                b.is_ascii_digit()
            }
        })
        || &value[6..8] > "59"
    {
        return Err(ScheduleError::InvalidTime);
    }
    NaiveTime::parse_from_str(value, "%H:%M:%S").map_err(|_| ScheduleError::InvalidTime)
}

pub fn normalize(spec: &ScheduleSpec) -> Result<ScheduleSpec, ScheduleError> {
    if spec.schema_version != SCHEDULE_SCHEMA_VERSION {
        return Err(ScheduleError::UnsupportedVersion);
    }
    let mut spec = spec.clone();
    match &mut spec.rule {
        ScheduleRule::AfterConfirmation { delay_seconds } => {
            if !(1..=MAX_INTERVAL_SECONDS).contains(delay_seconds) {
                return Err(ScheduleError::InvalidInterval);
            }
        }
        ScheduleRule::Once { at } => {
            *at = absolute(at)?.to_rfc3339_opts(SecondsFormat::Secs, true);
        }
        ScheduleRule::Interval {
            every_seconds,
            anchor_at,
        } => {
            if !(MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(every_seconds) {
                return Err(ScheduleError::InvalidInterval);
            }
            *anchor_at = absolute(anchor_at)?.to_rfc3339_opts(SecondsFormat::Secs, true);
        }
        ScheduleRule::Daily { utc_time } => {
            clock(utc_time)?;
        }
        ScheduleRule::Weekly { weekdays, utc_time } => {
            clock(utc_time)?;
            if weekdays.is_empty()
                || weekdays.len() > 7
                || weekdays.iter().any(|d| !(1..=7).contains(d))
            {
                return Err(ScheduleError::InvalidWeekdays);
            }
            weekdays.sort_unstable();
            if weekdays.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(ScheduleError::InvalidWeekdays);
            }
        }
    }
    Ok(spec)
}

pub fn parse_json(json: &str) -> Result<ScheduleSpec, ScheduleError> {
    if json.len() > MAX_SCHEDULE_SPEC_BYTES {
        return Err(ScheduleError::InvalidJson);
    }
    normalize(&serde_json::from_str(json).map_err(|_| ScheduleError::InvalidJson)?)
}

/// A bounded preview uses exactly the same iterator as production scheduling.
pub fn preview(
    spec: &ScheduleSpec,
    reference_ms: i64,
    count: usize,
) -> Result<Vec<i64>, ScheduleError> {
    if count > 5 {
        return Err(ScheduleError::OutOfRange);
    }
    normalize(spec)?;
    let mut cursor = reference_ms;
    let mut times = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(next) = next_after(spec, cursor)? else {
            break;
        };
        times.push(next);
        cursor = next;
    }
    Ok(times)
}

/// Publication cannot turn an elapsed one-shot intent into an immediate run.
pub fn validate_publication(
    spec: &ScheduleSpec,
    now_ms: i64,
    conversation_resume: bool,
) -> Result<ScheduleSpec, ScheduleError> {
    let mut normalized = normalize(spec)?;
    if let ScheduleRule::AfterConfirmation { delay_seconds } = normalized.rule {
        if !conversation_resume {
            return Err(ScheduleError::InvalidTime);
        }
        let at = now_ms
            .checked_add(i64::from(delay_seconds) * 1000)
            .and_then(|value| value.checked_add(999))
            .map(|value| value / 1000 * 1000)
            .and_then(DateTime::from_timestamp_millis)
            .ok_or(ScheduleError::OutOfRange)?;
        normalized.rule = ScheduleRule::Once {
            at: at.to_rfc3339_opts(SecondsFormat::Secs, true),
        };
        normalized = normalize(&normalized)?;
    }
    if conversation_resume && !matches!(normalized.rule, ScheduleRule::Once { .. }) {
        return Err(ScheduleError::InvalidTime);
    }
    if next_after(&normalized, now_ms)?.is_none() {
        return Err(ScheduleError::InvalidTime);
    }
    Ok(normalized)
}

/// Returns a whole-second occurrence strictly after the millisecond reference.
pub fn next_after(spec: &ScheduleSpec, reference_ms: i64) -> Result<Option<i64>, ScheduleError> {
    let spec = normalize(spec)?;
    let reference =
        DateTime::from_timestamp_millis(reference_ms).ok_or(ScheduleError::OutOfRange)?;
    if !(1970..=9999).contains(&reference.year()) {
        return Err(ScheduleError::OutOfRange);
    }
    let result = match &spec.rule {
        ScheduleRule::AfterConfirmation { .. } => return Err(ScheduleError::InvalidTime),
        ScheduleRule::Once { at } => {
            let at = absolute(at)?.timestamp_millis();
            (at > reference_ms).then_some(at)
        }
        ScheduleRule::Interval {
            every_seconds,
            anchor_at,
        } => {
            let anchor = absolute(anchor_at)?.timestamp_millis();
            let interval = i64::from(*every_seconds) * 1000;
            let next = if reference_ms < anchor {
                anchor
            } else {
                let n = (reference_ms - anchor) / interval + 1;
                anchor
                    .checked_add(n.checked_mul(interval).ok_or(ScheduleError::OutOfRange)?)
                    .ok_or(ScheduleError::OutOfRange)?
            };
            Some(next)
        }
        ScheduleRule::Daily { utc_time } | ScheduleRule::Weekly { utc_time, .. } => {
            let time = clock(utc_time)?;
            let mut result = None;
            for offset in 0..=7 {
                let day = reference
                    .date_naive()
                    .checked_add_signed(Duration::days(offset))
                    .ok_or(ScheduleError::OutOfRange)?;
                if let ScheduleRule::Weekly { weekdays, .. } = &spec.rule
                    && !weekdays.contains(&(day.weekday().number_from_monday() as u8))
                {
                    continue;
                }
                let at = day.and_time(time).and_utc().timestamp_millis();
                if at > reference_ms {
                    result = Some(at);
                    break;
                }
            }
            result
        }
    };
    if let Some(at) = result
        && DateTime::from_timestamp_millis(at).is_none_or(|t| t.year() > 9999)
    {
        return Err(ScheduleError::OutOfRange);
    }
    Ok(result)
}

#[cfg(test)]
mod tests;

pub mod management_tools;
pub mod proposal;

pub mod directory_recovery;

#[cfg(test)]
mod confirmation_delay_tests {
    use super::*;
    #[test]
    fn delay_is_draft_only_and_resolves_from_each_confirmation_clock() {
        let draft = ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::AfterConfirmation { delay_seconds: 300 },
        };
        assert!(next_after(&draft, 1_000_000).is_err());
        assert!(validate_publication(&draft, 1_000_000, false).is_err());
        for now in [1_000_000, 9_000_123] {
            let active = validate_publication(&draft, now, true).unwrap();
            let at = next_after(&active, now).unwrap().unwrap();
            assert!(at >= now + 300_000 && at < now + 301_000);
            assert!(matches!(active.rule, ScheduleRule::Once { .. }));
        }
        for delay_seconds in [0, MAX_INTERVAL_SECONDS + 1] {
            assert!(
                normalize(&ScheduleSpec {
                    schema_version: 1,
                    rule: ScheduleRule::AfterConfirmation { delay_seconds }
                })
                .is_err()
            );
        }
    }
}
