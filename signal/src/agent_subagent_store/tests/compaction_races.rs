//! Deterministic interleavings at the checkpoint plan/apply and durable save boundaries.
use super::*;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole},
    model_context::*,
    model_profile::WireProtocol,
    replay::SourceContextKey,
    session::TurnState,
};

pub(super) fn add_history(session: &mut PersistedAgentSession) {
    let labels = session
        .conversation
        .iter()
        .filter_map(|m| m.data_envelope.clone())
        .collect::<Vec<_>>();
    // The store fixture contains fabricated tool calls. Give those fixtures
    // an explicit no-reasoning replay decision bound to this test profile.
    for message in &mut session.conversation {
        if !message.tool_calls.is_empty() {
            message.replay_disposition =
                Some(desk_diagnose_core::replay::ReplayDisposition::NotRequired {
                    source_context_key: SourceContextKey::derive(
                        WireProtocol::OpenAiChatCompletions,
                        "fixture",
                        "model:1",
                        "test",
                    ),
                });
        }
    }
    let mut history = Vec::new();
    for index in 0..8 {
        let id = format!("old-history-{index}");
        let text = format!("Synthetic historical padding {}", "x".repeat(5000));
        let mut message = ChatMessage::text(&id, ChatRole::SystemEvent, &text).with_turn_id(&id);
        message.data_envelope = Some(
            desk_diagnose_core::subagent::projection::envelope(
                &id,
                &text,
                "fixture-history",
                &labels,
            )
            .unwrap(),
        );
        history.push(message);
    }
    history.append(&mut session.conversation);
    session.conversation = history;
}

pub(super) fn plan(session: &PersistedAgentSession) -> (CompressionPlan, ValidatedContextSummary) {
    let source = SourceContextKey::derive(
        WireProtocol::OpenAiChatCompletions,
        "fixture",
        "model:1",
        "test",
    );
    let mut policy = PinnedContextPolicy::checkpoint_summary(source, 1, 32_768, 0).unwrap();
    policy.preserve_history = true;
    let protection = ContextProtectionSet {
        current_turn_id: session.current_turn_id.clone(),
        ..Default::default()
    };
    let ContextBuildPlan::NeedsCompression(plan) = plan_model_context(
        &session.conversation,
        &session.model_context_state,
        &policy,
        &protection,
        session.version,
    )
    .unwrap() else {
        panic!("history must actually require compression")
    };
    let raw = serde_json::json!({"reported_observations":[{"text":"Synthetic historical padding was recorded.","source_message_ids":["old-history-0"]}]}).to_string();
    let provenance = CompressorProvenanceV1::for_call(
        &policy,
        "a".repeat(64),
        "b".repeat(64),
        1,
        "c".repeat(64),
        &chrono::Utc::now().to_rfc3339(),
        session.current_turn_id.as_deref().unwrap(),
    );
    let result = parse_validated_context_summary(&raw, &plan, provenance).unwrap();
    (*plan, result)
}

pub(super) fn apply(
    session: &mut PersistedAgentSession,
    plan: &CompressionPlan,
    result: ValidatedContextSummary,
) {
    let (state, _) = apply_validated_checkpoint(
        plan,
        result,
        &session.conversation,
        &session.model_context_state,
        session.version,
    )
    .unwrap();
    assert!(state.entries.iter().any(|entry| entry.checkpoint.is_some()));
    session.model_context_state = state;
}

async fn stored(db: &DatabaseConnection, conversation: &str) -> PersistedAgentSession {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(conversation))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

#[tokio::test]
async fn child_completion_between_parent_checkpoint_plan_and_save_survives_inbox_and_projection() {
    let db = database().await;
    let (mut parent, task_id) = super::parent_wait::waiting_parent(&db).await;
    add_history(&mut parent);
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let (pending, result) = plan(&parent);
    let store = SubAgentStore::new(db.clone());
    let before = store.main_projection_for_turn(&parent).await.unwrap();
    let before: serde_json::Value = serde_json::from_str(&before.text).unwrap();
    assert_eq!(before["runtime_delegation_state"]["unfinished"], 1);
    // The child commits after the main request snapshot, before its checkpoint save.
    super::main_tools::complete_child(&db, &task_id).await;
    apply(&mut parent, &pending, result);
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let restored = stored(&db, "root").await;
    assert_eq!(restored.model_context_state, parent.model_context_state);
    let after = store.main_projection_for_turn(&restored).await.unwrap();
    let after: serde_json::Value = serde_json::from_str(&after.text).unwrap();
    assert_eq!(after["runtime_delegation_state"]["unfinished"], 0);
    assert_eq!(
        after["runtime_delegation_state"]["tasks"][0]["task"]["state"],
        "completed"
    );
    let events = inbox_row::Entity::find()
        .filter(inbox_row::Column::TaskId.eq(&task_id))
        .filter(inbox_row::Column::EventKind.eq("completed"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].model_observed_at_ms.is_none());
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let ParentWaitResolution::Ready(wait) =
        store.resolve_parent_wait("root", "1", "1").await.unwrap()
    else {
        panic!("completed dependency must wake after the compressed parent releases")
    };
    assert_eq!(wait.tasks[0].task_id, task_id);
}

#[tokio::test]
async fn adjustment_between_child_checkpoint_plan_and_save_rejects_old_summary_and_report() {
    let db = database().await;
    let mut old = super::lifecycle::child(&db).await;
    add_history(&mut old);
    save_child_session(&db, &mut old).await.unwrap();
    let binding = old.agent_role.binding().unwrap().clone();
    let task = load(&db, &binding.task_id).await;
    let (pending, result) = plan(&old);
    let store = SubAgentStore::new(db.clone());
    store
        .control_for_owner(
            "root",
            "1",
            "1",
            &desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentControl {
                client_request_id: "adjust-during-checkpoint".into(),
                task_id: binding.task_id.clone(),
                expected_input_revision: binding.input_revision,
                expected_control_revision: binding.control_revision,
                action:
                    desk_agent_protocol::ai_assistant::subagent::SubAgentControlAction::Adjust {
                        message: "Investigate the revised symptom".into(),
                    },
            },
        )
        .await
        .unwrap();
    let current = stored(&db, &old.conversation_id).await;
    assert!(current.input_revision > old.input_revision);
    assert!(current.control_revision > old.control_revision);
    assert!(matches!(
        apply_validated_checkpoint(
            &pending,
            result.clone(),
            &current.conversation,
            &current.model_context_state,
            current.version
        ),
        Err(ModelContextError::StaleCompressionPlan)
    ));
    // An in-flight worker can still apply to its old in-memory copy. Durable CAS
    // must reject that copy, and the old final report must fail the task fence.
    apply(&mut old, &pending, result);
    assert!(save_child_session(&db, &mut old).await.is_err());
    assert!(
        store
            .evaluate_answer_for_turn(&old, task.fence(), &super::lifecycle::report().summary)
            .await
            .is_err()
    );
    assert_eq!(
        stored(&db, &current.conversation_id)
            .await
            .encode_json_for_storage()
            .unwrap(),
        current.encode_json_for_storage().unwrap()
    );
    let revised = load(&db, &binding.task_id).await;
    assert_eq!(revised.binding.deadline_ms, binding.deadline_ms);
    assert_eq!(revised.binding.objective, "Investigate the revised symptom");
    assert_ne!(revised.state, SubAgentState::Completed);
    let candidate = store
        .child_runtime_candidate(&binding.task_id)
        .await
        .unwrap()
        .unwrap();
    candidate.validate().unwrap();
    assert_eq!(candidate.session().agent_role, current.agent_role);
}

use crate::entity::agent_subagent_inbox as inbox_row;
