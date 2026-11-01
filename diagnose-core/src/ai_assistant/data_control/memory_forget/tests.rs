use super::*;
use crate::ai_assistant::{data_tools, test_support::personal_session};
use crate::chat::{ChatRole, ToolCall};

const NOW: &str = "2026-10-22T00:00:00Z";

fn fixture() -> (PersistedAgentSession, AssistantDataControlReceipt, String) {
    let mut session = personal_session("owner", "root", "device");
    session.begin_focus_epoch(1, Vec::new()).unwrap();
    session
        .begin_turn(
            "owner-turn",
            None,
            None,
            1,
            session.scope_snapshot.clone(),
            NOW,
        )
        .unwrap();
    session.adopt_trigger(crate::session::TriggerOrigin::User, "owner-turn");
    session.latest_input_seq = 1;
    session.input_revision = 1;
    session.conversation.push(
        ChatMessage::text("owner-input", ChatRole::User, "请遗忘这两条偏好。")
            .with_turn_id("owner-turn"),
    );
    let calls = ["a", "b"].map(|id| ToolCall { id: format!("forget-{id}"), name: data_tools::MANAGE_MEMORIES.into(),
        arguments_json: serde_json::json!({"operation":"forget", "memory_id":native_ids::stable(Kind::Memory, &id), "expected_revision":1}).to_string() });
    session.conversation.push(
        ChatMessage::assistant_tool_calls(
            "producer",
            "model text with old sensitive context",
            calls.iter().map(ToolCall::to_ref).collect(),
        )
        .with_turn_id("owner-turn"),
    );
    let hash = tool_group_sha256(&session.conversation[1..]).unwrap();
    let receipt = AssistantDataControlReceipt {
        operation_id: "forget-operation".into(),
        outcome: DataControlOutcome::Applied,
        reason: DataControlReason::MemoryForget,
        affected_count: 2,
        control_generation: 2,
        memory_state_generation: 2,
        cleanup_pending: true,
    };
    (session, receipt, hash)
}

#[test]
fn complete_forget_keeps_results_protected_and_the_original_turn_active() {
    let (mut session, receipt, hash) = fixture();
    let original = session.conversation.clone();
    let version = session.version;
    let group = commit_memory_forget_group(&mut session, "producer", &hash, &receipt, NOW).unwrap();
    assert_eq!(&session.conversation[..2], original);
    assert_eq!(session.conversation.len(), 4);
    assert_eq!(session.version, version + 1);
    assert_eq!(session.turn_state, TurnState::Running);
    assert_eq!(session.current_turn_id.as_deref(), Some("owner-turn"));
    assert_eq!(session.handled_input_seq, 0);
    assert_eq!(session.model_conversation().as_ref(), session.conversation);
    let protection = session.context_protection_set();
    assert!(protection.protected_tool_call_ids.contains("forget-a"));
    assert!(protection.protected_tool_call_ids.contains("forget-b"));
    for result in &session.conversation[2..] {
        assert_eq!(
            serde_json::from_str::<AssistantDataControlReceipt>(&result.text).unwrap(),
            receipt
        );
        assert!(!result.text.contains("sensitive"));
    }
    assert_eq!(
        read_memory_forget_group(&session, "producer").unwrap(),
        group
    );
    let decoded =
        PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
    assert_eq!(
        read_memory_forget_group(&decoded, "producer").unwrap(),
        group
    );
}

#[test]
fn transport_replay_does_not_rewrite_a_later_owner_turn() {
    let (mut session, mut receipt, hash) = fixture();
    let group = commit_memory_forget_group(&mut session, "producer", &hash, &receipt, NOW).unwrap();
    session.conversation.push(ChatMessage::text(
        "later-owner",
        ChatRole::User,
        "新的独立任务",
    ));
    session.current_turn_id = Some("later-turn".into());
    session.turn_state = TurnState::Running;
    session.latest_input_seq += 1;
    let before = session.encode_json_for_storage().unwrap();
    receipt.outcome = DataControlOutcome::AlreadyApplied;
    receipt.cleanup_pending = false;
    assert_eq!(
        commit_memory_forget_group(&mut session, "producer", &hash, &receipt, NOW).unwrap(),
        group
    );
    assert_eq!(session.encode_json_for_storage().unwrap(), before);
    receipt.memory_state_generation += 1;
    assert!(commit_memory_forget_group(&mut session, "producer", &hash, &receipt, NOW).is_err());
    assert_eq!(session.encode_json_for_storage().unwrap(), before);
}

#[test]
fn stale_wrong_scope_mixed_or_changed_batches_have_no_session_effects() {
    for case in 0..10 {
        let (mut session, mut receipt, mut hash) = fixture();
        match case {
            0 => receipt.outcome = DataControlOutcome::StaleTarget,
            1 => receipt.outcome = DataControlOutcome::AlreadyApplied,
            2 => receipt.reason = DataControlReason::MemoryClear,
            3 => receipt.affected_count = 1,
            4 => receipt.control_generation += 1,
            5 => hash = "0".repeat(64),
            6 => session.trigger_origin = crate::session::TriggerOrigin::GoalContinuation,
            7 => session.conversation[1].tool_calls[1].name = data_tools::SEARCH_MEMORIES.into(),
            8 => {
                session.conversation[1].tool_calls[1].id =
                    session.conversation[1].tool_calls[0].id.clone()
            }
            _ => session.conversation.push(ChatMessage::tool_result(
                "unbound-result",
                "forget-a",
                "old result",
            )),
        }
        let before = session.encode_json_for_storage().unwrap();
        assert!(
            commit_memory_forget_group(&mut session, "producer", &hash, &receipt, NOW).is_err(),
            "case {case}"
        );
        assert_eq!(
            session.encode_json_for_storage().unwrap(),
            before,
            "case {case}"
        );
    }
}

#[test]
fn ordinary_tool_protection_follows_the_latest_model_batch() {
    let (mut session, receipt, hash) = fixture();
    commit_memory_forget_group(&mut session, "producer", &hash, &receipt, NOW).unwrap();
    session.conversation.push(ChatMessage::assistant_tool_calls(
        "later-call",
        "",
        vec![
            ToolCall {
                id: "later-unresolved".into(),
                name: "device-action".into(),
                arguments_json: "{}".into(),
            }
            .to_ref(),
        ],
    ));
    let protection = session.context_protection_set();
    assert!(
        protection
            .protected_tool_call_ids
            .contains("later-unresolved")
    );
    assert!(!protection.protected_tool_call_ids.contains("forget-a"));
}

#[test]
fn changed_results_cannot_reuse_the_committed_group_digest() {
    for case in 0..7 {
        let (mut session, receipt, hash) = fixture();
        let group =
            commit_memory_forget_group(&mut session, "producer", &hash, &receipt, NOW).unwrap();
        match case {
            0 => session.conversation[1].tool_calls[0]
                .arguments_json
                .push(' '),
            1 => session.conversation[2].text.push(' '),
            2 => session.conversation.swap(2, 3),
            3 => session.conversation[2].tool_ok = Some(false),
            4 => session.conversation[2].tool_call_id = Some("other".into()),
            5 => {
                let mut duplicate = session.conversation[2].clone();
                duplicate.message_id = "duplicate".into();
                session.conversation.push(duplicate);
            }
            _ => {
                session.conversation.remove(3);
            }
        }
        assert_ne!(
            read_memory_forget_group(&session, "producer").ok(),
            Some(group),
            "case {case}"
        );
    }
}

#[test]
fn input_recovery_waits_for_the_actual_model_answer_and_keeps_its_original_turn() {
    use crate::agent_loop::{LoopOutcome, committed_input_outcome};
    let (mut session, receipt, hash) = fixture();
    session.conversation.insert(
        0,
        ChatMessage::text("queued-input", ChatRole::User, "queued"),
    );
    commit_memory_forget_group(&mut session, "producer", &hash, &receipt, NOW).unwrap();
    assert_eq!(committed_input_outcome(&session, "owner-input"), None);
    assert_eq!(committed_input_outcome(&session, "queued-input"), None);
    session.conversation.push(
        ChatMessage::text(
            "confirmation",
            ChatRole::Assistant,
            "The memories were deleted.",
        )
        .with_turn_id("owner-turn"),
    );
    session.conversation.push(
        ChatMessage::text("later-input", ChatRole::User, "new task").with_turn_id("later-turn"),
    );
    session.conversation.push(
        ChatMessage::text("later-answer", ChatRole::Assistant, "unrelated answer")
            .with_turn_id("later-turn"),
    );
    for id in ["owner-input", "queued-input"] {
        assert_eq!(
            committed_input_outcome(&session, id),
            Some(LoopOutcome::Answered("The memories were deleted.".into()))
        );
    }
    assert_eq!(
        committed_input_outcome(&session, "later-input"),
        Some(LoopOutcome::Answered("unrelated answer".into()))
    );
    assert_eq!(committed_input_outcome(&session, "producer"), None);
    assert_eq!(committed_input_outcome(&session, "missing"), None);
}
