use super::*;
use crate::ai_assistant::{
    data_access::{bind_opened_task, preserve_active_task, task_retrieval_snapshot},
    data_tools::{self, MANAGE_MEMORIES, SEARCH_MEMORIES},
    test_support::personal_session,
};

#[test]
#[allow(clippy::field_reassign_with_default)]
fn memory_consent_is_independent_of_history_settings_and_temporary_conversations() {
    let mut settings = AssistantDataSettings::default();
    settings.model_history_search_enabled = false;
    assert!(
        owner_memory_scope(
            "actor", "root", "device", 1, 100, "episode", 1, &settings, false
        )
        .unwrap()
        .is_some()
    );
    settings.automatic_memory_enabled = false;
    settings.model_history_search_enabled = true;
    assert!(
        owner_memory_scope(
            "actor", "root", "device", 1, 100, "episode", 1, &settings, false
        )
        .unwrap()
        .is_none()
    );
    settings.automatic_memory_enabled = true;
    assert!(
        owner_memory_scope(
            "actor", "root", "device", 1, 100, "episode", 1, &settings, true
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn enabling_memory_later_never_grants_a_preexisting_task_a_new_original_scope() {
    let mut session = personal_session("actor", "root", "device");
    session.assistant_data.as_mut().unwrap().memory_access_scope = None;
    bind_opened_task(&mut session, "goal", 10_000).unwrap();
    session.trigger_origin = TriggerOrigin::ScheduledContinuation;
    let enabled = AssistantDataSettings::default();
    let tools = data_tools::for_session(&session, &enabled, true, true, true, 1000);
    assert!(
        tools
            .iter()
            .any(|tool| tool.name() == data_tools::SEARCH_CONVERSATIONS)
    );
    assert!(!tools.iter().any(|tool| matches!(
        tool.name(),
        SEARCH_MEMORIES
            | MANAGE_MEMORIES
            | data_tools::READ_MEMORY_SOURCE
            | data_tools::READ_MEMORY_HISTORY
    )));
    assert!(usable_memory_scope(&session, 1000).is_none());
    let previous = task_retrieval_snapshot(&session);
    let data = session.assistant_data.as_mut().unwrap();
    data.current_actor_input_seq = 2;
    data.current_input_recorded_at_unix_ms = 200;
    data.memory_access_scope = owner_memory_scope(
        "actor",
        "root",
        "device",
        2,
        200,
        &data.retrieval_budget.episode_id,
        1,
        &enabled,
        false,
    )
    .unwrap();
    preserve_active_task(&mut session, previous, "goal", 9000).unwrap();
    assert!(
        session
            .assistant_data
            .as_ref()
            .unwrap()
            .memory_access_scope
            .is_none()
    );
    assert!(usable_memory_scope(&session, 1000).is_none());
}

#[test]
fn corrections_preserve_original_memory_consent_scope_and_shorter_task_deadline() {
    let mut session = personal_session("actor", "root", "device");
    session
        .assistant_data
        .as_mut()
        .unwrap()
        .memory_access_scope
        .as_mut()
        .unwrap()
        .device_rule = AssistantDeviceScope::CurrentDevice;
    bind_opened_task(&mut session, "goal", 10_000).unwrap();
    let previous = task_retrieval_snapshot(&session);
    let original = session
        .assistant_data
        .as_ref()
        .unwrap()
        .memory_access_scope
        .clone();
    let data = session.assistant_data.as_mut().unwrap();
    data.current_actor_input_seq = 10;
    data.current_input_recorded_at_unix_ms = 500;
    data.memory_access_scope = owner_memory_scope(
        "actor",
        "root",
        "device",
        10,
        500,
        "correction",
        9,
        &AssistantDataSettings::default(),
        false,
    )
    .unwrap();
    preserve_active_task(&mut session, previous, "goal", 20_000).unwrap();
    assert_eq!(
        session.assistant_data.as_ref().unwrap().memory_access_scope,
        original
    );
    let scope = usable_memory_scope(&session, 1000).unwrap();
    assert_eq!(scope.authorized_input_seq, 1);
    assert_eq!(scope.device_rule, AssistantDeviceScope::CurrentDevice);
    assert_eq!(scope.expires_at_unix_ms, Some(10_000));
    let previous = task_retrieval_snapshot(&session);
    preserve_active_task(&mut session, previous, "goal", 2000).unwrap();
    assert_eq!(
        usable_memory_scope(&session, 1000)
            .unwrap()
            .expires_at_unix_ms,
        Some(2000)
    );
    assert!(usable_memory_scope(&session, 2000).is_none());
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn tool_advertisement_uses_current_flags_and_original_unexpired_consent() {
    let mut session = personal_session("actor", "root", "device");
    bind_opened_task(&mut session, "goal", 2000).unwrap();
    let mut settings = AssistantDataSettings::default();
    settings.automatic_memory_enabled = false;
    assert!(
        !data_tools::for_session(&session, &settings, true, true, false, 1000)
            .iter()
            .any(|tool| tool.name() == SEARCH_MEMORIES)
    );
    settings.automatic_memory_enabled = true;
    assert!(
        data_tools::for_session(&session, &settings, true, true, false, 1000)
            .iter()
            .any(|tool| tool.name() == SEARCH_MEMORIES)
    );
    assert!(data_tools::for_session(&session, &settings, true, true, false, 2000).is_empty());
    session.surface = crate::session::AgentSessionSurface::TerminalAiAssistant;
    assert!(data_tools::for_session(&session, &settings, true, true, false, 1000).is_empty());
}

#[test]
fn wrong_memory_subject_fails_task_binding_without_partially_mutating_history() {
    let mut session = personal_session("actor", "root", "device");
    session
        .assistant_data
        .as_mut()
        .unwrap()
        .memory_access_scope
        .as_mut()
        .unwrap()
        .actor_id = "another-owner".into();
    let before = session.assistant_data.clone();
    assert!(bind_opened_task(&mut session, "goal", 1000).is_err());
    assert_eq!(session.assistant_data, before);
    assert!(usable_memory_scope(&session, 200).is_none());
}

#[test]
fn temporary_or_corrupted_memory_scope_never_becomes_a_usable_original_grant() {
    let mut session = personal_session("actor", "root", "device");
    let data = session.assistant_data.as_mut().unwrap();
    data.temporary = true;
    data.history_access_scope = None;
    assert!(data.validate().is_err());
    data.memory_access_scope = None;
    assert!(data.validate().is_ok());
    assert!(usable_memory_scope(&session, 200).is_none());
    let mut scope = owner_memory_scope(
        "actor",
        "root",
        "device",
        1,
        100,
        "episode",
        1,
        &AssistantDataSettings::default(),
        false,
    )
    .unwrap()
    .unwrap();
    scope.task_id = Some("original-task".into());
    assert!(scope.validate().is_ok());
    scope.expires_at_unix_ms = Some(100);
    assert!(scope.validate().is_err());
    scope.expires_at_unix_ms = Some(101);
    assert!(scope.validate().is_ok());
}

#[test]
fn reading_saved_memory_does_not_advertise_unavailable_mutation_authority() {
    let session = personal_session("actor", "root", "device");
    let settings = AssistantDataSettings::default();
    let tools = data_tools::for_session(&session, &settings, true, false, false, 1000);
    assert!(tools.iter().any(|tool| tool.name() == SEARCH_MEMORIES));
    assert!(
        tools
            .iter()
            .any(|tool| tool.name() == data_tools::READ_MEMORY_HISTORY)
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.name() == data_tools::READ_MEMORY_SOURCE)
    );
    assert!(!tools.iter().any(|tool| tool.name() == MANAGE_MEMORIES));
    assert!(
        data_tools::for_session(&session, &settings, true, true, false, 1000)
            .iter()
            .any(|tool| tool.name() == MANAGE_MEMORIES)
    );
    assert!(data_tools::for_session(&session, &settings, false, true, false, 1000).is_empty());
}
