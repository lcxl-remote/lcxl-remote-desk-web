use super::*;

#[test]
fn publication_and_preview_share_strict_future_semantics() {
    let rule = spec(ScheduleRule::Once {
        at: "2026-09-07T06:00:00Z".into(),
    });
    let at = ms("2026-09-07T06:00:00Z");
    assert!(validate_publication(&rule, at - 1, true).is_ok());
    assert!(validate_publication(&rule, at, true).is_err());
    assert_eq!(preview(&rule, at - 1, 5), Ok(vec![at]));
    assert!(preview(&rule, at - 1, 6).is_err());
    let daily = spec(ScheduleRule::Daily {
        utc_time: "06:00:00".into(),
    });
    assert!(validate_publication(&daily, at, true).is_err());
    assert!(validate_publication(&daily, at, false).is_ok());
    assert_eq!(
        preview(&daily, at - 1, 3),
        Ok(vec![at, at + 86_400_000, at + 172_800_000])
    );
}

fn ms(value: &str) -> i64 {
    absolute(value).unwrap().timestamp_millis()
}
fn spec(rule: ScheduleRule) -> ScheduleSpec {
    ScheduleSpec {
        schema_version: 1,
        rule,
    }
}

#[test]
fn utc_daily_is_strictly_future_at_and_inside_the_due_second() {
    let rule = spec(ScheduleRule::Daily {
        utc_time: "06:00:00".into(),
    });
    let due = ms("2026-09-07T06:00:00Z");
    assert_eq!(next_after(&rule, due - 1), Ok(Some(due)));
    assert_eq!(next_after(&rule, due), Ok(Some(due + 86_400_000)));
    assert_eq!(next_after(&rule, due + 999), Ok(Some(due + 86_400_000)));
}

#[test]
fn weekly_uses_utc_weekdays_and_crosses_year_boundary() {
    let rule = spec(ScheduleRule::Weekly {
        weekdays: vec![1],
        utc_time: "01:00:00".into(),
    });
    assert_eq!(
        next_after(&rule, ms("2026-12-31T23:59:59Z")),
        Ok(Some(ms("2027-01-04T01:00:00Z")))
    );
}

#[test]
fn interval_is_anchored_and_never_based_on_completion() {
    let anchor = ms("2026-09-07T00:00:00Z");
    let rule = spec(ScheduleRule::Interval {
        every_seconds: 7200,
        anchor_at: "2026-09-07T00:00:00Z".into(),
    });
    assert_eq!(next_after(&rule, anchor - 1), Ok(Some(anchor)));
    assert_eq!(next_after(&rule, anchor), Ok(Some(anchor + 7_200_000)));
    assert_eq!(
        next_after(&rule, anchor + 7_200_001),
        Ok(Some(anchor + 14_400_000))
    );
}

#[test]
fn once_normalizes_offset_and_exhausts_without_repeating() {
    let rule = normalize(&spec(ScheduleRule::Once {
        at: "2026-09-07T14:00:00+08:00".into(),
    }))
    .unwrap();
    assert_eq!(
        rule.rule,
        ScheduleRule::Once {
            at: "2026-09-07T06:00:00Z".into()
        }
    );
    assert_eq!(next_after(&rule, ms("2026-09-07T06:00:00Z")), Ok(None));
}

#[test]
fn invalid_or_ambiguous_wire_rules_are_rejected() {
    for raw in [
        r#"{"schema_version":1,"rule":{"kind":"daily","local_time":"14:00:00","timezone":"Asia/Shanghai"}}"#,
        r#"{"schema_version":1,"rule":{"kind":"daily","utc_time":"14:00:00","timezone":"UTC"}}"#,
        r#"{"schema_version":1,"rule":{"kind":"cron","expression":"* * * * *"}}"#,
        r#"{"schema_version":1,"schema_version":1,"rule":{"kind":"daily","utc_time":"14:00:00"}}"#,
    ] {
        assert!(parse_json(raw).is_err(), "{raw}");
    }
    assert_eq!(
        normalize(&ScheduleSpec {
            schema_version: 2,
            rule: ScheduleRule::Daily {
                utc_time: "00:00:00".into()
            }
        }),
        Err(ScheduleError::UnsupportedVersion)
    );
    for time in ["1:00:00", "24:00:00", "23:59:60", "06:00:00Z", "é:00:00"] {
        assert!(
            normalize(&spec(ScheduleRule::Daily {
                utc_time: time.into()
            }))
            .is_err()
        );
    }
    for at in [
        "2026-09-07T14:00:00",
        "2026-09-07T14:00:00.000Z",
        "2016-12-31T23:59:60Z",
    ] {
        assert!(normalize(&spec(ScheduleRule::Once { at: at.into() })).is_err());
    }
}

#[test]
fn limits_sorting_and_overflow_fail_closed() {
    for weekdays in [vec![], vec![0], vec![8], vec![1, 1]] {
        assert_eq!(
            normalize(&spec(ScheduleRule::Weekly {
                weekdays,
                utc_time: "00:00:00".into()
            })),
            Err(ScheduleError::InvalidWeekdays)
        );
    }
    let rule = spec(ScheduleRule::Weekly {
        weekdays: vec![5, 1, 3],
        utc_time: "00:00:00".into(),
    });
    assert_eq!(
        normalize(&rule).unwrap().rule,
        ScheduleRule::Weekly {
            weekdays: vec![1, 3, 5],
            utc_time: "00:00:00".into()
        }
    );
    let rule = spec(ScheduleRule::Daily {
        utc_time: "00:00:00".into(),
    });
    assert_eq!(
        next_after(&rule, ms("9999-12-31T23:59:59Z")),
        Err(ScheduleError::OutOfRange)
    );
    assert_eq!(next_after(&rule, i64::MAX), Err(ScheduleError::OutOfRange));
    assert!(parse_json(&" ".repeat(MAX_SCHEDULE_SPEC_BYTES + 1)).is_err());
}

#[test]
fn task_requirement_limit_matches_conversation_input_bytes() {
    use desk_agent_protocol::schedule::*;
    let mut draft = ScheduleDraft {
        time_confirmation: None,
        client_create_key: "key".into(),
        kind: ScheduledTaskKind::FreshTask,
        target_device_id: "device".into(),
        title: "Task".into(),
        prompt: "x".repeat(MAX_SCHEDULE_PROMPT_BYTES),
        locale: None,
        model_id: None,
        spec: spec(ScheduleRule::Daily {
            utc_time: "06:00:00".into(),
        }),
        source_conversation_id: None,
        requirement_revision: None,
        creation_source: ScheduleCreationSource::Manual,
    };
    assert!(normalize_draft(&draft).is_ok());
    draft.prompt.push('x');
    assert_eq!(normalize_draft(&draft), Err(ScheduleError::InvalidDraft));
    draft.prompt = "中".repeat(MAX_SCHEDULE_PROMPT_BYTES / 3);
    assert!(normalize_draft(&draft).is_ok());
    draft.prompt.push('中');
    assert_eq!(normalize_draft(&draft), Err(ScheduleError::InvalidDraft));
}
