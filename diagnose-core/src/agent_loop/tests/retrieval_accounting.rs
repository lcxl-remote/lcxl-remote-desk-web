use super::super::retrieval_accounting::{self as accounting, Admission};
use super::*;
use crate::ai_assistant::{
    history::serialized_tool_response_bytes, test_support::personal_session,
};

fn model() -> ScriptModel {
    ScriptModel {
        turns: RefCell::new(Default::default()),
        requests: Rc::new(RefCell::new(Vec::new())),
    }
}

fn tools() -> RecordingTools {
    RecordingTools {
        calls: Rc::new(RefCell::new(Vec::new())),
        reply: String::new(),
    }
}

fn read(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: "{}".into(),
    }
}

#[tokio::test]
async fn pending_read_is_durable_and_completed_response_replays_without_reopening_or_querying() {
    let store = MemSession::default();
    let model = model();
    let tools = tools();
    let clock_value = RefCell::new("2026-10-20T00:00:00Z");
    let clock = || clock_value.borrow().to_string();
    let deps = deps(&store, &model, &tools, &[], &clock);
    let mut session = personal_session("actor", "root", "device");
    let call = read(
        "call-1",
        crate::conversation_history::LOAD_CONVERSATION_HISTORY_TOOL_NAME,
    );
    session.conversation.push(ChatMessage::assistant_tool_calls(
        "producer",
        "",
        vec![ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        }],
    ));
    let Admission::New(reserved) = accounting::reserve(&deps, &mut session, "producer", &call)
        .await
        .unwrap()
    else {
        panic!("new read")
    };
    let persisted = store.inner.borrow().clone().unwrap();
    assert_eq!(
        persisted
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget
            .read_bytes,
        32768
    );
    assert!(
        persisted
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget
            .reservations[0]
            .actual_bytes
            .is_none()
    );
    let admitted_at = persisted
        .assistant_data
        .as_ref()
        .unwrap()
        .retrieval_budget
        .reservations[0]
        .reserved_at_unix_ms;
    assert_eq!(
        admitted_at,
        chrono::DateTime::parse_from_rfc3339("2026-10-20T00:00:00Z")
            .unwrap()
            .timestamp_millis() as u64
    );
    let saves = *store.saves.borrow();
    session = persisted;
    *clock_value.borrow_mut() = "2026-10-20T01:00:00Z";
    let Admission::New(retried) = accounting::reserve(&deps, &mut session, "producer", &call)
        .await
        .unwrap()
    else {
        panic!("pending read")
    };
    assert_eq!(reserved.id, retried.id);
    assert_eq!(*store.saves.borrow(), saves);
    assert_eq!(
        session
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget
            .reservations[0]
            .reserved_at_unix_ms,
        admitted_at
    );
    let mut result = ChatMessage::tool_result("result", &call.id, "{\"text\":\"a\\nb\"}");
    result.tool_ok = Some(true);
    let bytes = serialized_tool_response_bytes(&call.id, &result.text, None).unwrap();
    accounting::settle(&mut session, &retried, &result).unwrap();
    session.conversation.push(result.clone());
    store.save(&mut session).await.unwrap();
    let Admission::Replay(replayed) = accounting::reserve(&deps, &mut session, "producer", &call)
        .await
        .unwrap()
    else {
        panic!("committed replay")
    };
    assert_eq!(*replayed, result);
    let budget = &session.assistant_data.as_ref().unwrap().retrieval_budget;
    assert_eq!(budget.read_calls, 1);
    assert_eq!(budget.read_bytes, u64::from(bytes));
    let mut changed = call.clone();
    changed.arguments_json = "{\"limit\":1}".into();
    assert!(
        accounting::reserve(&deps, &mut session, "producer", &changed)
            .await
            .is_err()
    );
    assert!(tools.calls.borrow().is_empty());
}

#[tokio::test]
async fn legacy_attachment_and_cross_conversation_reads_share_one_eight_call_episode() {
    let store = MemSession::default();
    let model = model();
    let tools = tools();
    let clock = || "2026-10-20T00:00:00Z".into();
    let deps = deps(&store, &model, &tools, &[], &clock);
    let mut session = personal_session("actor", "root", "device");
    let names = [
        crate::conversation_history::LOAD_CONVERSATION_HISTORY_TOOL_NAME,
        crate::conversation_attachment::READ_ATTACHMENT_TOOL,
        crate::ai_assistant::data_tools::READ_CONVERSATION,
        crate::ai_assistant::data_tools::READ_MEMORY_SOURCE,
    ];
    for index in 0..8 {
        let call = read(&format!("call-{index}"), names[index % names.len()]);
        let Admission::New(reserved) = accounting::reserve(&deps, &mut session, "producer", &call)
            .await
            .unwrap()
        else {
            panic!("new read")
        };
        accounting::settle(
            &mut session,
            &reserved,
            &ChatMessage::tool_result("result", &call.id, "{}"),
        )
        .unwrap();
    }
    let data = &session.assistant_data.as_ref().unwrap();
    data.validate().unwrap();
    let decoded = serde_json::from_str::<crate::ai_assistant::data_control::AssistantDataSession>(
        &serde_json::to_string(data).unwrap(),
    )
    .unwrap();
    session.assistant_data = Some(Box::new(decoded));
    for name in names {
        assert!(
            accounting::reserve(&deps, &mut session, "next-segment", &read("ninth", name))
                .await
                .is_err()
        );
    }
    assert_eq!(
        session
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget
            .read_calls,
        8
    );
}

#[tokio::test]
async fn admitted_invalid_history_input_settles_only_the_small_framed_failure() {
    let store = MemSession::default();
    let model = model();
    let tools = tools();
    let clock = || "2026-10-20T00:00:00Z".into();
    let deps = deps(&store, &model, &tools, &[], &clock);
    let mut session = personal_session("actor", "root", "device");
    let mut call = read(
        "invalid-input",
        crate::conversation_history::LOAD_CONVERSATION_HISTORY_TOOL_NAME,
    );
    call.arguments_json = "{\"limit\":0}".into();
    let mut attachment_budget = 32768;
    let result = accounting::dispatch_legacy(
        &deps,
        &mut session,
        &call,
        "producer",
        "result".into(),
        None,
        &mut attachment_budget,
        &crate::model_observability::tool::ToolObservation::default(),
    )
    .await
    .unwrap();
    assert!(!result.ok);
    assert!(!result.replayed);
    assert_eq!(result.message.tool_ok, Some(false));
    let budget = &session.assistant_data.as_ref().unwrap().retrieval_budget;
    assert_eq!(budget.read_calls, 1);
    assert_eq!(
        budget.read_bytes,
        u64::from(serialized_tool_response_bytes(&call.id, &result.message.text, None).unwrap())
    );
    assert!(budget.read_bytes < 1024);
    assert_eq!(attachment_budget, 32768);
    assert!(tools.calls.borrow().is_empty());
}

#[tokio::test]
async fn other_surfaces_keep_their_original_local_history_limits() {
    let store = MemSession::default();
    let model = model();
    let tools = tools();
    let clock = || "2026-10-20T00:00:00Z".into();
    let deps = deps(&store, &model, &tools, &[], &clock);
    let mut session = personal_session("actor", "root", "device");
    session.surface = crate::session::AgentSessionSurface::TerminalAiAssistant;
    session.assistant_data = None;
    let call = read(
        "legacy",
        crate::conversation_attachment::READ_ATTACHMENT_TOOL,
    );
    let Admission::New(reserved) = accounting::reserve(&deps, &mut session, "producer", &call)
        .await
        .unwrap()
    else {
        panic!("untracked read")
    };
    assert!(reserved.id.is_none());
    assert!(accounting::fits(
        &reserved,
        &ChatMessage::tool_result("result", &call.id, "x".repeat(65536))
    ));
    assert_eq!(*store.saves.borrow(), 0);
}

async fn exercise_positive_remaining_read_bytes() {
    for remaining in [0_u64, 1, 511, 512, 513] {
        let store = MemSession::default();
        let model = model();
        let tools = tools();
        let clock = || "2026-10-20T00:00:00Z".into();
        let deps = deps(&store, &model, &tools, &[], &clock);
        let mut session = personal_session("actor", "root", "device");
        session
            .assistant_data
            .as_mut()
            .unwrap()
            .retrieval_budget
            .limits
            .read_bytes = remaining;
        let call = read(
            "small-budget",
            crate::ai_assistant::data_tools::READ_CONVERSATION,
        );
        let result =
            crate::future::boxed(|| accounting::reserve(&deps, &mut session, "producer", &call))
                .await;
        if remaining == 0 {
            assert!(result.is_err());
            assert_eq!(*store.saves.borrow(), 0);
            continue;
        }
        let Admission::New(reserved) = result.unwrap() else {
            panic!("new read")
        };
        assert_eq!(u64::from(reserved.upper_bytes), remaining);
        // An in-flight call retains its reservation even with no unreserved bytes.
        let Admission::New(replayed) = accounting::reserve(&deps, &mut session, "producer", &call)
            .await
            .unwrap()
        else {
            panic!("in-flight reservation")
        };
        assert_eq!(reserved.id, replayed.id);
        assert_eq!(reserved.upper_bytes, replayed.upper_bytes);
        let budget = &session.assistant_data.as_ref().unwrap().retrieval_budget;
        assert_eq!(budget.read_calls, 1);
        assert_eq!(budget.read_bytes, remaining);
        assert!(
            accounting::reserve(
                &deps,
                &mut session,
                "producer",
                &read("another-read", &call.name)
            )
            .await
            .is_err()
        );
        let encoder = crate::ai_assistant::retrieval_response::ToolReplyBudget::new(
            &call.id,
            reserved.upper_bytes,
        )
        .unwrap();
        assert!(
            encoder
                .encode(&serde_json::json!({"content": "x".repeat(1024)}))
                .is_err()
        );
        assert!(tools.calls.borrow().is_empty());
    }
}

#[tokio::test]
async fn positive_remaining_read_bytes_are_attempted_without_a_fixed_floor() {
    exercise_positive_remaining_read_bytes().await;
}

#[test]
fn positive_remaining_read_bytes_fit_explicit_small_stacks() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        std::thread::Builder::new()
            .stack_size(bytes)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(exercise_positive_remaining_read_bytes());
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
