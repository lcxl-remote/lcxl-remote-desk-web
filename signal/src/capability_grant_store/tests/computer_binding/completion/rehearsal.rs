use super::*;
use crate::{
    entity::{agent_schedule, agent_task_rehearsal},
    schedule_store::ScheduleStore,
};
use desk_agent_protocol::schedule::{
    ScheduleCreationSource, ScheduleDraft, ScheduleRule, ScheduleSpec, ScheduledTaskKind,
    management::ScheduleManagementResponse,
};

#[tokio::test]
async fn rehearsal_action_report_joins_original_dispatch_and_frozen_owner_session() {
    report_fixture(false).await;
}

#[tokio::test]
async fn rehearsal_native_read_is_classified_once_with_original_receipt() {
    report_fixture(true).await;
}

async fn report_fixture(read: bool) {
    let dir = tempfile::tempdir().unwrap();
    let page: desk_agent_protocol::browser_control::BrowserPageRef = serde_json::from_value(serde_json::json!({
        "schema_version":1,"adapter":{"engine":"chrome_extension","device_id":"device-1",
        "os_session_id":"desktop-1","browser_major_version":145,"browser_version":"145","adapter_id":"fixture","adapter_version":"1",
        "profile_incarnation":"profile-1","connection_revision":1},
        "page_id":"page-1","page_incarnation":"page-first","origin":{"kind":"https","host_ascii":"example.test","port":443},
        "document_revision":1,"url_sha256":"a".repeat(64),"observed_at_unix_ms":1000
    })).unwrap();
    let custom = read.then(|| ToolCall {
        id: "model-call-original".into(),
        name: "browser_wait_for".into(),
        arguments_json: serde_json::json!({"page":page,"element":{"page_id":"page-1","page_incarnation":"page-first","document_revision":1,"element_id":"ready","role":"button","accessible_name":"Ready","value":null,"element_revision":1},"state":"absent","timeout_ms":1000}).to_string(),
    });
    let f = Fixture::new_with_call(
        file_db(&dir.path().join("rehearsal.db")).await,
        "1",
        false,
        false,
        false,
        custom,
    )
    .await;
    f.bind().await;
    let mut native = verified(&f.plan);
    if read {
        use desk_agent_protocol::browser_control::{BrowserActionOutcome, BrowserSemanticSnapshot};
        let Some(ComputerActionOutput::Browser(result)) = &mut native.output else {
            panic!("browser output")
        };
        result.outcome = BrowserActionOutcome::WaitSatisfied;
        result.snapshot = Some(BrowserSemanticSnapshot {
            schema_version: 1,
            page: page.clone(),
            elements: vec![],
            truncated: false,
            captured_at_unix_ms: 1000,
        });
        native.facts[0].changed = false;
    }
    observe(&f, &native).await.unwrap();
    let db = &f.store.db;
    let schema = Schema::new(db.get_database_backend());
    for table in [
        schema.create_table_from_entity(agent_schedule::Entity),
        schema.create_table_from_entity(agent_task_rehearsal::Entity),
    ] {
        db.execute(&table).await.unwrap();
    }
    let store = ScheduleStore::new(db.clone());
    let draft = ScheduleDraft {
        time_confirmation: None,
        client_create_key: "observation".into(),
        kind: ScheduledTaskKind::FreshTask,
        target_device_id: f.session.device_id.clone(),
        title: "Open page".into(),
        prompt: "Open approved page".into(),
        locale: None,
        model_id: None,
        spec: ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::Daily {
                utc_time: "06:00:00".into(),
            },
        },
        source_conversation_id: None,
        requirement_revision: None,
        creation_source: ScheduleCreationSource::Manual,
    };
    let task = store.create_draft(1, &draft, 1000).await.unwrap();
    let mut session = f.session.clone();
    {
        let original = f
            .store
            .read_computer_result(
                &f.plan.execution_generation,
                &session.conversation_id,
                &session.actor_id,
                &session.device_id,
            )
            .await
            .unwrap()
            .unwrap();
        let mut result =
            ChatMessage::tool_result("native-result", &f.call.id, original.output.content)
                .with_turn_id("turn-1");
        result.data_envelope = Some(original.receipt.envelope);
        session.conversation.push(result);
    }
    session.client_conversation_id = Some("rehearsal-client".into());
    let row = agent_session::Entity::find()
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let state = session.encode_json_for_storage().unwrap();
    let mut frozen: agent_session::ActiveModel = row.into();
    frozen.state_json = Set(state.clone());
    frozen.update(db).await.unwrap();
    let original = work(&f).await;
    let finished = Utc::now().timestamp_millis();
    let rehearsal = agent_task_rehearsal::ActiveModel {
        rehearsal_id: Set("rehearsal-observation".into()),
        schedule_id: Set(task.schedule_id.clone()),
        owner_user_id: Set(1),
        task_revision: Set(task.task_revision),
        target_device_id: Set(task.target_device_id.clone()),
        prompt: Set(task.prompt.clone()),
        prompt_sha256: Set(format!("{:x}", Sha256::digest(task.prompt.as_bytes()))),
        locale: Set(None),
        model_id: Set(None),
        client_conversation_id: Set("rehearsal-client".into()),
        conversation_id: Set(session.conversation_id.clone()),
        status: Set("completed".into()),
        creation_identity: Set("observation-creation".into()),
        creation_payload_sha256: Set("a".repeat(64)),
        started_at: Set(Some(original.created_at.timestamp_millis())),
        finished_at: Set(Some(finished)),
        completed_session_version: Set(Some(session.version)),
        completed_session_sha256: Set(Some(format!("{:x}", Sha256::digest(state.as_bytes())))),
        answer_message_id: Set(Some("answer".into())),
        created_at: Set(original.created_at.timestamp_millis()),
        updated_at: Set(finished),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    let txn = db.begin().await.unwrap();
    let report = ScheduleStore::read_rehearsal_actions_on(&txn, 1, &rehearsal.rehearsal_id)
        .await
        .unwrap();
    assert_eq!(report.actions.len(), usize::from(!read));
    if read {
        let reads = ScheduleStore::read_rehearsal_reads_on(&txn, 1, &rehearsal.rehearsal_id)
            .await
            .unwrap();
        assert_eq!(reads.reads.len(), 1);
        assert_eq!(reads.reads[0].tool_call_id, f.call.id);
        assert!(reads.unconfirmed_read_call_ids.is_empty());
        let sources =
            ScheduleStore::read_rehearsal_read_sources_on(&txn, 1, &rehearsal.rehearsal_id)
                .await
                .unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].lineage.source_tool_name, f.call.name);
    } else {
        assert_eq!(report.actions[0].origin.tool_call_id, f.call.id);
        use desk_diagnose_core::schedule::rehearsal::read_sources::verified_action_source;
        let action = &report.actions[0];
        let proof = verified_action_source(
            &session.conversation,
            &action.authority,
            &action.origin,
            &action.receipt,
        )
        .unwrap();
        assert_eq!(
            proof.lineage.envelope_id,
            action.receipt.envelope.envelope_id
        );
        // Completion after promotion is an untrusted follow-up beside the original placeholder.
        let mut background = session.conversation.clone();
        let output = background.pop().unwrap();
        background.push(ChatMessage::background_task_running(
            "running-placeholder",
            &f.call.id,
            &action.receipt.action.action_request_id,
        ));
        let proposal = background
            .iter()
            .find(|message| message.role == desk_diagnose_core::chat::ChatRole::Assistant)
            .unwrap()
            .clone();
        let running = background.last_mut().unwrap();
        running.data_envelope =
            desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                proposal.data_envelope.as_ref(),
                &f.call.id,
                &running.text,
                "provider_execution_status",
            )
            .unwrap();
        let mut completed = ChatMessage::untrusted_output(
            &output.message_id,
            &f.call.id,
            &action.receipt.action.action_request_id,
            &output.text,
        );
        completed.data_envelope = output.data_envelope.clone();
        background.push(completed);
        let projected = verified_action_source(
            &background,
            &action.authority,
            &action.origin,
            &action.receipt,
        )
        .unwrap();
        assert_eq!(projected.lineage, proof.lineage);
        use desk_diagnose_core::schedule::rehearsal::read_sources::verified_action_status_sources;
        let statuses = verified_action_status_sources(
            &background,
            &action.authority,
            &action.origin,
            &action.receipt,
        )
        .unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(
            statuses[0].lineage.source_envelope_ids,
            vec![proposal.data_envelope.as_ref().unwrap().envelope_id.clone()]
        );
        let mut changed_status = background.clone();
        let status = changed_status
            .iter_mut()
            .find(|message| message.message_id == "running-placeholder")
            .unwrap();
        status.text.push_str(" instruction");
        assert!(
            verified_action_status_sources(
                &changed_status,
                &action.authority,
                &action.origin,
                &action.receipt
            )
            .is_err()
        );
        let mut changed_status = background.clone();
        let status = changed_status
            .iter_mut()
            .find(|message| message.message_id == "running-placeholder")
            .unwrap();
        status
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_envelope_ids
            .clear();
        assert!(
            verified_action_status_sources(
                &changed_status,
                &action.authority,
                &action.origin,
                &action.receipt
            )
            .is_err()
        );
        let mut wrong_task = background.clone();
        wrong_task.last_mut().unwrap().background_task_id = Some("another-task".into());
        assert!(
            verified_action_source(
                &wrong_task,
                &action.authority,
                &action.origin,
                &action.receipt
            )
            .is_err()
        );
        let mut duplicate = background.clone();
        duplicate.push(background.last().unwrap().clone());
        assert!(
            verified_action_source(
                &duplicate,
                &action.authority,
                &action.origin,
                &action.receipt
            )
            .is_err()
        );
        background.last_mut().unwrap().role = desk_diagnose_core::chat::ChatRole::User;
        assert!(
            verified_action_source(
                &background,
                &action.authority,
                &action.origin,
                &action.receipt
            )
            .is_err()
        );
        let mut changed = session.conversation.clone();
        changed
            .last_mut()
            .unwrap()
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_envelope_ids
            .clear();
        assert!(
            verified_action_source(&changed, &action.authority, &action.origin, &action.receipt)
                .is_err()
        );
        let mut changed = session.conversation.clone();
        changed.last_mut().unwrap().text.push_str(" forged");
        assert!(
            verified_action_source(&changed, &action.authority, &action.origin, &action.receipt)
                .is_err()
        );
    }
    assert!(report.unconfirmed_tool_call_ids.is_empty());
    assert_eq!(report.other_tool_call_ids.len(), usize::from(read));
    assert!(
        ScheduleStore::read_rehearsal_actions_on(&txn, 2, &rehearsal.rehearsal_id)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    let response =
        crate::schedule_management::rehearsal_permissions::read(db, 1, &rehearsal.rehearsal_id)
            .await
            .unwrap();
    let ScheduleManagementResponse::RehearsalPermissions {
        observations,
        unconfirmed_tool_call_ids,
        unclassified_tool_call_ids,
        ..
    } = response
    else {
        panic!("expected permission history")
    };
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].tool_call_id, f.call.id);
    assert!(unconfirmed_tool_call_ids.is_empty());
    assert!(unclassified_tool_call_ids.is_empty());
    let txn = db.begin().await.unwrap();
    let row = agent_session::Entity::find()
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let mut changed: agent_session::ActiveModel = row.into();
    changed.state_json = Set(format!("{state} "));
    changed.update(&txn).await.unwrap();
    assert!(
        ScheduleStore::read_rehearsal_actions_on(&txn, 1, &rehearsal.rehearsal_id)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
}
