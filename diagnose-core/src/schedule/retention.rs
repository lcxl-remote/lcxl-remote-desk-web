//! Conversation timers must fire while their source session still exists.
//!
//! A `conversation_resume` task continues one stored conversation. Idle
//! sessions are reclaimed after the deployment's session retention window, so a
//! timer that fires later would find its input and evidence gone. Creation,
//! owner approval and activation therefore reject a continuation whose one-time
//! run falls outside the window minus a safety margin. `fresh_task` schedules
//! start a new context on every run and are not bound by this rule.

use chrono::DateTime;
use desk_agent_protocol::schedule::{ScheduleRule, ScheduleSpec};

/// One expiry sweep interval (both products sweep hourly) plus one hour of
/// slack, so a timer cannot land between a reclaim tick and its own run.
pub const SESSION_RETENTION_SAFETY_MARGIN_MS: i64 = 2 * 60 * 60 * 1_000;

/// The run falls outside the session retention window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionExceeded {
    /// Longest delay from now, in whole seconds, the current window accepts.
    pub max_delay_seconds: u64,
}

/// Longest accepted delay between now and the run, in milliseconds.
pub fn max_resume_delay_ms(retention_ms: i64) -> i64 {
    retention_ms
        .saturating_sub(SESSION_RETENTION_SAFETY_MARGIN_MS)
        .max(0)
}

/// Checks a conversation timer's single run against the retention window.
/// `spec` may be the model's `after_confirmation` proposal or the normalized
/// `once` rule produced at activation. Every other rule is invalid for a
/// conversation timer and is rejected by publication validation, so it is
/// accepted here unchanged.
pub fn conversation_resume_fits_retention(
    spec: &ScheduleSpec,
    now_ms: i64,
    retention_ms: i64,
) -> Result<(), RetentionExceeded> {
    let max_delay_ms = max_resume_delay_ms(retention_ms);
    let exceeded = || RetentionExceeded {
        max_delay_seconds: u64::try_from(max_delay_ms / 1_000).unwrap_or(0),
    };
    let delay_ms = match &spec.rule {
        ScheduleRule::AfterConfirmation { delay_seconds } => i64::from(*delay_seconds) * 1_000,
        ScheduleRule::Once { at } => {
            let at = DateTime::parse_from_rfc3339(at).map_err(|_| exceeded())?;
            at.timestamp_millis().saturating_sub(now_ms)
        }
        _ => return Ok(()),
    };
    if delay_ms > max_delay_ms {
        Err(exceeded())
    } else {
        Ok(())
    }
}

/// Retention window in milliseconds for a window configured in whole days.
pub fn retention_ms_from_days(days: u32) -> i64 {
    i64::from(days) * 24 * 60 * 60 * 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
    const NOW: i64 = 1_800_000_000_000;

    fn once_after(delay_ms: i64) -> ScheduleSpec {
        let at = DateTime::from_timestamp_millis(NOW + delay_ms).unwrap();
        ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::Once {
                at: at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            },
        }
    }

    #[test]
    fn once_run_must_fall_inside_the_window_minus_margin() {
        let retention = retention_ms_from_days(30);
        let limit = 30 * DAY_MS - SESSION_RETENTION_SAFETY_MARGIN_MS;
        assert_eq!(
            conversation_resume_fits_retention(&once_after(limit), NOW, retention),
            Ok(())
        );
        assert_eq!(
            conversation_resume_fits_retention(&once_after(limit + 1_000), NOW, retention),
            Err(RetentionExceeded {
                max_delay_seconds: (limit / 1_000) as u64
            })
        );
    }

    #[test]
    fn after_confirmation_delay_is_checked_before_normalization() {
        let retention = retention_ms_from_days(1);
        let max = max_resume_delay_ms(retention) / 1_000;
        let spec = |delay_seconds: u32| ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::AfterConfirmation { delay_seconds },
        };
        assert!(conversation_resume_fits_retention(&spec(max as u32), NOW, retention).is_ok());
        assert!(conversation_resume_fits_retention(&spec(max as u32 + 1), NOW, retention).is_err());
    }

    #[test]
    fn recurring_rules_are_left_to_fresh_task_validation() {
        let spec = ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::Interval {
                every_seconds: 31_536_000,
                anchor_at: "2100-01-01T00:00:00Z".into(),
            },
        };
        assert_eq!(
            conversation_resume_fits_retention(&spec, NOW, retention_ms_from_days(1)),
            Ok(())
        );
    }

    #[test]
    fn a_window_shorter_than_the_margin_accepts_nothing() {
        assert_eq!(max_resume_delay_ms(60 * 60 * 1_000), 0);
        assert!(
            conversation_resume_fits_retention(&once_after(1_000), NOW, 60 * 60 * 1_000).is_err()
        );
    }
}
