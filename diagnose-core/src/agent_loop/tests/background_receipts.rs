use super::*;

fn original_task_session() -> MemSession {
    let sess = seeded_executing();
    let mut inner = sess.inner.borrow_mut();
    let session = inner.as_mut().unwrap();
    session.conversation.push(ChatMessage::assistant_tool_calls(
        "original-proposal",
        "",
        vec![ToolCallRef {
            id: "c1".into(),
            name: "exec_command".into(),
            arguments_json: "{}".into(),
        }],
    ));
    session
        .conversation
        .push(ChatMessage::background_task_running(
            "accepted-status",
            "c1",
            "exec_task9",
        ));
    drop(inner);
    sess
}

fn waiter() -> ScriptModel {
    ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args("c2", "wait_for_task", r#"{"task_id":"exec_task9"}"#),
                answer("done"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    }
}

#[tokio::test]
async fn background_receipt_wait_in_a_batch_does_not_split_or_ack_an_open_tool_group() {
    let sess = original_task_session();
    let mut first = tool_use_args("c2", "wait_for_task", r#"{"task_id":"exec_task9"}"#);
    first.tool_calls.push(ToolCall {
        id: "c3".into(),
        name: "wait_for_task".into(),
        arguments_json: r#"{"task_id":"exec_task9"}"#.into(),
    });
    let model = ScriptModel {
        turns: RefCell::new([first, answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let output = ToolRunOutput {
        content: "original native result".into(),
        image_data_url: None,
    };
    let envelope = original_results::original(&output, false);
    let completed = || WaitOutcome::CompletedWithReceipt {
        action: crate::session::ActionIdentity::agent_exec(8, "exec_task9", "e9"),
        original_call_id: "c1".into(),
        output: output.clone(),
        event_id: "work:8:done".into(),
        data_envelope: envelope.clone(),
    };
    let scripted = tools_with_waits(vec![], vec![completed(), completed()]);
    let reg = wait_reg();
    let clock = || "2026-06-20T00:01:00Z".to_string();
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        ChatMessage::text("u", ChatRole::User, "is it done?"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert_eq!(scripted.acks.borrow().as_slice(), ["work:8:done"]);
    assert!(scripted.exec_calls.borrow().is_empty());
    let saved = sess.inner.borrow();
    let messages = &saved.as_ref().unwrap().conversation;
    let index = |id| {
        messages
            .iter()
            .position(|m| m.tool_call_id.as_deref() == Some(id))
            .unwrap()
    };
    assert!(index("c2") < index("c3"));
    assert!(
        index("c3")
            < messages
                .iter()
                .position(|m| m.message_id == "work:8:done")
                .unwrap()
    );
    assert_eq!(
        saved.as_ref().unwrap().execution_state,
        ExecutionState::None
    );
}

#[tokio::test]
async fn background_receipt_keeps_original_call_and_label_separate_from_wait_status() {
    let sess = original_task_session();
    let model = waiter();
    let output = ToolRunOutput {
        content: "original native result".into(),
        image_data_url: None,
    };
    let envelope = original_results::original(&output, false);
    let scripted = tools_with_waits(
        vec![],
        vec![WaitOutcome::CompletedWithReceipt {
            action: crate::session::ActionIdentity::agent_exec(8, "exec_task9", "e9"),
            original_call_id: "c1".into(),
            output,
            event_id: "work:8:done".into(),
            data_envelope: envelope.clone(),
        }],
    );
    let reg = wait_reg();
    let clock = || "2026-06-20T00:01:00Z".to_string();
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        ChatMessage::text("u", ChatRole::User, "is it done?"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    let mut saved = sess.inner.borrow_mut();
    let session = saved.as_mut().unwrap();
    assert_eq!(session.execution_state, ExecutionState::None);
    let result = session
        .conversation
        .iter()
        .find(|m| m.message_id == "work:8:done")
        .unwrap();
    assert_eq!(result.role, ChatRole::UntrustedOutput);
    assert_eq!(result.tool_call_id.as_deref(), Some("c1"));
    assert_eq!(result.background_task_id.as_deref(), Some("exec_task9"));
    assert_eq!(result.text, "original native result");
    assert_eq!(result.data_envelope.as_ref(), Some(&envelope));
    let status = session
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c2"))
        .unwrap();
    assert_ne!(status.message_id, result.message_id);
    let label = status.data_envelope.as_ref().unwrap();
    assert_eq!(
        label.provenance.source_provider_id,
        crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
    );
    assert_eq!(
        label.provenance.source_envelope_ids,
        [envelope.envelope_id.clone()]
    );
    assert_eq!(label.retention, envelope.retention);
    assert_eq!(label.allowed_destinations, envelope.allowed_destinations);
    assert_eq!(scripted.acks.borrow().as_slice(), ["work:8:done"]);
    assert!(scripted.mutation_envelope_inputs.borrow().is_empty());
    assert!(!session.apply_completion_with_envelope(
        "work:8:done",
        "e9",
        "c1",
        "exec_task9",
        "original native result",
        Some(envelope),
        "2026-06-20T00:02:00Z"
    ));
}

#[tokio::test]
async fn background_receipt_rejects_mismatched_identity_or_bytes_without_ack() {
    for corruption in [
        "work",
        "action",
        "generation",
        "bytes",
        "image",
        "size",
        "digest",
    ] {
        let sess = original_task_session();
        let model = waiter();
        let mut output = ToolRunOutput {
            content: "original native result".into(),
            image_data_url: None,
        };
        let mut envelope = original_results::original(&output, false);
        let mut action = crate::session::ActionIdentity::agent_exec(8, "exec_task9", "e9");
        match corruption {
            "work" => action.work_id += 1,
            "action" => action.action_request_id = "other".into(),
            "generation" => action.execution_id = "other".into(),
            "bytes" => output.content = "changed".into(),
            "image" => output.image_data_url = Some("data:image/png;base64,AQ==".into()),
            "size" => {
                if let ContentRef::ImmutableBlob { size_bytes, .. } = &mut envelope.content {
                    *size_bytes += 1;
                }
            }
            "digest" => envelope.digest_sha256 = "a".repeat(64),
            _ => unreachable!(),
        }
        let scripted = tools_with_waits(
            vec![],
            vec![WaitOutcome::CompletedWithReceipt {
                action,
                original_call_id: "c1".into(),
                output,
                event_id: "work:8:done".into(),
                data_envelope: envelope,
            }],
        );
        let reg = wait_reg();
        let clock = || "2026-06-20T00:01:00Z".to_string();
        let result = run_agent_turn(
            &exec_deps(&sess, &model, &scripted, &reg, &clock),
            exec_claim(),
            ChatMessage::text("u", ChatRole::User, "is it done?"),
            &mut NullTurnSink,
        )
        .await;
        assert!(result.is_err(), "{corruption}");
        assert!(scripted.acks.borrow().is_empty(), "{corruption}");
        assert!(
            !sess
                .inner
                .borrow()
                .as_ref()
                .unwrap()
                .conversation
                .iter()
                .any(|m| m.message_id == "work:8:done")
        );
        assert_eq!(model.requests.borrow().len(), 1, "{corruption}");
    }
}

#[tokio::test]
async fn background_unknown_wait_retains_original_anchor_for_late_completion() {
    let sess = original_task_session();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args("c2", "wait_for_task", r#"{"task_id":"exec_task9"}"#),
                tool_use_args("c3", "wait_for_task", r#"{"task_id":"exec_task9"}"#),
                answer("done"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let action = crate::session::ActionIdentity::agent_exec(8, "exec_task9", "e9");
    let output = ToolRunOutput {
        content: "late original result".into(),
        image_data_url: None,
    };
    let envelope = original_results::original(&output, false);
    let scripted = tools_with_waits(
        vec![],
        vec![
            WaitOutcome::UnknownWithIdentity {
                action: action.clone(),
                original_call_id: "c1".into(),
            },
            WaitOutcome::CompletedWithReceipt {
                action,
                original_call_id: "c1".into(),
                output,
                event_id: "work:8:done".into(),
                data_envelope: envelope.clone(),
            },
        ],
    );
    let reg = wait_reg();
    let clock = || "2026-06-20T00:01:00Z".to_string();
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        ChatMessage::text("u", ChatRole::User, "is it done?"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    let saved = sess.inner.borrow();
    let session = saved.as_ref().unwrap();
    let result = session
        .conversation
        .iter()
        .find(|m| m.message_id == "work:8:done")
        .unwrap();
    assert_eq!(result.tool_call_id.as_deref(), Some("c1"));
    assert_eq!(result.role, ChatRole::Tool);
    assert_eq!(result.data_envelope.as_ref(), Some(&envelope));
    assert_eq!(session.execution_state, ExecutionState::None);
    assert_eq!(scripted.acks.borrow().as_slice(), ["work:8:done"]);
    assert_eq!(
        session
            .conversation
            .iter()
            .filter(|m| m.tool_call_id.as_deref() == Some("c1"))
            .count(),
        1
    );
}

#[tokio::test]
async fn background_wait_rejects_missing_duplicate_or_wrong_original_anchor() {
    for corruption in ["missing", "duplicate", "call", "state-anchor"] {
        for completed in [false, true] {
            let sess = original_task_session();
            {
                let mut saved = sess.inner.borrow_mut();
                let session = saved.as_mut().unwrap();
                match corruption {
                    "missing" => {
                        session.conversation.pop();
                    }
                    "duplicate" => session
                        .conversation
                        .push(session.conversation.last().unwrap().clone()),
                    "call" => {
                        session.conversation.last_mut().unwrap().tool_call_id = Some("other".into())
                    }
                    "state-anchor" => {
                        session.execution_state = ExecutionState::OutcomeUnknown {
                            action: crate::session::ActionIdentity::agent_exec(
                                8,
                                "exec_task9",
                                "e9",
                            ),
                            placeholder_message_id: "missing".into(),
                            since: "t".into(),
                        }
                    }
                    _ => unreachable!(),
                }
            }
            let model = waiter();
            let action = crate::session::ActionIdentity::agent_exec(8, "exec_task9", "e9");
            let output = ToolRunOutput {
                content: "late original result".into(),
                image_data_url: None,
            };
            let envelope = original_results::original(&output, false);
            let outcome = if completed {
                WaitOutcome::CompletedWithReceipt {
                    action,
                    original_call_id: "c1".into(),
                    output,
                    event_id: "work:8:done".into(),
                    data_envelope: envelope,
                }
            } else {
                WaitOutcome::UnknownWithIdentity {
                    action,
                    original_call_id: "c1".into(),
                }
            };
            let scripted = tools_with_waits(vec![], vec![outcome]);
            let reg = wait_reg();
            let clock = || "2026-06-20T00:01:00Z".to_string();
            assert!(
                run_agent_turn(
                    &exec_deps(&sess, &model, &scripted, &reg, &clock),
                    exec_claim(),
                    ChatMessage::text("u", ChatRole::User, "is it done?"),
                    &mut NullTurnSink
                )
                .await
                .is_err(),
                "{corruption}"
            );
            assert!(scripted.acks.borrow().is_empty());
            assert_ne!(
                sess.inner.borrow().as_ref().unwrap().execution_state,
                ExecutionState::None
            );
        }
    }
}

#[test]
fn background_status_inherits_proposal_boundary_without_requesting_a_native_receipt() {
    let sess = MemSession::default();
    let model = waiter();
    let scripted = tools(vec![]);
    let reg = wait_reg();
    let clock = || "t".into();
    let deps = exec_deps(&sess, &model, &scripted, &reg, &clock);
    let call = ToolCall {
        id: "wait".into(),
        name: "wait_for_task".into(),
        arguments_json: "{}".into(),
    };
    let mut session = PersistedAgentSession::new("conv", "actor", "device", 1, exec_scope(), "t");
    let mut proposal = ChatMessage::assistant_tool_calls(
        "proposal",
        "",
        vec![ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        }],
    );
    let parent = original_results::original(
        &ToolRunOutput {
            content: "proposal".into(),
            image_data_url: None,
        },
        true,
    );
    proposal.data_envelope = Some(parent.clone());
    session.conversation.push(proposal);
    for text in ["still running", "outcome unknown", "wait error"] {
        append_mutating_result(
            &deps,
            &mut session,
            &call,
            ChatMessage::tool_result(text, "wait", text),
        )
        .unwrap();
        let label = session
            .conversation
            .last()
            .unwrap()
            .data_envelope
            .as_ref()
            .unwrap();
        assert_eq!(
            label.provenance.source_provider_id,
            crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
        );
        assert_eq!(
            label.provenance.source_envelope_ids,
            [parent.envelope_id.clone()]
        );
        assert_eq!(label.retention, parent.retention);
    }
    assert!(scripted.mutation_envelope_inputs.borrow().is_empty());
}
