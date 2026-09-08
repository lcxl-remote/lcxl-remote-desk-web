//! IANA local-time editing resolves once to a fixed UTC rule.

use super::*;
use chrono::{LocalResult, NaiveDate, Offset, TimeZone, Timelike};
use chrono_tz::Tz;
use desk_agent_protocol::schedule::{
    LocalScheduleRule, ScheduleTimeConversion, ScheduleTimeConverted, ScheduleTimeFold,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionError {
    InvalidTime,
    InvalidZone,
    NonexistentTime,
    AmbiguousTime {
        earlier_utc: String,
        later_utc: String,
    },
    InvalidRule(ScheduleError),
}

pub fn convert(input: &ScheduleTimeConversion) -> Result<ScheduleTimeConverted, ConversionError> {
    if input.timezone != "UTC" && !input.timezone.contains('/') {
        return Err(ConversionError::InvalidZone);
    }
    let zone: Tz = input
        .timezone
        .parse()
        .map_err(|_| ConversionError::InvalidZone)?;
    if input.reference_date.len() != 10 {
        return Err(ConversionError::InvalidTime);
    }
    let date = NaiveDate::parse_from_str(&input.reference_date, "%Y-%m-%d")
        .map_err(|_| ConversionError::InvalidTime)?;
    if date.format("%Y-%m-%d").to_string() != input.reference_date
        || !(1970..=9999).contains(&date.year())
    {
        return Err(ConversionError::InvalidTime);
    }
    let time = clock(&input.local_time).map_err(ConversionError::InvalidRule)?;
    let local = date.and_time(time);
    let resolved = match zone.from_local_datetime(&local) {
        LocalResult::None => return Err(ConversionError::NonexistentTime),
        LocalResult::Single(at) => at,
        LocalResult::Ambiguous(earlier, later) => match input.fold {
            Some(ScheduleTimeFold::Earlier) => earlier,
            Some(ScheduleTimeFold::Later) => later,
            None => {
                return Err(ConversionError::AmbiguousTime {
                    earlier_utc: earlier
                        .with_timezone(&Utc)
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                    later_utc: later
                        .with_timezone(&Utc)
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                });
            }
        },
    };
    let offset_seconds = resolved.offset().fix().local_minus_utc();
    let utc = resolved.with_timezone(&Utc);
    let utc_time = utc.format("%H:%M:%S").to_string();
    let rule = match &input.rule {
        LocalScheduleRule::Once => ScheduleRule::Once {
            at: utc.to_rfc3339_opts(SecondsFormat::Secs, true),
        },
        LocalScheduleRule::Interval { every_seconds } => ScheduleRule::Interval {
            every_seconds: *every_seconds,
            anchor_at: utc.to_rfc3339_opts(SecondsFormat::Secs, true),
        },
        LocalScheduleRule::Daily => ScheduleRule::Daily { utc_time },
        LocalScheduleRule::Weekly { weekdays } => {
            // Validate before shifting so invalid or duplicate source days cannot disappear.
            normalize(&ScheduleSpec {
                schema_version: 1,
                rule: ScheduleRule::Weekly {
                    weekdays: weekdays.clone(),
                    utc_time: utc_time.clone(),
                },
            })
            .map_err(ConversionError::InvalidRule)?;
            let day_shift = (i64::from(time.num_seconds_from_midnight())
                - i64::from(offset_seconds))
            .div_euclid(86_400);
            ScheduleRule::Weekly {
                weekdays: weekdays
                    .iter()
                    .map(|d| ((i64::from(*d) - 1 + day_shift).rem_euclid(7) + 1) as u8)
                    .collect(),
                utc_time,
            }
        }
    };
    let spec = normalize(&ScheduleSpec {
        schema_version: SCHEDULE_SCHEMA_VERSION,
        rule,
    })
    .map_err(ConversionError::InvalidRule)?;
    Ok(ScheduleTimeConverted {
        spec,
        offset_seconds,
        conversion_version: format!("utc-edit-v1/{}", chrono_tz::IANA_TZDB_VERSION),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(zone: &str, date: &str, time: &str) -> ScheduleTimeConversion {
        ScheduleTimeConversion {
            timezone: zone.into(),
            reference_date: date.into(),
            local_time: time.into(),
            fold: None,
            rule: LocalScheduleRule::Daily,
        }
    }

    #[test]
    fn conversion_fixes_utc_and_rotates_weekdays_across_midnight() {
        let mut edit = input("Asia/Shanghai", "2026-09-07", "01:00:00");
        edit.rule = LocalScheduleRule::Weekly {
            weekdays: vec![1, 3, 5],
        };
        let converted = convert(&edit).unwrap();
        assert_eq!(converted.offset_seconds, 28800);
        assert_eq!(
            converted.spec.rule,
            ScheduleRule::Weekly {
                weekdays: vec![2, 4, 7],
                utc_time: "17:00:00".into()
            }
        );
        assert!(
            !serde_json::to_string(&converted.spec)
                .unwrap()
                .contains("timezone")
        );
    }

    #[test]
    fn ambiguous_input_requires_explicit_selection_and_gap_is_rejected() {
        let mut edit = input("America/Los_Angeles", "2026-11-01", "01:30:00");
        assert!(matches!(
            convert(&edit),
            Err(ConversionError::AmbiguousTime { .. })
        ));
        edit.fold = Some(ScheduleTimeFold::Earlier);
        assert_eq!(convert(&edit).unwrap().offset_seconds, -25200);
        edit.fold = Some(ScheduleTimeFold::Later);
        assert_eq!(convert(&edit).unwrap().offset_seconds, -28800);
        assert_eq!(
            convert(&input("America/Los_Angeles", "2026-03-08", "02:30:00")),
            Err(ConversionError::NonexistentTime)
        );
    }

    #[test]
    fn quarter_hour_offsets_and_invalid_source_rules_are_handled() {
        let mut edit = input("Asia/Kathmandu", "2026-09-07", "14:00:00");
        assert_eq!(
            convert(&edit).unwrap().spec.rule,
            ScheduleRule::Daily {
                utc_time: "08:15:00".into()
            }
        );
        edit.rule = LocalScheduleRule::Weekly { weekdays: vec![0] };
        assert!(convert(&edit).is_err());
        assert_eq!(
            convert(&input("CST", "2026-09-07", "14:00:00")),
            Err(ConversionError::InvalidZone)
        );
    }
}

/// Recompute with the current tzdb; a stale confirmation cannot save a different rule.
pub fn verify_confirmation(
    spec: &ScheduleSpec,
    confirmation: &desk_agent_protocol::schedule::ScheduleTimeConfirmation,
) -> Result<(), ConversionError> {
    let current = convert(&confirmation.input)?;
    if current.conversion_version != confirmation.conversion_version
        || current.spec != normalize(spec).map_err(ConversionError::InvalidRule)?
    {
        return Err(ConversionError::InvalidTime);
    }
    Ok(())
}

pub fn upcoming_runs(spec: &ScheduleSpec, reference_ms: i64) -> Result<Vec<String>, ScheduleError> {
    preview(spec, reference_ms, 5)?
        .into_iter()
        .map(|at| {
            DateTime::<Utc>::from_timestamp_millis(at)
                .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
                .ok_or(ScheduleError::OutOfRange)
        })
        .collect()
}

#[cfg(test)]
mod confirmation_tests {
    use super::*;
    use desk_agent_protocol::schedule::ScheduleTimeConfirmation;

    #[test]
    fn save_rejects_changed_conversion_version_input_or_utc_spec() {
        let input = ScheduleTimeConversion {
            timezone: "Asia/Shanghai".into(),
            reference_date: "2026-09-07".into(),
            local_time: "14:00:00".into(),
            fold: None,
            rule: LocalScheduleRule::Daily,
        };
        let converted = convert(&input).unwrap();
        let confirmation = ScheduleTimeConfirmation {
            input,
            conversion_version: converted.conversion_version.clone(),
        };
        verify_confirmation(&converted.spec, &confirmation).unwrap();
        let mut changed = confirmation.clone();
        changed.conversion_version.push_str("-stale");
        assert!(verify_confirmation(&converted.spec, &changed).is_err());
        changed = confirmation.clone();
        changed.input.local_time = "15:00:00".into();
        assert!(verify_confirmation(&converted.spec, &changed).is_err());
        let mut spec = converted.spec;
        spec.rule = ScheduleRule::Daily {
            utc_time: "07:00:00".into(),
        };
        assert!(verify_confirmation(&spec, &confirmation).is_err());
    }

    #[test]
    fn interval_conversion_and_preview_share_the_persisted_utc_iterator() {
        let input = ScheduleTimeConversion {
            timezone: "Asia/Kathmandu".into(),
            reference_date: "2026-09-07".into(),
            local_time: "14:00:00".into(),
            fold: None,
            rule: LocalScheduleRule::Interval {
                every_seconds: 3600,
            },
        };
        let converted = convert(&input).unwrap();
        assert_eq!(
            converted.spec.rule,
            ScheduleRule::Interval {
                anchor_at: "2026-09-07T08:15:00Z".into(),
                every_seconds: 3600
            }
        );
        let reference = DateTime::parse_from_rfc3339("2026-09-07T08:14:59Z")
            .unwrap()
            .timestamp_millis();
        let values = upcoming_runs(&converted.spec, reference).unwrap();
        assert_eq!(values.len(), 5);
        assert_eq!(values[0], "2026-09-07T08:15:00Z");
        assert_eq!(values[4], "2026-09-07T12:15:00Z");
    }
}
