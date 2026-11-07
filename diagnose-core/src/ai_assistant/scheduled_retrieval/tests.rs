use super::*;
use crate::ai_assistant::{history::RetrievalCallKind, test_support::personal_session};
use desk_agent_protocol::ai_assistant::history::{AssistantDeviceScope, HistoryTimeRange};

fn consent() -> TaskRetrievalConsent {
    let mut session = personal_session("owner", "root", "device");
    session.input_revision = 1;
    let data = session.assistant_data.as_mut().unwrap();
    data.memory_access_scope.as_mut().unwrap().device_rule = AssistantDeviceScope::CurrentDevice;
    let history = data.history_access_scope.as_mut().unwrap();
    history.timezone = "Asia/Shanghai".into();
    history.time_range = Some(HistoryTimeRange {
        from_unix_ms: 10,
        until_unix_ms: 90,
    });
    history.expires_at_unix_ms = Some(8000);
    capture(&session).unwrap()
}

fn occurrence(run: &str) -> RetrievalOccurrence<'_> {
    RetrievalOccurrence {
        schedule_id: "schedule",
        task_revision: 2,
        run_id: run,
        kind: ScheduledRetrievalKind::ConversationResume,
        source_conversation_id: Some("root"),
        root_conversation_id: "root",
        actor_id: "owner",
        device_id: "device",
        source_device_ref: "device",
        source_instance_id: None,
        started_at_unix_ms: 2000,
        original_deadline_unix_ms: Some(10000),
    }
}

#[test]
fn frozen_scope_preserves_original_owner_input_fixed_dates_timezone_and_device_rule() {
    let consent = consent();
    let raw = consent.encode().unwrap();
    assert_eq!(TaskRetrievalConsent::decode(&raw).unwrap(), consent);
    let scope = consent
        .for_occurrence(&occurrence("schedule-run-one"), Default::default())
        .unwrap();
    assert_eq!(scope.task_id.as_deref(), Some("schedule-run-one"));
    let history = scope.scope.as_ref().unwrap();
    assert_eq!(history.root_conversation_id, "root");
    assert_eq!(history.authorized_input_seq, 1);
    assert_eq!(history.accepted_at_unix_ms, 100);
    assert_eq!(history.timezone, "Asia/Shanghai");
    assert_eq!(
        history.time_range,
        consent.history_scope.as_ref().unwrap().time_range
    );
    assert_eq!(history.expires_at_unix_ms, Some(8000));
    assert_eq!(history.policy_revision, 1);
    assert_eq!(
        scope.memory_scope.as_ref().unwrap().device_rule,
        AssistantDeviceScope::CurrentDevice
    );
    assert_eq!(
        scope.memory_scope.as_ref().unwrap().expires_at_unix_ms,
        Some(10000)
    );
    assert_eq!(scope.budget.relevance_calls, 0);
}

#[test]
fn actual_occurrence_identity_changes_budget_once_while_permission_retry_loads_the_persisted_original()
 {
    let consent = consent();
    let one = occurrence("schedule-run-one");
    let mut frozen = consent.for_occurrence(&one, Default::default()).unwrap();
    let original_episode = frozen.budget.episode_id.clone();
    for index in 0..4 {
        frozen
            .budget
            .reserve(
                &format!("call-{index}"),
                &"a".repeat(64),
                RetrievalCallKind::Relevance,
                16384,
                2100,
            )
            .unwrap();
    }
    // Native retry code persists this ledger. Reconstruction only demonstrates
    // deterministic identity; it must never replace the existing spent ledger.
    let persisted = serde_json::to_string(&frozen.budget).unwrap();
    let recovered: AssistantRetrievalBudget = serde_json::from_str(&persisted).unwrap();
    assert_eq!(recovered, frozen.budget);
    assert_eq!(recovered.relevance_calls, 4);
    assert_eq!(
        consent
            .for_occurrence(&one, Default::default())
            .unwrap()
            .budget
            .episode_id,
        original_episode
    );
    assert_ne!(
        consent
            .for_occurrence(&occurrence("schedule-run-two"), Default::default())
            .unwrap()
            .budget
            .episode_id,
        original_episode
    );
}

#[test]
fn fresh_task_rebinds_only_to_its_original_new_occurrence_and_keeps_original_evidence_identity() {
    let consent = consent();
    let mut run = occurrence("schedule-run-fresh");
    run.kind = ScheduledRetrievalKind::FreshTask;
    assert!(consent.for_occurrence(&run, Default::default()).is_err());
    run.root_conversation_id = run.run_id;
    let frozen = consent.for_occurrence(&run, Default::default()).unwrap();
    assert_eq!(
        frozen.scope.as_ref().unwrap().root_conversation_id,
        run.run_id
    );
    assert_eq!(
        frozen.memory_scope.as_ref().unwrap().root_conversation_id,
        run.run_id
    );
    assert_eq!(frozen.scope.as_ref().unwrap().accepted_at_unix_ms, 100);
    assert_eq!(frozen.scope.as_ref().unwrap().current_device_ref, "device");
}

#[test]
fn conversation_resume_preserves_original_scope_expiry_without_inventing_a_runtime_deadline() {
    let consent = consent();
    let mut run = occurrence("schedule-run-without-runtime-bound");
    run.original_deadline_unix_ms = None;
    let frozen = consent.for_occurrence(&run, Default::default()).unwrap();
    assert_eq!(
        frozen.scope.as_ref().unwrap().expires_at_unix_ms,
        Some(8000)
    );
    assert_eq!(
        frozen.memory_scope.as_ref().unwrap().expires_at_unix_ms,
        None
    );
    assert_eq!(
        frozen.scope.as_ref().unwrap().task_id.as_deref(),
        Some(run.run_id)
    );
    assert_eq!(
        frozen.budget.episode_id,
        consent
            .for_occurrence(&run, Default::default())
            .unwrap()
            .budget
            .episode_id
    );

    // Fresh tasks do have a mandatory published runtime bound. The host must
    // supply it from that contract rather than fall back to a renewed lease.
    run.kind = ScheduledRetrievalKind::FreshTask;
    run.root_conversation_id = run.run_id;
    assert!(consent.for_occurrence(&run, Default::default()).is_err());
    run.original_deadline_unix_ms = Some(5000);
    let frozen = consent.for_occurrence(&run, Default::default()).unwrap();
    assert_eq!(
        frozen.scope.as_ref().unwrap().expires_at_unix_ms,
        Some(5000)
    );
    assert_eq!(
        frozen.memory_scope.as_ref().unwrap().expires_at_unix_ms,
        Some(5000)
    );
}

#[test]
fn disabled_and_temporary_consent_cannot_be_enlarged_by_later_settings() {
    for temporary in [false, true] {
        let mut session = personal_session("owner", "root", "device");
        session.input_revision = 1;
        let data = session.assistant_data.as_mut().unwrap();
        data.temporary = temporary;
        data.history_access_scope = None;
        data.memory_access_scope = None;
        let consent = capture(&session).unwrap();
        let frozen = consent
            .for_occurrence(&occurrence("schedule-run-disabled"), Default::default())
            .unwrap();
        assert!(frozen.scope.is_none() && frozen.memory_scope.is_none());
        assert_eq!(consent.temporary, temporary);
    }
    let mut session = personal_session("owner", "root", "device");
    session.input_revision = 1;
    session.assistant_data = None;
    let consent = capture(&session).unwrap();
    let frozen = consent
        .for_occurrence(&occurrence("schedule-run-absent"), Default::default())
        .unwrap();
    assert!(frozen.scope.is_none() && frozen.memory_scope.is_none());
}

#[test]
fn definition_subject_device_instance_root_and_native_time_changes_are_rejected() {
    let consent = consent();
    for variant in 0..11 {
        let mut run = occurrence("schedule-run-one");
        match variant {
            0 => run.schedule_id = "other-task",
            1 => run.task_revision += 1,
            2 => run.actor_id = "other-owner",
            3 => run.device_id = "other-device",
            4 => run.source_device_ref = "new-binding-after-reregistration",
            5 => run.source_instance_id = Some("another-instance"),
            6 => run.root_conversation_id = "other-root",
            7 => run.started_at_unix_ms = 999,
            8 => run.original_deadline_unix_ms = Some(2000),
            9 => run.source_conversation_id = Some("another-approved-source"),
            _ => run.run_id = "invented-run",
        }
        assert!(
            consent.for_occurrence(&run, Default::default()).is_err(),
            "variant {variant}"
        );
    }
    assert!(
        consent
            .validate_definition("other-schedule", 2, "owner", "device", Some("root"))
            .is_err()
    );
    assert!(
        consent
            .validate_definition("schedule", 2, "owner", "device", Some("other-source"))
            .is_err()
    );
}

#[test]
fn malformed_oversized_or_mixed_original_scopes_do_not_decode_as_owner_consent() {
    let original = consent();
    assert!(TaskRetrievalConsent::decode(&"x".repeat(32 * 1024 + 1)).is_err());
    for variant in 0..8 {
        let mut changed = original.clone();
        match variant {
            0 => changed.memory_scope.as_mut().unwrap().actor_id = "other-owner".into(),
            1 => changed.history_scope.as_mut().unwrap().root_conversation_id = "other-root".into(),
            2 => {
                changed.memory_scope.as_mut().unwrap().current_device_ref = "another-device".into()
            }
            3 => {
                changed.memory_scope.as_mut().unwrap().retrieval_episode_id = "other-episode".into()
            }
            4 => changed.source_device_ref = None,
            5 => changed.temporary = true,
            6 => changed.source_input_revision = Some(0),
            _ => changed.captured_at_unix_ms = 99,
        }
        assert!(changed.validate().is_err(), "variant {variant}");
    }
}

#[test]
fn native_resume_binding_keeps_prior_context_and_rechecks_without_refilling_spent_calls() {
    let mut session = personal_session("owner", "root", "device");
    session.input_revision = 1;
    let consent = capture(&session).unwrap();
    let prior = session.clone();
    let run = occurrence("schedule-run-bound");
    consent
        .bind_conversation_resume(&mut session, &run, &Default::default())
        .unwrap();
    assert_eq!(session.conversation, prior.conversation);
    let data = session.assistant_data.as_mut().unwrap();
    let original = prior.assistant_data.as_ref().unwrap();
    assert_eq!(data.instructions, original.instructions);
    assert_eq!(data.read_uses, original.read_uses);
    assert_eq!(data.history_dependencies, original.history_dependencies);
    assert_eq!(data.current_memories, original.current_memories);
    assert_eq!(
        data.current_actor_input_seq,
        original.current_actor_input_seq
    );
    data.retrieval_budget
        .reserve(
            "spent",
            &"b".repeat(64),
            RetrievalCallKind::Relevance,
            4096,
            2100,
        )
        .unwrap();
    let spent = session.clone();
    consent.validate_occurrence_binding(&session, &run).unwrap();
    assert!(
        consent
            .bind_conversation_resume(&mut session, &run, &Default::default())
            .is_err()
    );
    assert_eq!(session, spent);
    assert_eq!(
        session
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget
            .relevance_calls,
        1
    );
    let mut other = occurrence("schedule-run-another");
    assert!(
        consent
            .validate_occurrence_binding(&session, &other)
            .is_err()
    );
    other.source_device_ref = "other-device";
    assert!(
        consent
            .bind_conversation_resume(&mut session, &other, &Default::default())
            .is_err()
    );
    assert_eq!(session, spent);
}

#[test]
fn resume_binding_replaces_later_enabled_scopes_with_original_absent_consent() {
    let mut session = personal_session("owner", "root", "device");
    session.input_revision = 1;
    let mut source = session.clone();
    source.assistant_data = None;
    let consent = capture(&source).unwrap();
    let run = occurrence("schedule-run-without-consent");
    consent
        .bind_conversation_resume(&mut session, &run, &Default::default())
        .unwrap();
    let data = session.assistant_data.as_ref().unwrap();
    assert!(data.history_access_scope.is_none() && data.memory_access_scope.is_none());
    assert_eq!(data.retrieval_task_id.as_deref(), Some(run.run_id));
    consent.validate_occurrence_binding(&session, &run).unwrap();
}

fn fresh_root(run: &str) -> PersistedAgentSession {
    let mut session = personal_session("owner", run, "device");
    session.assistant_data = None;
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    session.input_revision = 1;
    session.latest_input_seq = 1;
    session
        .begin_turn(
            format!("{run}-turn"),
            Some(run.into()),
            None,
            1,
            session.scope_snapshot.clone(),
            "2026-10-20T00:00:00Z",
        )
        .unwrap();
    session.adopt_trigger(
        crate::session::TriggerOrigin::ScheduledTask,
        &format!("{run}-turn"),
    );
    session
}

#[test]
fn fresh_context_inherits_original_personalization_and_scopes_without_old_transcript_or_ledger() {
    let mut source = personal_session("owner", "root", "device");
    source.input_revision = 1;
    source.conversation.push(crate::chat::ChatMessage::text(
        "old-user",
        crate::chat::ChatRole::User,
        "DO_NOT_COPY_OLD_CHAT",
    ));
    let data = source.assistant_data.as_mut().unwrap();
    data.current_actor_input_seq = 11;
    data.current_memories
        .push(super::super::data_control::CurrentMemoryUse {
            memory_id: "old-memory".into(),
            revision: 1,
            purpose: desk_agent_protocol::ai_assistant::memory::MemoryUsePurpose::CurrentFact,
        });
    data.retrieval_budget
        .reserve(
            "old-spent",
            &"b".repeat(64),
            RetrievalCallKind::Relevance,
            4096,
            100,
        )
        .unwrap();
    let consent = capture(&source).unwrap();
    let mut run = occurrence("schedule-run-fresh-bind");
    run.kind = ScheduledRetrievalKind::FreshTask;
    run.root_conversation_id = run.run_id;
    let mut fresh = fresh_root(run.run_id);
    let prior = fresh.clone();
    consent
        .bind_fresh_task(&mut fresh, &run, &Default::default())
        .unwrap();
    assert_eq!(fresh.conversation, prior.conversation);
    assert_eq!(fresh.input_revision, 1);
    let data = fresh.assistant_data.as_ref().unwrap();
    assert_eq!(data.current_actor_input_seq, 11);
    assert_eq!(
        data.instructions,
        source.assistant_data.as_ref().unwrap().instructions
    );
    assert!(
        data.current_memories.is_empty()
            && data.read_uses.is_empty()
            && data.history_dependencies.is_empty()
    );
    assert_eq!(data.retrieval_budget.relevance_calls, 0);
    assert_eq!(data.retrieval_task_id.as_deref(), Some(run.run_id));
    assert_eq!(
        data.memory_access_scope
            .as_ref()
            .unwrap()
            .expires_at_unix_ms,
        Some(10000)
    );
    consent.validate_occurrence_binding(&fresh, &run).unwrap();
    assert!(
        consent
            .bind_fresh_task(&mut fresh, &run, &Default::default())
            .is_err()
    );
    assert!(!consent.encode().unwrap().contains("DO_NOT_COPY_OLD_CHAT"));
}

#[test]
fn standalone_definition_freezes_personalization_without_inventing_owner_input_or_search_scope() {
    let instructions = desk_agent_protocol::ai_assistant::user_instructions::UserInstructions {
        revision: 9,
        content: "\\".repeat(8192),
    };
    let consent = TaskRetrievalConsent::capture_definition(
        "schedule",
        2,
        "owner",
        "device",
        &instructions,
        "Asia/Shanghai",
        1000,
    )
    .unwrap();
    assert!(consent.source_conversation_id.is_none() && consent.source_input_revision.is_none());
    assert_eq!(
        TaskRetrievalConsent::decode(&consent.encode().unwrap()).unwrap(),
        consent
    );
    let mut run = occurrence("schedule-run-standalone");
    run.kind = ScheduledRetrievalKind::FreshTask;
    run.root_conversation_id = run.run_id;
    run.source_conversation_id = None;
    let mut fresh = fresh_root(run.run_id);
    consent
        .bind_fresh_task(&mut fresh, &run, &Default::default())
        .unwrap();
    let data = fresh.assistant_data.as_ref().unwrap();
    assert_eq!(data.current_actor_input_seq, 0);
    assert_eq!(data.instructions.accepted_input_seq, 0);
    assert_eq!(data.instructions.content, instructions.content);
    assert!(data.history_access_scope.is_none() && data.memory_access_scope.is_none());
    assert_eq!(data.current_input_timezone, "Asia/Shanghai");
    consent.validate_occurrence_binding(&fresh, &run).unwrap();
    PersistedAgentSession::decode_json(&fresh.encode_json_for_storage().unwrap()).unwrap();
    let mut wrong = fresh.clone();
    wrong.conversation_id = "ordinary-chat".into();
    assert!(PersistedAgentSession::decode_json(&wrong.encode_json_for_storage().unwrap()).is_err());
}

#[test]
fn configured_personalization_capacity_survives_task_definition_snapshot_encoding() {
    let mut limits = desk_agent_protocol::ai_assistant::limits::AssistantDataLimits::default();
    limits.memory.user_instructions_bytes = 128 * 1024;
    let instructions = desk_agent_protocol::ai_assistant::user_instructions::UserInstructions {
        revision: 1,
        content: "偏好".repeat(10000),
    };
    desk_agent_protocol::ai_assistant::user_instructions::UserInstructionsUpdate {
        expected_revision: 1,
        content: instructions.content.clone(),
    }
    .validate_with_limits(&limits.memory)
    .unwrap();
    let consent = TaskRetrievalConsent::capture_definition(
        "schedule",
        1,
        "owner",
        "device",
        &instructions,
        "UTC",
        1000,
    )
    .unwrap();
    let encoded = consent.encode().unwrap();
    assert!(encoded.len() > 32 * 1024);
    assert_eq!(TaskRetrievalConsent::decode(&encoded).unwrap(), consent);
}

#[test]
fn a_new_occurrence_uses_current_platform_limits_and_resume_preserves_its_allowance() {
    let mut source = personal_session("owner", "root", "device");
    source.input_revision = 1;
    let consent = capture(&source).unwrap();
    let mut run = occurrence("schedule-run-configured");
    run.kind = ScheduledRetrievalKind::ConversationResume;
    run.source_conversation_id = Some(&source.conversation_id);
    run.root_conversation_id = &source.conversation_id;
    let mut limits = desk_agent_protocol::ai_assistant::limits::AssistantDataLimits::default();
    limits.retrieval.episode.relevance_calls = 7;
    limits.retrieval.query_chars = 512;
    let mut resumed = source.clone();
    consent
        .bind_conversation_resume(&mut resumed, &run, &limits)
        .unwrap();
    let data = resumed.assistant_data.as_mut().unwrap();
    data.retrieval_budget
        .reserve(
            "first",
            &"a".repeat(64),
            crate::ai_assistant::history::RetrievalCallKind::Relevance,
            100,
            2000,
        )
        .unwrap();
    limits.retrieval.episode.relevance_calls = 1;
    consent.validate_occurrence_binding(&resumed, &run).unwrap();
    assert_eq!(
        resumed
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget
            .limits
            .relevance_calls,
        7
    );
    assert_eq!(
        resumed
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget
            .relevance_calls,
        1
    );
    assert!(
        consent
            .bind_conversation_resume(&mut resumed, &run, &limits)
            .is_err()
    );
}

fn capture(source: &PersistedAgentSession) -> Result<TaskRetrievalConsent, &'static str> {
    let mut source = source.clone();
    source.bind_assistant_input_targets(&[])?;
    let target = source.single_assistant_request_target()?.clone();
    TaskRetrievalConsent::capture("schedule", 2, &source, target, None, 1000)
}

#[test]
fn selectable_timer_keeps_its_target_and_context_after_later_owner_selection() {
    let mut source = personal_session("owner", "root", "");
    source.input_revision = 1;
    source.begin_focus_epoch(1, Vec::new()).unwrap();
    source
        .bind_assistant_request_targets(vec!["a".into(), "b".into()])
        .unwrap();
    let target = source.assistant_request_device("a").unwrap().clone();
    let original = crate::input_read_context::ReadContextSelection::default();
    let consent =
        TaskRetrievalConsent::capture("schedule", 2, &source, target, Some(original.clone()), 1000)
            .unwrap();
    assert!(consent.source_session_device_id.is_empty());
    assert_eq!(consent.device_id, "a");
    let frozen = TaskRetrievalConsent::decode(&consent.encode().unwrap()).unwrap();
    source.input_revision = 2;
    source.begin_focus_epoch(2, Vec::new()).unwrap();
    source
        .bind_assistant_request_targets(vec!["b".into()])
        .unwrap();
    source.current_request_id = Some("schedule-run-one".into());
    source.adopt_trigger(
        crate::session::TriggerOrigin::ScheduledContinuation,
        "timer-turn",
    );
    source.scheduled_request = Some(Box::new(frozen.request_binding("schedule-run-one")));
    source
        .scheduled_request
        .as_ref()
        .unwrap()
        .validate(&source)
        .unwrap();
    assert_eq!(
        source.single_assistant_request_target().unwrap().device_id,
        "a"
    );
    assert_eq!(
        source.admitted_assistant_request_targets().unwrap()[0].device_id,
        "b"
    );
    assert_eq!(
        source
            .scheduled_request
            .as_ref()
            .unwrap()
            .original_read_context
            .as_ref(),
        Some(&original)
    );
    let restored =
        PersistedAgentSession::decode_json(&source.encode_json_for_storage().unwrap()).unwrap();
    assert_eq!(
        restored
            .single_assistant_request_target()
            .unwrap()
            .device_id,
        "a"
    );
    source.input_revision = 3;
    source.begin_focus_epoch(3, Vec::new()).unwrap();
    source
        .bind_assistant_request_targets(vec!["b".into()])
        .unwrap();
    assert!(source.scheduled_request.is_none());
    assert_eq!(
        source.single_assistant_request_target().unwrap().device_id,
        "b"
    );
}

#[test]
fn native_timer_restores_owner_scopes_and_spent_budget_without_copying_content() {
    let mut source = personal_session("owner", "root", "device");
    source.input_revision = 1;
    source.bind_assistant_input_targets(&[]).unwrap();
    let consent = capture(&source).unwrap();
    let data = source.assistant_data.as_mut().unwrap();
    // A later owner choice remains the source for any subsequently created task.
    data.memory_access_scope = None;
    data.history_access_scope = None;
    data.retrieval_budget
        .reserve(
            "owner-spent",
            &"c".repeat(64),
            RetrievalCallKind::Relevance,
            100,
            2000,
        )
        .unwrap();
    let owner = data.clone();
    let run = occurrence("schedule-run-owner-restore");
    consent
        .bind_conversation_resume(&mut source, &run, &Default::default())
        .unwrap();
    source.scheduled_request = Some(Box::new(consent.request_binding(run.run_id)));
    assert!(
        source
            .assistant_data
            .as_ref()
            .unwrap()
            .memory_access_scope
            .is_some()
    );
    let next = capture(&source).unwrap();
    assert!(next.memory_scope.is_none());
    assert!(next.history_scope.is_none());
    assert!(restore_owner_retrieval(&mut source).unwrap());
    let restored = source.assistant_data.as_ref().unwrap();
    assert_eq!(restored.retrieval_budget, owner.retrieval_budget);
    assert_eq!(restored.memory_access_scope, owner.memory_access_scope);
    assert_eq!(restored.history_access_scope, owner.history_access_scope);
    assert_eq!(restored.current_memories, owner.current_memories);
    assert!(!restore_owner_retrieval(&mut source).unwrap());
}
