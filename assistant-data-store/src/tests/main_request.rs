use super::*;
use crate::main_request::{self as queue, FrozenOwnerInput};
use desk_agent_protocol::ai_assistant::{AiAssistantAsk, MainRequestFailure, MainRequestState};
use desk_diagnose_core::input_read_context::ReadContextSelection;

fn input(message_id: &str, text: &str) -> FrozenOwnerInput {
    FrozenOwnerInput {
        ask: AiAssistantAsk {
            question: text.into(),
            client_message_id: message_id.into(),
            client_timezone: "UTC".into(),
            conversation_id: Some("intent".into()),
            ..Default::default()
        },
        device_id: String::new(),
        targets: Vec::new(),
        reads: ReadContextSelection {
            external_issues: Vec::new(),
            external_issue_sources: Vec::new(),
            tool_names: Vec::new(),
            expires_at: None,
            object_attachments: Vec::new(),
            live_targets: Vec::new(),
        },
        destination: destination("model"),
    }
}

#[tokio::test]
async fn queued_issue_access_keeps_its_original_native_target_after_activation() {
    use desk_agent_protocol::external_tools::IssueSelection;
    use desk_diagnose_core::external_tools::{IssueAccess, IssueTarget};
    let TestStore { db } = setup().await;
    let mut original = input("issue-message", "Investigate the selected issue");
    original.ask.selected_issues.push(IssueSelection {
        connection_ref: "forge".into(),
        owner: "org".into(),
        repository: "project".into(),
        issue_number: 4,
    });
    original.reads.external_issues.push(IssueAccess {
        connection_ref: "forge".into(),
        target: IssueTarget {
            platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
            instance: "https://forge.test/".into(),
            account_id: 3,
            repository_id: 42,
            owner: "org".into(),
            repository: "project".into(),
            issue_number: 4,
            issue_id: 82,
            private: true,
            internal: false,
        },
    });
    original.reads.external_issue_sources.push(
        desk_diagnose_core::issue_authority::IssueSourceRef {
            authorization_id: desk_diagnose_core::native_ids::stable(
                desk_diagnose_core::native_ids::Kind::IssueAdmission,
                &"accepted-source",
            ),
            subscription_revision: 3,
            tool_ref: original.reads.external_issues[0].tool_ref(),
        },
    );
    let txn = db.begin().await.unwrap();
    let queued = queue::enqueue(&txn, "owner", "root", &original, None, 32, 100)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    original.reads.external_issues[0].target.issue_id = 999;
    let restored = queue::first_pending(&db, "owner", "root")
        .await
        .unwrap()
        .unwrap();
    let frozen = FrozenOwnerInput::decode(&restored).unwrap();
    assert_eq!(frozen.reads.external_issues[0].target.issue_id, 82);
    assert!(frozen.reads.external_issues[0].matches_selection(&frozen.ask.selected_issues[0]));
    let txn = db.begin().await.unwrap();
    assert!(queue::activate(&txn, &queued, 1, 101).await.unwrap());
    txn.commit().await.unwrap();
    let activated = queue::load(&db, "owner", "root", &queued.request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(FrozenOwnerInput::decode(&activated).unwrap(), frozen);
    let activation = queue::OwnerQueueTurn::Activate {
        request_id: queued.request_id.clone(),
        conversation_id: "root".into(),
        execution_conversation_id: "root".into(),
        destination: frozen.destination.clone(),
    };
    let selected =
        queue::frozen_read_selection(&db, Some(&activation), "owner", "root", "", &original.ask)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(selected, frozen.reads);
    let mut changed = original.ask.clone();
    changed.selected_issues[0].issue_number = 5;
    assert!(
        queue::frozen_read_selection(&db, Some(&activation), "owner", "root", "", &changed)
            .await
            .is_err()
    );
    let retry = queue::OwnerQueueTurn::Submit {
        ask: original.ask.clone(),
        reply_connection_id: "browser".into(),
    };
    assert_eq!(
        queue::frozen_read_selection(&db, Some(&retry), "owner", "root", "", &original.ask)
            .await
            .unwrap(),
        Some(selected)
    );
    queue::retire_superseded(&db, &activated, 102)
        .await
        .unwrap();
    assert_eq!(
        queue::frozen_read_selection(&db, Some(&retry), "owner", "root", "", &original.ask)
            .await
            .unwrap(),
        Some(ReadContextSelection::default())
    );
}

#[test]
fn native_subagent_wait_and_ready_resume_retain_the_original_slot() {
    use desk_diagnose_core::subagent::wait::{
        ParentWait, ReportCursor, ReportWait, TaskFence, WaitMode,
    };
    let mut session = desk_diagnose_core::session::PersistedAgentSession::new(
        "root",
        "owner",
        "",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-08-25T00:00:00Z",
    );
    session.input_revision = 1;
    assert!(!queue::holds_input_slot(&session));
    session.subagent_wait = Some(ParentWait {
        wait_id: "wait".into(),
        tool_call_id: "call".into(),
        result_message_id: "result".into(),
        group_id: "group".into(),
        source_epoch: 1,
        retry_after_ms: None,
        parent_input_revision: 1,
        parent_control_revision: session.control_revision,
        report_wait: ReportWait {
            requests: vec![ReportCursor {
                request_id: "child".into(),
                after_seq: 0,
            }],
            kinds: vec![desk_diagnose_core::subagent::report::ReportKind::Completed],
            mode: WaitMode::AllTerminal,
        },
        tasks: vec![TaskFence {
            task_id: "child".into(),
            input_revision: 1,
            control_revision: 1,
        }],
    });
    assert!(queue::holds_input_slot(&session));
    session.ready_subagent_wait = session.subagent_wait.take();
    assert!(queue::holds_input_slot(&session));
    session.ready_subagent_wait = None;
    assert!(!queue::holds_input_slot(&session));
}

#[tokio::test]
async fn coordination_preserves_native_fences_and_defers_omissions_independently() {
    use desk_diagnose_core::{
        chat::{ModelTurn, StopReason},
        session::{AgentSessionSurface, PersistedAgentSession},
    };
    let TestStore { db } = setup().await;
    let now = chrono::Utc::now().timestamp_millis();
    let scope = desk_agent_protocol::AgentScope {
        granted: vec![],
        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let mut session = PersistedAgentSession::new("root", "owner", "", 1, scope.clone(), "now");
    session.surface = AgentSessionSurface::AiAssistant;
    session.input_revision = 4;
    session.focus_epoch.input_revision = 4;
    let txn = db.begin().await.unwrap();
    let b = queue::enqueue(
        &txn,
        "owner",
        "root",
        &input("b", "Request B"),
        None,
        4,
        now,
    )
    .await
    .unwrap();
    let c = queue::enqueue(
        &txn,
        "owner",
        "root",
        &input("c", "Request C"),
        None,
        4,
        now,
    )
    .await
    .unwrap();
    let mut other = input("d", "Request D");
    other.destination = destination("different-model");
    let d = queue::enqueue(&txn, "owner", "root", &other, None, 4, now)
        .await
        .unwrap();
    let (batch, projection) = queue::coordination::reserve(&txn, &mut session, &b, now, Vec::new())
        .await
        .unwrap()
        .unwrap();
    assert!(batch.is_current(&session, now));
    assert_eq!(session.input_revision, 4);
    assert_eq!(session.focus_epoch.input_revision, 4);
    assert_eq!(projection.messages[1].text, "Request B");
    assert_eq!(projection.messages[2].text, "Request C");
    assert_eq!(projection.messages.len(), 3);
    assert!(projection.context_conversation_id.is_none());
    assert!(projection.tools.is_empty());
    let mut rival = session.clone();
    let instant = |ms| {
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .to_rfc3339()
    };
    assert!(
        rival
            .begin_turn("too-soon", None, None, 1, scope.clone(), instant(now))
            .is_err()
    );
    rival
        .begin_turn("takeover", None, None, 1, scope, instant(now + 61_000))
        .unwrap();
    assert!(!batch.is_current(&rival, now + 61_000));
    assert!(
        queue::coordination::reserve(&txn, &mut session, &b, now + 1, Vec::new())
            .await
            .unwrap()
            .is_none()
    );

    let turn = ModelTurn {
        text: serde_json::json!({"decisions":[
            {"request_id": b.request_id, "route": "native"},
            {"request_id": d.request_id, "route": "native"},
            {"request_id": c.request_id, "route": "invalid"},
        ]})
        .to_string(),
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: Default::default(),
        provider_meta: Default::default(),
    };
    queue::coordination::settle(&txn, &batch, Some(&turn), now + 1, &Default::default())
        .await
        .unwrap();
    let b = queue::load(&txn, "owner", "root", &b.request_id)
        .await
        .unwrap()
        .unwrap();
    let c = queue::load(&txn, "owner", "root", &c.request_id)
        .await
        .unwrap()
        .unwrap();
    let d = queue::load(&txn, "owner", "root", &d.request_id)
        .await
        .unwrap()
        .unwrap();
    assert!(b.coordinated);
    assert_eq!(c.retry_at_ms, Some(now + 5_001));
    assert!(!d.coordinated);
    assert!(queue::activate(&txn, &b, 5, now + 2).await.unwrap());
    txn.commit().await.unwrap();
    assert_eq!(
        queue::recovery_heads(&db, 0, 10).await.unwrap()[0].request_id,
        b.request_id
    );
    assert_eq!(
        queue::first_pending(&db, "owner", "root")
            .await
            .unwrap()
            .unwrap()
            .request_id,
        d.request_id
    );
    let activated = queue::load(&db, "owner", "root", &b.request_id)
        .await
        .unwrap()
        .unwrap();
    let txn = db.begin().await.unwrap();
    session.main_coordination = None;
    session.input_revision = 5;
    session.latest_input_seq = 5;
    session.handled_input_seq = 4;
    session
        .conversation
        .push(desk_diagnose_core::chat::ChatMessage::text(
            "b",
            desk_diagnose_core::chat::ChatRole::User,
            "Request B",
        ));
    assert!(
        queue::fail_unclaimed(
            &txn,
            &activated,
            &mut session,
            chrono::DateTime::from_timestamp_millis(now + 3).unwrap(),
            MainRequestFailure::SourceUnavailable
        )
        .await
        .unwrap()
    );
    txn.commit().await.unwrap();
    assert_eq!(session.handled_input_seq, 5);
    assert!(session.terminal_error.is_some());
    assert_eq!(
        queue::recovery_heads(&db, 0, 10).await.unwrap()[0].request_id,
        d.request_id
    );
    assert!(
        queue::receipts(&db, "owner", "root", None)
            .await
            .unwrap()
            .iter()
            .any(|receipt| receipt.request_id == b.request_id
                && receipt.state == MainRequestState::Failed
                && receipt.failure == Some(MainRequestFailure::SourceUnavailable))
    );
}

#[tokio::test]
async fn coordination_attempts_survive_a_lost_lease() {
    use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
    let TestStore { db } = setup().await;
    let now = chrono::Utc::now().timestamp_millis();
    let mut session = PersistedAgentSession::new(
        "root",
        "owner",
        "",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "now",
    );
    session.surface = AgentSessionSurface::AiAssistant;
    let txn = db.begin().await.unwrap();
    let mut head = queue::enqueue(
        &txn,
        "owner",
        "root",
        &input("b", "Request B"),
        None,
        2,
        now,
    )
    .await
    .unwrap();
    queue::enqueue(
        &txn,
        "owner",
        "root",
        &input("c", "Request C"),
        None,
        2,
        now,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    for attempt in 1..=3 {
        let txn = db.begin().await.unwrap();
        let time = now + i64::from(attempt) * 61_000;
        queue::coordination::reserve(&txn, &mut session, &head, time, Vec::new())
            .await
            .unwrap()
            .unwrap();
        txn.commit().await.unwrap();
        head = queue::load(&db, "owner", "root", &head.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(head.coordination_attempts, attempt);
    }
    assert!(queue::coordination::exhausted(&head));
    let txn = db.begin().await.unwrap();
    assert!(
        queue::coordination::reserve(&txn, &mut session, &head, now + 244_000, Vec::new())
            .await
            .unwrap()
            .is_none()
    );
    queue::fail_unactivated(
        &txn,
        &head,
        now + 244_000,
        MainRequestFailure::CoordinationExhausted,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let receipt = queue::receipts(&db, "owner", "root", Some(&head.request_id))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(receipt.state, MainRequestState::Failed);
    assert_eq!(
        receipt.failure,
        Some(MainRequestFailure::CoordinationExhausted)
    );
}

#[tokio::test]
async fn duplicate_intake_keeps_the_first_frozen_input_and_fifo_order() {
    let TestStore { db } = setup().await;
    let txn = db.begin().await.unwrap();
    let b = input("message-b", "Request B");
    let c = input("message-c", "Request C");
    let first = queue::enqueue(&txn, "owner", "root", &b, Some("first-connection"), 2, 100)
        .await
        .unwrap();
    assert_eq!(
        first.reply_connection_id.as_deref(),
        Some("first-connection")
    );
    let second = queue::enqueue(&txn, "owner", "root", &c, None, 2, 100)
        .await
        .unwrap();
    let retry = FrozenOwnerInput {
        destination: destination("new-default-model"),
        ..b.clone()
    };
    assert_eq!(
        queue::enqueue(
            &txn,
            "owner",
            "root",
            &retry,
            Some("retry-connection"),
            2,
            101
        )
        .await
        .unwrap(),
        first
    );
    assert!(matches!(
        queue::enqueue(
            &txn,
            "owner",
            "root",
            &input("message-b", "Changed"),
            None,
            2,
            101
        )
        .await,
        Err(AssistantStoreError::Invalid(_))
    ));
    assert!(matches!(
        queue::enqueue(
            &txn,
            "owner",
            "root",
            &input("message-d", "Too many"),
            None,
            2,
            101
        )
        .await,
        Err(AssistantStoreError::Capacity)
    ));
    txn.commit().await.unwrap();

    let txn = db.begin().await.unwrap();
    let claimed = queue::first_pending(&txn, "owner", "root")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed, first);
    assert_eq!(FrozenOwnerInput::decode(&claimed).unwrap(), b);
    assert!(
        queue::load(&txn, "other-owner", "root", &first.request_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(queue::activate(&txn, &first, 5, 102).await.unwrap());
    txn.rollback().await.unwrap();

    let txn = db.begin().await.unwrap();
    assert!(queue::activate(&txn, &first, 5, 103).await.unwrap());
    assert!(matches!(
        queue::cancel_owner_request(&txn, "owner", "root", &first.request_id, 103).await,
        Err(AssistantStoreError::Stale {
            current_revision: 5
        })
    ));
    let heads = queue::recovery_heads(&txn, 0, 10).await.unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].request_id, first.request_id);
    assert_eq!(FrozenOwnerInput::decode(&heads[0]).unwrap(), b);
    let mut session = desk_diagnose_core::session::PersistedAgentSession::new(
        "root",
        "owner",
        "",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-08-25T00:00:00Z",
    );
    session.input_revision = 5;
    assert!(
        !queue::mark_claimed(&txn, &first.request_id, &session, "wrong-message", 104)
            .await
            .unwrap()
    );
    assert!(
        queue::mark_claimed(
            &txn,
            &first.request_id,
            &session,
            &first.client_message_id,
            104
        )
        .await
        .unwrap()
    );
    assert_eq!(
        queue::recovery_heads(&txn, 0, 10).await.unwrap(),
        vec![second.clone()]
    );
    let claimed = queue::first_pending(&txn, "owner", "root")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed, second);
    assert_eq!(FrozenOwnerInput::decode(&claimed).unwrap(), c);
    let receipts = queue::receipts(&txn, "owner", "root", None).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].request_id, second.request_id);
    assert_eq!(receipts[0].state, MainRequestState::Queued);
    assert!(matches!(
        queue::cancel_owner_request(&txn, "other-owner", "root", &second.request_id, 104).await,
        Err(AssistantStoreError::Inaccessible)
    ));
    let cancelled = queue::cancel_owner_request(&txn, "owner", "root", &second.request_id, 104)
        .await
        .unwrap();
    assert_eq!(cancelled.state, MainRequestState::Cancelled);
    assert_eq!(
        queue::cancel_owner_request(&txn, "owner", "root", &second.request_id, 105)
            .await
            .unwrap(),
        cancelled
    );
    assert!(
        queue::receipts(&txn, "owner", "root", None)
            .await
            .unwrap()
            .is_empty()
    );
    txn.commit().await.unwrap();
    for (row, revision) in [(first, Some(5)), (second, None)] {
        let stored = queue::load(&db, "owner", "root", &row.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.input_revision, revision);
        assert!(stored.pending_input_json.is_none());
        assert!(stored.reply_connection_id.is_none());
        assert!(FrozenOwnerInput::decode(&stored).is_err());
    }
    assert!(
        queue::first_pending(&db, "owner", "root")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn workbench_dispatch_proposals_are_scoped_and_activation_is_atomic() {
    use desk_diagnose_core::{
        chat::{ModelTurn, StopReason},
        model_egress::ModelEgressPolicy,
        native_ids::{self, Kind},
        session::{AgentSessionSurface, PersistedAgentSession},
    };
    let TestStore { db } = setup().await;
    let now = chrono::Utc::now();
    let ms = now.timestamp_millis();
    let mut root = PersistedAgentSession::new_workbench_coordinator(
        "owner",
        "root",
        None,
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        &now.to_rfc3339(),
    )
    .unwrap();
    root.surface = AgentSessionSurface::AiAssistant;
    let txn = db.begin().await.unwrap();
    let mut original = input("b", "Work B");
    original.ask.target_refs.push("selected-device".into());
    original
        .targets
        .push(desk_diagnose_core::dynamic_run::AssistantInputTarget {
            reference: "selected-device".into(),
            device_id: "9".into(),
            member_generation: Some(1),
        });
    let b = queue::enqueue(&txn, "owner", "root", &original, None, 4, ms)
        .await
        .unwrap();
    let c = queue::enqueue(&txn, "owner", "root", &input("c", "Work C"), None, 4, ms)
        .await
        .unwrap();
    let (batch, request) = queue::coordination::reserve(&txn, &mut root, &b, ms, Vec::new())
        .await
        .unwrap()
        .unwrap();
    assert!(!request.messages[0].text.contains("selected-device"));
    let scope = request
        .messages
        .iter()
        .find(|message| message.text.contains("selected-device"))
        .unwrap();
    assert_eq!(
        scope
            .data_envelope
            .as_ref()
            .unwrap()
            .provenance
            .source_envelope_ids,
        ["user-message-b"]
    );
    let policy = ModelEgressPolicy {
        destination: batch.destination.clone(),
        selected_source_tools: Default::default(),
        export_authorization_id: "test-dispatch-output".into(),
        now_unix_ms: ms as u64,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let accepted = policy.authorize_request(request).unwrap();
    let mut turn = ModelTurn {
        text: serde_json::json!({"decisions":[
            {"request_id":b.request_id,"route":"worker","dispatch":{}},
            {"request_id":c.request_id,"route":"worker","dispatch":{"target_refs":["other-device"]}},
        ]}).to_string(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    turn.provider_meta.data_envelope = Some(
        policy
            .derive_model_output_envelope(&turn, &accepted.input_envelopes)
            .unwrap(),
    );
    queue::coordination::settle(&txn, &batch, Some(&turn), ms + 1, &Default::default())
        .await
        .unwrap();
    txn.commit().await.unwrap();

    let b = queue::load(&db, "owner", "root", &b.request_id)
        .await
        .unwrap()
        .unwrap();
    let c = queue::load(&db, "owner", "root", &c.request_id)
        .await
        .unwrap()
        .unwrap();
    assert!(queue::coordination::has_route(&b));
    assert!(!b.coordinated && b.first_worker_request_id.is_none());
    assert!(!queue::coordination::has_route(&c));
    assert!(c.retry_at_ms.is_some());

    let worker = native_ids::stable(Kind::Task, &b.request_id);
    let txn = db.begin().await.unwrap();
    assert!(
        queue::activate_with_dispatch(&txn, &b, 1, ms + 2, Some(&worker))
            .await
            .unwrap()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        queue::load(&db, "owner", "root", &b.request_id)
            .await
            .unwrap(),
        Some(b.clone())
    );

    let txn = db.begin().await.unwrap();
    assert!(
        queue::activate_with_dispatch(&txn, &b, 1, ms + 3, Some(&worker))
            .await
            .unwrap()
    );
    assert!(
        !queue::activate_with_dispatch(&txn, &b, 1, ms + 4, Some(&worker))
            .await
            .unwrap()
    );
    txn.commit().await.unwrap();
    let saved = queue::load(&db, "owner", "root", &b.request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.first_worker_request_id.as_deref(),
        Some(worker.as_str())
    );
    assert!(
        saved.coordinated
            && saved.pending_input_json.is_none()
            && saved.first_dispatch_json.is_none()
    );
}
