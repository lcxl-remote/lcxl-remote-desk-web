use super::*;
use crate::session::ActionIdentity;
use desk_agent_protocol::data_lineage::{RetentionBoundary, Sensitivity};

pub(super) fn original(output: &ToolRunOutput, ephemeral: bool) -> DataEnvelope {
    use crate::{action_result::ActionResultOrigin, action_turn_fence::AssistantTurnFence};
    ActionResultOrigin {
        command_completion: None,
        schema_version: 1,
        turn_fence: AssistantTurnFence {
            schema_version: 1,
            conversation_id: "conv".into(),
            turn_id: "original-turn".into(),
            actor_id: "actor".into(),
            device_id: "device".into(),
            input_revision: 1,
            lease_token: 1,
        },
        tool_call_id: "c1".into(),
        provider_id: "test.provider".into(),
        tool_name: "exec_command".into(),
        source_object_id: "device:c1".into(),
        source_envelope_ids: vec!["original-input".into()],
        sensitivity: Sensitivity::Sensitive,
        retention: RetentionBoundary {
            expires_at_unix_ms: Some(12345),
            delete_with_run: true,
        },
        ephemeral,
    }
    .receipt(
        ActionIdentity::agent_exec(42, "request", "generation"),
        1,
        1000,
        output,
    )
    .unwrap()
    .envelope
}

fn model() -> ScriptModel {
    ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command"), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    }
}

#[tokio::test]
async fn original_result_preserves_label_without_factory_or_new_input_binding() {
    // A diagnostic turn isolates the shared transport from strict Assistant
    // egress policy. Preserving an expired label is not permission to export it.
    for ephemeral in [false, true] {
        let sess = MemSession::default();
        let model = model();
        let output = ToolRunOutput {
            content: "exit_code=0".into(),
            image_data_url: None,
        };
        let envelope = original(&output, ephemeral);
        let scripted = tools(vec![ExecOutcome::Executed {
            output,
            event_id: Some("durable-done-42".into()),
            data_envelope: Some(envelope.clone()),
        }]);
        let reg = vec![mutating_tool(
            "exec_command",
            Capability::ShellExecConfirmed,
        )];
        let clock = || "2026-08-30T12:00:00Z".to_string();
        let mut user = ChatMessage::text("u", ChatRole::User, "restart it");
        let mut input_label = original(
            &ToolRunOutput {
                content: user.text.clone(),
                image_data_url: None,
            },
            false,
        );
        input_label.envelope_id = "new-input-label".into();
        user.data_envelope = Some(input_label);
        let result = run_agent_turn(
            &exec_deps(&sess, &model, &scripted, &reg, &clock),
            exec_claim(),
            user,
            &mut NullTurnSink,
        )
        .await
        .unwrap();
        assert_eq!(result, LoopOutcome::Answered("done".into()));
        assert!(scripted.mutation_envelope_inputs.borrow().is_empty());
        assert_eq!(scripted.acks.borrow().as_slice(), ["durable-done-42"]);
        let saved = sess.inner.borrow();
        let message = saved
            .as_ref()
            .unwrap()
            .conversation
            .iter()
            .find(|message| message.message_id == "durable-done-42")
            .unwrap();
        assert_eq!(message.data_envelope.as_ref(), Some(&envelope));
        assert_eq!(message.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(message.text, "exit_code=0");
    }
}

#[tokio::test]
async fn inconsistent_original_result_is_not_saved_acknowledged_or_relabelled() {
    for corruption in ["bytes", "image", "size", "content-digest", "schema"] {
        let sess = MemSession::default();
        let model = model();
        let mut output = ToolRunOutput {
            content: "exit_code=0".into(),
            image_data_url: None,
        };
        let mut envelope = original(&output, false);
        match corruption {
            "bytes" => output.content = "different result".into(),
            "image" => output.image_data_url = Some("data:image/png;base64,AQ==".into()),
            "size" => {
                if let ContentRef::ImmutableBlob { size_bytes, .. } = &mut envelope.content {
                    *size_bytes += 1;
                }
            }
            "content-digest" => {
                if let ContentRef::ImmutableBlob { sha256, .. } = &mut envelope.content {
                    *sha256 = "a".repeat(64);
                }
            }
            "schema" => envelope.schema_version += 1,
            _ => unreachable!(),
        }
        let scripted = tools(vec![ExecOutcome::Executed {
            output,
            event_id: Some("durable-done-42".into()),
            data_envelope: Some(envelope),
        }]);
        let reg = vec![mutating_tool(
            "exec_command",
            Capability::ShellExecConfirmed,
        )];
        let clock = || "t".to_string();
        let error = run_agent_turn(
            &exec_deps(&sess, &model, &scripted, &reg, &clock),
            exec_claim(),
            ChatMessage::text("u", ChatRole::User, "restart it"),
            &mut NullTurnSink,
        )
        .await
        .unwrap_err();
        assert!(!error.safe_for_model && !error.retryable, "{corruption}");
        assert_eq!(error.kind, AgentErrorKind::Internal);
        assert!(scripted.acks.borrow().is_empty(), "{corruption}");
        assert!(
            scripted.mutation_envelope_inputs.borrow().is_empty(),
            "{corruption}"
        );
        assert_eq!(model.requests.borrow().len(), 1, "{corruption}");
        assert!(
            sess.inner
                .borrow()
                .as_ref()
                .unwrap()
                .conversation
                .iter()
                .all(|message| message.message_id != "durable-done-42"),
            "{corruption}"
        );
    }
}

#[tokio::test]
async fn original_result_save_failure_leaves_delivery_unacknowledged() {
    let sess = MemSession {
        fail_save_with_message_id: Some("durable-done-42"),
        ..Default::default()
    };
    let model = model();
    let output = ToolRunOutput {
        content: "exit_code=0".into(),
        image_data_url: None,
    };
    let envelope = original(&output, false);
    let scripted = tools(vec![ExecOutcome::Executed {
        output,
        event_id: Some("durable-done-42".into()),
        data_envelope: Some(envelope.clone()),
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    assert!(
        run_agent_turn(
            &exec_deps(&sess, &model, &scripted, &reg, &clock),
            exec_claim(),
            ChatMessage::text("u", ChatRole::User, "restart it"),
            &mut NullTurnSink,
        )
        .await
        .is_err()
    );
    assert_eq!(scripted.exec_calls.borrow().as_slice(), ["c1"]);
    assert!(scripted.acks.borrow().is_empty());
    assert!(scripted.mutation_envelope_inputs.borrow().is_empty());
    assert!(
        sess.inner
            .borrow()
            .as_ref()
            .unwrap()
            .conversation
            .iter()
            .all(|message| message.message_id != "durable-done-42")
    );
}

#[tokio::test]
async fn original_failure_preserves_receipt_reports_failure_and_stops_the_group() {
    let sess = MemSession::default();
    let mut first = tool_use("c1", "exec_command");
    first.tool_calls.push(ToolCall {
        id: "c2".into(),
        name: "exec_command".into(),
        arguments_json: "{}".into(),
    });
    let model = ScriptModel {
        turns: RefCell::new([first, answer("failed")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    // Opaque content cannot determine success: the Provider's typed outcome does.
    let output = ToolRunOutput {
        content: "a native receipt without an error keyword".into(),
        image_data_url: None,
    };
    let envelope = original(&output, true);
    let scripted = tools(vec![ExecOutcome::Failed {
        output: output.clone(),
        event_id: Some("durable-failed-42".into()),
        data_envelope: Some(envelope.clone()),
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let events = Rc::new(RefCell::new(vec![]));
    let clock = || "t".to_string();
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        ChatMessage::text("u", ChatRole::User, "run the actions"),
        &mut EventLog(events.clone()),
    )
    .await
    .unwrap();
    assert_eq!(scripted.exec_calls.borrow().as_slice(), ["c1"]);
    assert_eq!(scripted.acks.borrow().as_slice(), ["durable-failed-42"]);
    let saved = sess.inner.borrow();
    let messages = &saved.as_ref().unwrap().conversation;
    let result = messages
        .iter()
        .find(|m| m.message_id == "durable-failed-42")
        .unwrap();
    assert_eq!(result.text, output.content);
    assert_eq!(result.tool_call_id.as_deref(), Some("c1"));
    assert_eq!(result.data_envelope.as_ref(), Some(&envelope));
    let skipped = messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c2"))
        .unwrap();
    assert!(skipped.text.contains("prior action in this group failed"));
    assert!(
        events
            .borrow()
            .iter()
            .any(|e| e.starts_with("finished:c1:false:"))
    );
    assert!(
        !events
            .borrow()
            .iter()
            .any(|e| e.starts_with("finished:c1:true:"))
    );
}

#[tokio::test]
async fn original_failure_rejects_corruption_and_never_acks_before_save() {
    for case in ["corrupt-output", "corrupt-label", "save-failed"] {
        let sess = MemSession {
            fail_save_with_message_id: (case == "save-failed").then_some("durable-failed-42"),
            ..Default::default()
        };
        let model = model();
        let mut output = ToolRunOutput {
            content: "native failure".into(),
            image_data_url: None,
        };
        let mut envelope = original(&output, false);
        if case == "corrupt-output" {
            output.content = "altered".into();
        }
        if case == "corrupt-label" {
            envelope.schema_version += 1;
        }
        let scripted = tools(vec![ExecOutcome::Failed {
            output,
            event_id: Some("durable-failed-42".into()),
            data_envelope: Some(envelope),
        }]);
        let reg = vec![mutating_tool(
            "exec_command",
            Capability::ShellExecConfirmed,
        )];
        let clock = || "t".to_string();
        assert!(
            run_agent_turn(
                &exec_deps(&sess, &model, &scripted, &reg, &clock),
                exec_claim(),
                ChatMessage::text("u", ChatRole::User, "run it"),
                &mut NullTurnSink,
            )
            .await
            .is_err(),
            "{case}"
        );
        assert!(scripted.acks.borrow().is_empty(), "{case}");
        assert!(
            scripted.mutation_envelope_inputs.borrow().is_empty(),
            "{case}"
        );
        assert!(
            !sess
                .inner
                .borrow()
                .as_ref()
                .unwrap()
                .conversation
                .iter()
                .any(|m| m.message_id == "durable-failed-42"),
            "{case}"
        );
    }
}
