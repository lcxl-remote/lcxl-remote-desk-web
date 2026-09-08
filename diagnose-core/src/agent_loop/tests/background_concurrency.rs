use super::*;

#[tokio::test]
async fn multiple_dispatches_in_one_turn_preserve_ids_and_allow_sync_queries() {
    let sess = MemSession::default();
    let mut first = tool_use("c1", "exec_command");
    first.tool_calls.push(ToolCall {
        id: "c2".into(),
        name: "exec_command".into(),
        arguments_json: "{}".into(),
    });
    let model = ScriptModel {
        turns: RefCell::new([first, tool_use("c3", "read_sys"), answer("running")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let a = crate::session::ActionIdentity::agent_exec(1, "task-a", "gen-a");
    let b = crate::session::ActionIdentity::agent_exec(2, "task-b", "gen-b");
    let scripted = tools(vec![
        ExecOutcome::Dispatched(a.clone()),
        ExecOutcome::Dispatched(b.clone()),
    ]);
    let reg = vec![
        mutating_tool("exec_command", Capability::ShellExecConfirmed),
        read_tool("read_sys", Capability::SystemInfo),
    ];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        ChatMessage::text("u", ChatRole::User, "run both"),
        &mut sink,
    )
    .await
    .unwrap();
    let saved = sess.inner.borrow();
    let saved = saved.as_ref().unwrap();
    assert!(saved.execution_state.contains(&a));
    assert!(saved.execution_state.contains(&b));
    assert_eq!(scripted.exec_calls.borrow().len(), 2);
    assert!(
        saved
            .conversation
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("c3"))
    );
}

#[tokio::test]
async fn wait_selects_second_task_and_keeps_first_running() {
    let sess = seeded_executing();
    let b = crate::session::ActionIdentity::agent_exec(10, "task-b", "gen-b");
    sess.inner
        .borrow_mut()
        .as_mut()
        .unwrap()
        .execution_state
        .insert(ExecutionState::Executing { action: b });
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args("wait-b", "wait_for_task", r#"{"task_id":"task-b"}"#),
                answer("done"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools_with_waits(
        vec![],
        vec![WaitOutcome::Completed {
            output: ToolRunOutput {
                content: "done".into(),
                image_data_url: None,
            },
            event_id: Some("done-b".into()),
        }],
    );
    let reg = wait_reg();
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        ChatMessage::text("u", ChatRole::User, "wait for B"),
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(*scripted.wait_calls.borrow(), vec!["task-b"]);
    let saved = sess.inner.borrow();
    let state = &saved.as_ref().unwrap().execution_state;
    assert!(state.task("exec_task9").is_some());
    assert!(state.task("task-b").is_none());
}
