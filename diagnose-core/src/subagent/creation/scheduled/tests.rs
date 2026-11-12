use super::*;
use crate::{
    chat::ChatRole,
    model_message_labels::model_bound_user_message,
    schedule::{
        definition::ScheduledTaskDefinition,
        fresh_session::{FreshSessionInput, initial_session},
        published_input::{model_bound_published_input, validate_published_input},
    },
    subagent::{DelegationSource, creation::CreationEnvelope},
};
use desk_agent_protocol::{
    AgentScope, ExecutionMode, data_lineage::DestinationIdentity, schedule::policy::TaskBudget,
};

const RUN: &str = "schedule-run-test";
const PROMPT: &str = "Inspect the published report and cite the observed facts.";
const NOW: &str = "2026-09-30T00:00:00Z";

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 2,
        model_id: "model".into(),
        profile_revision: 3,
    }
}

fn fixture() -> (
    ScheduledCreationSource,
    crate::session::PersistedAgentSession,
    i64,
) {
    fixture_with_tokens(500)
}

fn fixture_with_tokens(
    tokens: u64,
) -> (
    ScheduledCreationSource,
    crate::session::PersistedAgentSession,
    i64,
) {
    let definition = ScheduledTaskDefinition {
        schedule_id: "schedule-test".into(),
        task_revision: 2,
        target_device_id: "device-test".into(),
        prompt: PROMPT.into(),
        locale: Some("zh-CN".into()),
        model_id: None,
        policy_revision: 3,
        budget: TaskBudget {
            max_runs_per_utc_day: 2,
            max_calls_per_run: 9,
            max_model_tokens_per_run: tokens,
            max_runtime_seconds: 60,
        },
    };
    let now = chrono::DateTime::parse_from_rfc3339(NOW)
        .unwrap()
        .timestamp_millis();
    let source = ScheduledCreationSource::capture(
        TaskGrantProvenance {
            schedule_id: "schedule-test".into(),
            scheduled_run_id: RUN.into(),
            task_revision: 2,
            policy_revision: 3,
            recovery_epoch: 1,
        },
        &definition,
        now,
    )
    .unwrap();
    let mut session = initial_session(
        &definition,
        FreshSessionInput {
            run_id: RUN,
            actor_id: "1",
            prompt: PROMPT,
            locale: Some("zh-CN"),
            policy_revision: crate::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
            scope: AgentScope {
                granted: Vec::new(),
                mode: ExecutionMode::SuggestOnly,
                expires_at: None,
                policy_name: None,
            },
            now: NOW,
        },
    )
    .unwrap();
    session.version = 1;
    (source, session, now)
}

#[test]
fn occurrence_source_freezes_task_limits_and_original_deadline() {
    let (source, session, now) = fixture();
    let creation =
        CreationEnvelope::capture_scheduled(&session, source.clone(), destination()).unwrap();
    creation.validate().unwrap();
    assert_eq!(creation.scheduled_source, Some(source));
    assert!(creation.original_read_context.is_none());
    assert_eq!(creation.owner_requirement.role, ChatRole::User);
    assert_eq!(
        creation
            .owner_requirement
            .data_envelope
            .as_ref()
            .unwrap()
            .provenance
            .source_provider_id,
        crate::schedule::published_input::PUBLISHED_INPUT_PROVIDER
    );
    let original = creation.new_group(now, None, 1).unwrap();
    assert_eq!(original.limits.total.tool_calls, 9);
    assert_eq!(original.limits.total.tokens, Some(500));
    assert_eq!(original.limits.child_ceiling().tokens, Some(400));
    assert_eq!(original.limits.deadline_ms, now + 60_000);
    let later = creation.new_group(now + 20_000, None, 1).unwrap();
    assert_eq!(later.group_id, original.group_id);
    assert_eq!(later.limits.deadline_ms, original.limits.deadline_ms);
    assert!(creation.new_group(now + 60_000, None, 1).is_err());
    let stored = serde_json::to_string(&creation).unwrap();
    let restored: CreationEnvelope = serde_json::from_str(&stored).unwrap();
    assert_eq!(restored, creation);
    let projection = restored.child_source_message("child-test").unwrap();
    assert_eq!(projection.role, ChatRole::SystemEvent);
    assert_eq!(
        projection.data_envelope.unwrap().allowed_destinations,
        vec![destination()]
    );
}

#[test]
fn scheduled_evidence_cannot_be_attached_to_a_user_or_another_occurrence() {
    let (source, session, _) = fixture();
    let original = CreationEnvelope::capture_scheduled(&session, source, destination()).unwrap();
    for case in 0..8 {
        let mut changed = original.clone();
        match case {
            0 => changed.scheduled_source = None,
            1 => changed.source = DelegationSource::UserInput { input_revision: 1 },
            2 => {
                changed
                    .scheduled_source
                    .as_mut()
                    .unwrap()
                    .provenance
                    .scheduled_run_id = "schedule-run-other".into()
            }
            3 => {
                changed
                    .scheduled_source
                    .as_mut()
                    .unwrap()
                    .provenance
                    .task_revision += 1
            }
            4 => {
                changed
                    .scheduled_source
                    .as_mut()
                    .unwrap()
                    .provenance
                    .policy_revision += 1
            }
            5 => changed.parent_input_revision = 2,
            6 => changed.device_id = "device-other".into(),
            7 => {
                changed.owner_requirement =
                    model_bound_user_message(format!("{RUN}:input"), PROMPT.into(), destination())
                        .unwrap()
                        .with_turn_id(format!("{RUN}-turn"))
            }
            _ => unreachable!(),
        }
        assert!(changed.validate().is_err(), "case {case}");
    }
    assert!(
        CreationEnvelope::capture(
            &session,
            original.source.clone(),
            original.owner_requirement,
            None,
            Default::default(),
        )
        .is_err()
    );
}

#[test]
fn initial_capture_does_not_replace_an_existing_label_or_claim() {
    let (source, session, _) = fixture();
    for case in 0..6 {
        let mut changed = session.clone();
        match case {
            0 => {
                changed.conversation[0] =
                    model_bound_user_message(format!("{RUN}:input"), PROMPT.into(), destination())
                        .unwrap()
                        .with_turn_id(format!("{RUN}-turn"))
            }
            1 => changed.version = 2,
            2 => changed.current_request_id = Some("schedule-run-other".into()),
            3 => changed.trigger_origin = crate::session::TriggerOrigin::User,
            4 => changed.active_control_connection_id = Some("browser-controller".into()),
            5 => {
                changed.agent_role = crate::subagent::AgentRole::SubAgent {
                    binding: Box::new(crate::subagent::DelegatedTaskBinding {
                        targets: Vec::new(),
                        root_conversation_id: "parent".into(),
                        group_id: "group".into(),
                        task_id: "child".into(),
                        source: DelegationSource::ScheduledOccurrence {
                            schedule_id: source.provenance.schedule_id.clone(),
                            occurrence_id: RUN.into(),
                        },
                        objective: "Inspect the report".into(),
                        acceptance_criteria: vec!["Cite observations".into()],
                        input_revision: 1,
                        control_revision: 1,
                        source_epoch: 1,
                        deadline_ms: source.deadline_ms().unwrap(),
                    }),
                }
            }
            _ => unreachable!(),
        }
        assert!(
            CreationEnvelope::capture_scheduled(&changed, source.clone(), destination()).is_err(),
            "case {case}"
        );
    }
    let definition = source.validate().unwrap().clone();
    let mut labelled = session;
    labelled.conversation[0] =
        model_bound_published_input(RUN, PROMPT, &definition, destination()).unwrap();
    assert!(CreationEnvelope::capture_scheduled(&labelled, source, destination()).is_ok());
}

#[test]
fn published_label_binds_definition_occurrence_and_model() {
    let (source, _, _) = fixture();
    let contract = source.validate().unwrap().clone();
    let message = model_bound_published_input(RUN, PROMPT, &contract, destination()).unwrap();
    let label = message.data_envelope.as_ref().unwrap();
    assert!(matches!(
        &label.content,
        desk_agent_protocol::data_lineage::ContentRef::Message { message_id, size_bytes, .. }
            if *message_id == message.message_id && *size_bytes == PROMPT.len() as u64
    ));
    assert!(label.content_sha256().is_none());
    validate_published_input(&message, RUN, &contract, &destination()).unwrap();
    assert!(
        validate_published_input(&message, "schedule-run-other", &contract, &destination())
            .is_err()
    );
    let changed_destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 3,
        model_id: "model".into(),
        profile_revision: 3,
    };
    assert!(validate_published_input(&message, RUN, &contract, &changed_destination).is_err());
    let mut different = contract.clone();
    different.task_revision += 1;
    assert!(validate_published_input(&message, RUN, &different, &destination()).is_err());
    let mut bridge = message;
    bridge
        .data_envelope
        .as_mut()
        .unwrap()
        .provenance
        .source_provider_id = "assistant-runtime-control".into();
    assert!(validate_published_input(&bridge, RUN, &contract, &destination()).is_err());
}

#[test]
fn invalid_definition_and_deadline_are_rejected_before_allocation() {
    let (source, _, now) = fixture();
    for case in 0..4 {
        let mut changed = source.clone();
        match case {
            0 => changed.started_at_ms = 0,
            1 => changed.definition.prompt = String::new(),
            2 => changed.definition.task_revision += 1,
            3 => changed.started_at_ms = i64::MAX - 1,
            _ => unreachable!(),
        }
        assert!(changed.validate().is_err(), "case {case}");
    }
    assert_eq!(source.deadline_ms().unwrap(), now + 60_000);
}

#[test]
fn scheduled_source_keeps_large_pinned_token_limits() {
    let (source, session, now) = fixture_with_tokens(2_000_000);
    let creation = CreationEnvelope::capture_scheduled(&session, source, destination()).unwrap();
    let group = creation.new_group(now, None, 1).unwrap();
    assert_eq!(group.limits.total.tokens, Some(2_000_000));
    assert_eq!(group.limits.child_ceiling().tokens, Some(1_600_000));
}
