//! Constant-bounded catch-up calculation over UTC recurrence slots.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueWindow {
    /// The newest due slot; only this occurrence is eligible for catch-up.
    pub latest_at: Option<i64>,
    pub due_count: u64,
    pub next_at: Option<i64>,
}

/// Counts theoretical slots in `(cursor_ms, now_ms]` without replaying a backlog.
pub fn due_window(
    spec: &ScheduleSpec,
    cursor_ms: i64,
    now_ms: i64,
) -> Result<DueWindow, ScheduleError> {
    let normalized = normalize(spec)?;
    let future = next_after(&normalized, now_ms)?;
    let first = next_after(&normalized, cursor_ms)?;
    if now_ms <= cursor_ms || first.is_none_or(|at| at > now_ms) {
        return Ok(DueWindow {
            latest_at: None,
            due_count: 0,
            next_at: next_after(&normalized, cursor_ms.max(now_ms))?,
        });
    }
    let first = first.unwrap();
    let (latest, count) = match &normalized.rule {
        ScheduleRule::AfterConfirmation { .. } => return Err(ScheduleError::InvalidTime),
        ScheduleRule::Once { .. } => (first, 1),
        ScheduleRule::Interval { every_seconds, .. } => {
            let step = i64::from(*every_seconds) * 1000;
            let additional = (now_ms - first) / step;
            (first + additional * step, additional as u64 + 1)
        }
        ScheduleRule::Daily { .. } => {
            let additional = (now_ms - first) / 86_400_000;
            (first + additional * 86_400_000, additional as u64 + 1)
        }
        ScheduleRule::Weekly { weekdays, .. } => {
            let day_count = (now_ms - first) / 86_400_000 + 1;
            let first_day =
                DateTime::from_timestamp_millis(first).ok_or(ScheduleError::OutOfRange)?;
            let first_weekday = first_day.weekday().number_from_monday() as i64;
            let mut count = 0;
            let mut latest = first;
            for weekday in weekdays {
                let offset = (i64::from(*weekday) - first_weekday).rem_euclid(7);
                if offset >= day_count {
                    continue;
                }
                let matches = (day_count - 1 - offset) / 7 + 1;
                count += matches as u64;
                latest = latest.max(first + (offset + (matches - 1) * 7) * 86_400_000);
            }
            (latest, count)
        }
    };
    Ok(DueWindow {
        latest_at: Some(latest),
        due_count: count,
        next_at: future,
    })
}

/// The exclusive start deadline never extends when a scanner retries a slot.
pub fn start_deadline(scheduled_at: i64, grace_seconds: u32) -> Result<i64, ScheduleError> {
    if grace_seconds == 0 {
        return Err(ScheduleError::OutOfRange);
    }
    scheduled_at
        .checked_add(i64::from(grace_seconds) * 1000)
        .ok_or(ScheduleError::OutOfRange)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ms(value: &str) -> i64 {
        absolute(value).unwrap().timestamp_millis()
    }

    #[test]
    fn bounded_count_matches_occurrence_iterator_for_all_weekday_sets() {
        let from = ms("2024-02-27T23:59:59Z");
        let until = ms("2026-03-01T05:59:59Z");
        for mask in 1..128 {
            let spec = ScheduleSpec {
                schema_version: 1,
                rule: ScheduleRule::Weekly {
                    weekdays: (1..=7).filter(|d| mask & (1 << (d - 1)) != 0).collect(),
                    utc_time: "06:00:00".into(),
                },
            };
            let window = due_window(&spec, from, until).unwrap();
            let mut cursor = from;
            let mut slots = Vec::new();
            while let Some(next) = next_after(&spec, cursor).unwrap() {
                if next > until {
                    break;
                }
                slots.push(next);
                cursor = next;
            }
            assert_eq!(window.due_count, slots.len() as u64);
            assert_eq!(window.latest_at, slots.last().copied());
            assert!(window.next_at.unwrap() > until);
        }
    }

    #[test]
    fn centuries_of_interval_backlog_are_counted_without_iteration() {
        let spec = ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::Interval {
                every_seconds: 60,
                anchor_at: "1970-01-01T00:00:00Z".into(),
            },
        };
        let until = ms("9998-01-01T00:00:00Z");
        let window = due_window(&spec, 0, until).unwrap();
        assert_eq!(window.due_count, (until / 60_000) as u64);
        assert_eq!(window.latest_at, Some(until));
        assert_eq!(window.next_at, Some(until + 60_000));
    }

    #[test]
    fn rollback_and_exact_boundaries_do_not_repeat_slots() {
        let at = ms("2026-09-07T06:00:00Z");
        let spec = ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::Once {
                at: "2026-09-07T06:00:00Z".into(),
            },
        };
        assert_eq!(due_window(&spec, at - 1, at).unwrap().due_count, 1);
        assert_eq!(due_window(&spec, at, at).unwrap().due_count, 0);
        assert_eq!(due_window(&spec, at, at - 1).unwrap().due_count, 0);
        assert_eq!(start_deadline(at, 60), Ok(at + 60_000));
        assert!(start_deadline(i64::MAX, 1).is_err());
    }
}
