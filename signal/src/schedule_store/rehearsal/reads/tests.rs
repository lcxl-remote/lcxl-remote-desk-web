use super::*;
use crate::entity::agent_exec_task;
use desk_agent_protocol::{
    AgentScope, Capability, ExecutionMode,
    capability_grant::{
        CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant, CapabilityGrantUsePolicy,
    },
    capability_provider::ProductSurface,
    data_lineage::DestinationIdentity,
};
use desk_diagnose_core::{
    chat::{ChatMessage, ToolCall},
    input_read_context::{ReadContextSelection, object_read::ObjectReadBinding},
    provider_preflight::{ProviderCallSubject, read::ReadCallPreflight},
    session::{AgentSessionSurface, TurnState},
};
use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema};

async fn fixture(success: bool) -> (ScheduleStore, rehearsal::Model) {
    fixture_with_text(success, None, "Synthetic tool result").await
}

async fn fixture_with_text(
    success: bool,
    prompt: Option<&str>,
    content: &str,
) -> (ScheduleStore, rehearsal::Model) {
    let store = super::super::super::tests::store().await;
    let schema = Schema::new(store.db.get_database_backend());
    for table in [
        schema.create_table_from_entity(rehearsal::Entity),
        schema.create_table_from_entity(agent_session::Entity),
        schema.create_table_from_entity(work_row::Entity),
        schema.create_table_from_entity(agent_exec_task::Entity),
        schema.create_table_from_entity(grant_row::Entity),
        schema.create_table_from_entity(reservation_row::Entity),
        schema.create_table_from_entity(outbox_row::Entity),
    ] {
        store.db.execute(&table).await.unwrap();
    }
    let mut draft = super::super::super::tests::draft();
    if let Some(prompt) = prompt {
        draft.prompt = prompt.into();
    }
    let task = store.create_draft(1, &draft, 1000).await.unwrap();
    let reserved = store
        .reserve_rehearsal(1, &task.schedule_id, task.revision, "read-report")
        .await
        .unwrap();
    let started = store
        .claim_rehearsal(1, &reserved.rehearsal_id)
        .await
        .unwrap();
    let scope = AgentScope {
        granted: vec![Capability::SystemInfo],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let now = chrono::Utc::now();
    let mut session = PersistedAgentSession::new(
        &started.conversation_id,
        "1",
        &started.target_device_id,
        1,
        scope.clone(),
        now.to_rfc3339(),
    );
    session.surface = AgentSessionSurface::DeviceAssistant;
    session.client_conversation_id = Some(started.client_conversation_id.clone());
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    session.input_revision = 1;
    session.latest_input_seq = 1;
    session.handled_input_seq = 1;
    session
        .begin_turn(
            "turn-1",
            Some("request-1".into()),
            None,
            1,
            scope,
            now.to_rfc3339(),
        )
        .unwrap();
    session.conversation.push(
        ChatMessage::text(
            format!("rehearsal:{}:input", started.rehearsal_id),
            ChatRole::User,
            &started.prompt,
        )
        .with_turn_id("turn-1"),
    );
    let call = ToolCall {
        id: "model-read-1".into(),
        name: "read_system_info".into(),
        arguments_json: "{}".into(),
    };
    session.conversation.push(
        ChatMessage::assistant_tool_calls("proposal", "", vec![call.to_ref()])
            .with_turn_id("turn-1"),
    );
    let row = agent_session::ActiveModel {
        conversation_id: Set(started.conversation_id.clone()),
        actor_id: Set("1".into()),
        device_id: Set(started.target_device_id.clone()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(session.version),
        lease_token: Set(session.lease_token as i64),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    let original = ReadContextSelection {
        tool_names: vec![call.name.clone()],
        expires_at: Some((now + chrono::Duration::minutes(2)).to_rfc3339()),
        object_attachments: vec![],
        live_targets: vec![],
    };
    let destination = DestinationIdentity::Model {
        connection_id: "test-gateway".into(),
        connection_revision: 1,
        profile_revision: 1,
        model_id: "test-model".into(),
    };
    let binding = ObjectReadBinding {
        original: &original,
        destination: &destination,
        now_unix_ms: now.timestamp_millis() as u64,
    };
    let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
    let preflight =
        ReadCallPreflight::build(&registry, ProductSurface::OssPersonalOwner, &call, &binding)
            .unwrap();
    let subject = ProviderCallSubject {
        actor_id: "1",
        run_id: &started.conversation_id,
        input_revision: 1,
        target_device_id: &started.target_device_id,
        policy_revision: 1,
        readiness_revision: 1,
        now_unix_ms: now.timestamp_millis() as u64,
    };
    let authority = preflight.grant_call(&subject).unwrap();
    let mut limits = preflight.output_limits();
    limits.max_calls = 1;
    let grant = CapabilityGrant {
        schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
        grant_id: "read-grant".into(),
        actor_id: "1".into(),
        run_id: started.conversation_id.clone(),
        input_revision: 1,
        surface: ProductSurface::OssPersonalOwner,
        target_device_id: started.target_device_id.clone(),
        target_session_id: None,
        provider_id: authority.provider_id.into(),
        capability_id: authority.capability_id.into(),
        tool_name: call.name.clone(),
        tool_schema_version: authority.tool_schema_version,
        effect: authority.effect,
        risk_tier: authority.risk_tier,
        resource_scope: authority.resource_scope.to_vec(),
        operation_scope: authority.operation_scope.to_vec(),
        export_destinations: authority.export_destinations.to_vec(),
        allowed_envelope_ids: vec![],
        allowed_content_digests_sha256: vec![],
        use_policy: CapabilityGrantUsePolicy::OneShotExact,
        canonical_input_digest_sha256: Some(authority.canonical_input_digest_sha256.into()),
        issued_by: CapabilityGrantIssuer::PolicyAuto,
        issued_at_unix_ms: subject.now_unix_ms,
        expires_at_unix_ms: preflight.valid_until_unix_ms(),
        remaining_uses: 1,
        limits,
        policy_revision: 1,
        readiness_revision: 1,
        revoked_at_unix_ms: None,
        revoked_reason: None,
    };
    let grants = SignalCapabilityGrantStore::new(store.db.clone());
    grants.issue(&grant).await.unwrap();
    let server_call_id = format!(
        "capability-call-{}",
        digest(&format!("{}:turn-1:{}", started.conversation_id, call.id))
    );
    let request = || PrepareCapabilityCall {
        grant_id: &grant.grant_id,
        call_id: &server_call_id,
        turn_id: "turn-1",
        input_revision: 1,
        input_watermark: 1,
        generation: 1,
        canonical_input_json: "{}",
        call: authority.clone(),
    };
    grants.prepare(request()).await.unwrap();
    let DispatchIntentResult::Recorded { dispatch_id, .. } =
        grants.record_dispatch_intent(request()).await.unwrap()
    else {
        panic!("expected intent")
    };
    assert!(matches!(
        grants
            .claim_dispatch(&dispatch_id, chrono::Utc::now().timestamp_millis() as u64)
            .await
            .unwrap(),
        DispatchClaimResult::Claimed(_)
    ));
    grants
        .record_dispatch_completion(
            &CapabilityDispatchCompletion {
                dispatch_id,
                call_id: server_call_id,
                generation: 1,
                outcome: if success {
                    CapabilityDispatchOutcome::Succeeded
                } else {
                    CapabilityDispatchOutcome::Failed
                },
                result_digest_sha256: digest(content),
            },
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    let mut result = ChatMessage::tool_result("result", &call.id, content).with_turn_id("turn-1");
    result.data_envelope = Some(
        desk_diagnose_core::model_message_labels::read_result_envelope(
            &registry,
            &call,
            &desk_diagnose_core::seam::ToolRunOutput {
                content: result.text.clone(),
                image_data_url: None,
            },
            desk_diagnose_core::model_message_labels::ReadResultLabel {
                envelope_id: "original-read-source".into(),
                observation_id: "original-observation".into(),
                source_object_id: Some("device:read".into()),
                observed_at_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
            },
        )
        .unwrap(),
    );
    session.conversation.push(result);
    session.conversation.push(
        ChatMessage::text("answer", ChatRole::Assistant, "Report complete").with_turn_id("turn-1"),
    );
    session.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    let mut updated: agent_session::ActiveModel = row.into();
    updated.state_json = Set(session.encode_json_for_storage().unwrap());
    updated.version = Set(session.version);
    updated.lease_deadline = Set(None);
    updated.update(&store.db).await.unwrap();
    let completed = store
        .finish_answered_rehearsal(1, &started.rehearsal_id, "Report complete")
        .await
        .unwrap();
    (store, completed)
}

#[tokio::test]
async fn rehearsal_read_report_observes_uncommitted_evidence_and_preserves_rollback() {
    let (store, rehearsal) = fixture(true).await;
    let original = store
        .read_rehearsal_reads(1, &rehearsal.rehearsal_id)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let txn = store.db.begin().await.unwrap();
        let report = ScheduleStore::read_rehearsal_reads_on(&txn, 1, &rehearsal.rehearsal_id)
            .await
            .unwrap();
        assert_eq!(report.reads.len(), 1);
        assert_eq!(report.session_sha256, original.session_sha256);
        let mut changed: rehearsal::ActiveModel = rehearsal.clone().into();
        changed.completed_session_sha256 = Set(Some("0".repeat(64)));
        changed.update(&txn).await.unwrap();
        assert!(
            ScheduleStore::read_rehearsal_reads_on(&txn, 1, &rehearsal.rehearsal_id)
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
    })
    .await
    .unwrap();
    let restored = store
        .read_rehearsal_reads(1, &rehearsal.rehearsal_id)
        .await
        .unwrap();
    assert_eq!(restored.session_sha256, original.session_sha256);
    assert_eq!(
        restored.reads[0].output_sha256,
        original.reads[0].output_sha256
    );
}

#[tokio::test]
async fn successful_read_report_requires_original_completed_dispatch() {
    let (store, rehearsal) = fixture(true).await;
    let report = store
        .read_rehearsal_reads(1, &rehearsal.rehearsal_id)
        .await
        .unwrap();
    assert_eq!(report.reads.len(), 1);
    let view = crate::schedule_management::rehearsal_permissions::read(
        &store.db,
        1,
        &rehearsal.rehearsal_id,
    )
    .await
    .unwrap();
    let value = serde_json::to_value(&view).unwrap();
    assert_eq!(value["result"], "rehearsal_permissions");
    assert_eq!(value["observations"].as_array().unwrap().len(), 1);
    assert_eq!(value["observations"][0]["approval_source"], "policy_auto");
    for field in [
        "grant_id",
        "target_session_id",
        "envelope_ids",
        "canonical_input_sha256",
        "output_sha256",
    ] {
        assert!(value["observations"][0].get(field).is_none(), "{field}");
    }
    assert!(
        value["unconfirmed_tool_call_ids"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        value["unclassified_tool_call_ids"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        crate::schedule_management::rehearsal_permissions::read(
            &store.db,
            2,
            &rehearsal.rehearsal_id
        )
        .await
        .is_err()
    );

    assert_eq!(report.reads[0].tool_call_id, "model-read-1");
    assert_eq!(report.reads[0].authority.tool_name, "read_system_info");
    assert_eq!(report.reads[0].issued_by, CapabilityGrantIssuer::PolicyAuto);
    assert_eq!(
        report.reads[0].output_sha256,
        digest("Synthetic tool result")
    );
    assert!(report.unconfirmed_read_call_ids.is_empty());
    assert!(report.other_tool_call_ids.is_empty());
    assert!(
        store
            .read_rehearsal_reads(2, &rehearsal.rehearsal_id)
            .await
            .is_err()
    );
    SignalCapabilityGrantStore::new(store.db.clone())
        .revoke(
            "read-grant",
            "1",
            &rehearsal.target_device_id,
            chrono::Utc::now().timestamp_millis() as u64,
            "owner revoked",
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(
            store
                .read_rehearsal_reads(1, &rehearsal.rehearsal_id)
                .await
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(report).unwrap()
    );
    let task = store.read(1, &rehearsal.schedule_id).await.unwrap();
    assert!(task.authorization_revision.is_none());
    assert!(task.next_run_at.is_none());
    let original = outbox_row::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    for case in 0..3 {
        let mut broken: outbox_row::ActiveModel = original.clone().into();
        match case {
            0 => broken.state = Set(DISPATCH_OUTBOX_PENDING.into()),
            1 => broken.call_id = Set("different-call".into()),
            _ => {
                let mut payload: CapabilityDispatchPayload =
                    serde_json::from_str(&original.payload_json).unwrap();
                payload
                    .observed_authority
                    .resources
                    .push("unobserved-resource".into());
                broken.payload_json = Set(serde_json::to_string(&payload).unwrap());
            }
        }
        broken.update(&store.db).await.unwrap();
        assert!(
            store
                .read_rehearsal_reads(1, &rehearsal.rehearsal_id)
                .await
                .is_err(),
            "case {case}"
        );
        let restored: outbox_row::ActiveModel = original.clone().into();
        restored.reset_all().update(&store.db).await.unwrap();
    }
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut changed: agent_session::ActiveModel = row.into();
    changed.version = Set(rehearsal.completed_session_version.unwrap() + 1);
    changed.update(&store.db).await.unwrap();
    assert!(
        store
            .read_rehearsal_reads(1, &rehearsal.rehearsal_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn failed_read_is_visible_but_never_successful_permission_evidence() {
    let (store, rehearsal) = fixture(false).await;
    let report = store
        .read_rehearsal_reads(1, &rehearsal.rehearsal_id)
        .await
        .unwrap();
    assert!(report.reads.is_empty());
    assert_eq!(report.unconfirmed_read_call_ids, vec!["model-read-1"]);
    let view = crate::schedule_management::rehearsal_permissions::read(
        &store.db,
        1,
        &rehearsal.rehearsal_id,
    )
    .await
    .unwrap();
    let value = serde_json::to_value(view).unwrap();
    assert!(value["observations"].as_array().unwrap().is_empty());
    assert_eq!(
        value["unconfirmed_tool_call_ids"],
        serde_json::json!(["model-read-1"])
    );
    assert!(
        value["unclassified_tool_call_ids"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn publication_evidence_requires_successful_provider_read_receipt() {
    for success in [true, false] {
        let (store, rehearsal) = fixture(success).await;
        let task = store.read(1, &rehearsal.schedule_id).await.unwrap();
        let txn = store.db.begin().await.unwrap();
        let proof =
            ScheduleStore::publication_rehearsal_evidence_on(&txn, &task, &rehearsal.rehearsal_id)
                .await;
        assert_eq!(proof.is_ok(), success);
        txn.rollback().await.unwrap();
        assert_eq!(store.read(1, &rehearsal.schedule_id).await.unwrap(), task);
    }
}

#[tokio::test]
async fn exact_contract_coverage_requires_observed_input_and_allows_omitting_exploration() {
    use desk_agent_protocol::schedule::contract::*;
    use desk_diagnose_core::schedule::contract::validate_contract;
    let (store, rehearsal) = fixture(true).await;
    let task = store.read(1, &rehearsal.schedule_id).await.unwrap();
    let report = store
        .read_rehearsal_reads(1, &rehearsal.rehearsal_id)
        .await
        .unwrap();
    let observed = &report.reads[0].authority;
    let scope = TaskPermissionScope {
        resources: observed.resources.clone(),
        operations: observed.operations.clone(),
        export_destinations: observed.export_destinations.clone(),
        limits: desk_agent_protocol::capability_grant::CapabilityGrantLimits {
            max_calls: 1,
            max_bytes_per_call: 10000,
            max_items_per_call: 10,
        },
    };
    let mut definition = TaskContract {
        schema_version: 1,
        schedule_id: task.schedule_id.clone(),
        task_revision: task.task_revision as u64,
        contract_revision: 1,
        target_device_id: task.target_device_id.clone(),
        prompt_sha256: digest(&task.prompt),
        permissions: vec![TaskPermissionRule {
            rule_id: "read".into(),
            provider_id: observed.provider_id.clone(),
            capability_id: observed.capability_id.clone(),
            tool_name: observed.tool_name.clone(),
            tool_schema_version: observed.tool_schema_version,
            effect: observed.effect,
            risk_tier: observed.risk_tier,
            input: TaskInputConstraint::Exact {
                canonical_json: "{}".into(),
            },
            automatic: scope.clone(),
            approval_ceiling: scope,
        }],
        steps: vec![],
        exception_mode: TaskExceptionMode::Deny,
        budget: TaskBudget {
            max_runs_per_utc_day: 1,
            max_calls_per_run: 1,
            max_model_tokens_per_run: 10000,
            max_runtime_seconds: 60,
        },
    };
    let txn = store.db.begin().await.unwrap();
    let validated = validate_contract(&definition).unwrap();
    let proof = ScheduleStore::publication_contract_scope_evidence_on(
        &txn,
        &task,
        &validated,
        &rehearsal.rehearsal_id,
    )
    .await
    .unwrap();
    definition.permissions[0].input = TaskInputConstraint::Exact {
        canonical_json: "{\"other\":true}".into(),
    };
    assert!(
        ScheduleStore::publication_contract_scope_evidence_on(
            &txn,
            &task,
            &validate_contract(&definition).unwrap(),
            &rehearsal.rehearsal_id
        )
        .await
        .is_err()
    );
    definition.permissions[0].input = TaskInputConstraint::ScopedRead;
    let scoped = ScheduleStore::publication_contract_scope_evidence_on(
        &txn,
        &task,
        &validate_contract(&definition).unwrap(),
        &rehearsal.rehearsal_id,
    )
    .await
    .unwrap();
    assert_ne!(proof.evidence_sha256, scoped.evidence_sha256);
    let mut expanded = definition.clone();
    expanded.permissions[0]
        .automatic
        .resources
        .push("unobserved".into());
    expanded.permissions[0]
        .approval_ceiling
        .resources
        .push("unobserved".into());
    assert!(
        ScheduleStore::publication_contract_scope_evidence_on(
            &txn,
            &task,
            &validate_contract(&expanded).unwrap(),
            &rehearsal.rehearsal_id,
        )
        .await
        .is_err()
    );
    definition.permissions.clear();
    let omitted = ScheduleStore::publication_contract_scope_evidence_on(
        &txn,
        &task,
        &validate_contract(&definition).unwrap(),
        &rehearsal.rehearsal_id,
    )
    .await
    .unwrap();
    assert_ne!(proof.evidence_sha256, omitted.evidence_sha256);
    txn.rollback().await.unwrap();
    assert_eq!(store.read(1, &rehearsal.schedule_id).await.unwrap(), task);
    assert!(task.authorization_revision.is_none());
}

#[tokio::test]
async fn read_sources_bind_successful_receipt_to_frozen_result() {
    let (store, rehearsal) = fixture(true).await;
    let txn = store.db.begin().await.unwrap();
    let sources = ScheduleStore::read_rehearsal_read_sources_on(&txn, 1, &rehearsal.rehearsal_id)
        .await
        .unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].lineage.envelope_id, "original-read-source");
    let reads = ScheduleStore::read_rehearsal_reads_on(&txn, 1, &rehearsal.rehearsal_id)
        .await
        .unwrap();
    assert_eq!(
        sources[0].authority.authority,
        desk_diagnose_core::schedule::source_graph::TaskSourceAuthority::Scopes(
            reads.reads[0].authority.resources.clone()
        )
    );
    assert!(
        ScheduleStore::read_rehearsal_read_sources_on(&txn, 2, &rehearsal.rehearsal_id)
            .await
            .is_err()
    );
    let row = work_row::Entity::find().one(&txn).await.unwrap().unwrap();
    let mut completion: CapabilityDispatchCompletion =
        serde_json::from_str(row.result_json.as_ref().unwrap()).unwrap();
    completion.result_digest_sha256 = "a".repeat(64);
    let mut changed: work_row::ActiveModel = row.into();
    changed.result_json = Set(Some(serde_json::to_string(&completion).unwrap()));
    changed.update(&txn).await.unwrap();
    assert!(
        ScheduleStore::read_rehearsal_read_sources_on(&txn, 1, &rehearsal.rehearsal_id)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    let txn = store.db.begin().await.unwrap();
    assert_eq!(
        ScheduleStore::read_rehearsal_read_sources_on(&txn, 1, &rehearsal.rehearsal_id)
            .await
            .unwrap()
            .len(),
        1
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn model_sources_join_frozen_reads_and_original_successful_receipt() {
    model_sources_fixture(false).await;
}

#[tokio::test]
async fn compressed_model_sources_join_original_receipts_in_frozen_transaction() {
    model_sources_fixture(true).await;
}

async fn model_sources_fixture(compressed: bool) {
    use crate::{
        assistant_model::{ModelExportSource, model_export_id},
        model_egress_store::SignalModelEgressStore,
    };
    use desk_agent_protocol::schedule::contract::{TaskBudget, TaskContract, TaskExceptionMode};
    use desk_diagnose_core::{
        chat::{ModelTurn, StopReason},
        model_egress::ModelEgressPolicy,
        model_message_labels::model_bound_user_message,
        prompt::ResponseFormatSpec,
        schedule::{contract::validate_contract, rehearsal::fixed_input::task_input_scope},
        seam::ModelRequest,
    };
    let (store, rehearsal) = if compressed {
        fixture_with_text(true, Some(&"x".repeat(16000)), &"r".repeat(1000)).await
    } else {
        fixture(true).await
    };
    let schema = Schema::new(store.db.get_database_backend());
    store
        .db
        .execute(&schema.create_table_from_entity(crate::entity::model_egress_receipt::Entity))
        .await
        .unwrap();
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let input_id = format!("rehearsal:{}:input", rehearsal.rehearsal_id);
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let user = model_bound_user_message(
        input_id.clone(),
        rehearsal.prompt.clone(),
        destination.clone(),
    )
    .unwrap()
    .with_turn_id("turn-1");
    session.conversation[0] = user.clone();
    let read = session
        .conversation
        .iter()
        .find(|m| m.message_id == "result")
        .unwrap()
        .clone();
    let policy = ModelEgressPolicy {
        destination,
        selected_source_tools: [read
            .data_envelope
            .as_ref()
            .unwrap()
            .provenance
            .source_tool_name
            .clone()]
        .into_iter()
        .collect(),
        export_authorization_id: model_export_id(
            "1",
            &session.device_id,
            &session.conversation_id,
            ModelExportSource::Input(&input_id),
        ),
        now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        omit_finite_retention_historical_turns: false,
    };
    let model = SignalModelEgressStore::new(store.db.clone());
    let model_messages = if compressed {
        use desk_diagnose_core::{
            model_context::*, model_profile::WireProtocol, replay::SourceContextKey,
        };
        use sha2::{Digest, Sha256};
        let projected = policy
            .authorize_request(ModelRequest::text_only(
                vec![user.clone()],
                ResponseFormatSpec::None,
            ))
            .unwrap();
        let proposal = session
            .conversation
            .iter_mut()
            .find(|m| !m.tool_calls.is_empty())
            .unwrap();
        let proposal_turn = ModelTurn {
            text: proposal.text.clone(),
            tool_calls: proposal
                .tool_calls
                .iter()
                .map(|c| desk_diagnose_core::chat::ToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments_json: c.arguments_json.clone(),
                })
                .collect(),
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        };
        let label = policy
            .derive_model_output_envelope(&proposal_turn, &projected.input_envelopes)
            .unwrap();
        model
            .record_dispatch_intent(
                "source-proposal".into(),
                policy.export_authorization_id.clone(),
                1,
                &projected.audit,
                &projected.input_envelopes,
            )
            .await
            .unwrap();
        model
            .mark_succeeded("source-proposal", &label)
            .await
            .unwrap();
        proposal.data_envelope = Some(label);
        let history = &session.conversation[..session.conversation.len() - 1];
        let context = PinnedContextPolicy::checkpoint_summary(
            SourceContextKey::derive(
                WireProtocol::OpenAiChatCompletions,
                "gateway",
                "model",
                "test",
            ),
            1,
            desk_diagnose_core::MIN_MODEL_CONTEXT_BYTES * 5,
            1,
        )
        .unwrap();
        let ContextBuildPlan::NeedsCompression(plan) = plan_model_context(
            history,
            &session.model_context_state,
            &context,
            &ContextProtectionSet::default(),
            7,
        )
        .unwrap() else {
            panic!("compression expected");
        };
        let input = authorize_compression_input(&policy, &plan, history).unwrap();
        let projected = policy
            .authorize_request(ModelRequest::text_only(
                input.messages.clone(),
                ResponseFormatSpec::None,
            ))
            .unwrap();
        let mut turn = ModelTurn { text: serde_json::json!({"goals":[{"text":"Original requirement","source_message_ids":[input_id]}]}).to_string(), stop_reason: StopReason::EndTurn, ..Default::default() };
        let output = policy
            .derive_model_output_envelope(&turn, &projected.input_envelopes)
            .unwrap();
        model
            .record_dispatch_intent(
                "source-compression".into(),
                policy.export_authorization_id.clone(),
                2,
                &projected.audit,
                &projected.input_envelopes,
            )
            .await
            .unwrap();
        model
            .mark_succeeded("source-compression", &output)
            .await
            .unwrap();
        turn.provider_meta.data_envelope = Some(output);
        let provenance = CompressorProvenanceV1::for_call(
            &context,
            "a".repeat(64),
            "b".repeat(64),
            1,
            format!("{:x}", Sha256::digest(b"source-compression")),
            "2026-09-07T00:00:00Z",
            "turn-1",
        );
        let mut summary = parse_validated_context_summary(&turn.text, &plan, provenance).unwrap();
        bind_context_summary_lineage(&policy, &mut summary, &turn, &input).unwrap();
        let (next, view) =
            apply_validated_checkpoint(&plan, summary, history, &session.model_context_state, 7)
                .unwrap();
        session.model_context_state = next;
        vec![view.messages[0].clone()]
    } else {
        vec![user, read]
    };
    let authorized = policy
        .authorize_request(ModelRequest::text_only(
            model_messages,
            ResponseFormatSpec::None,
        ))
        .unwrap();
    let turn = ModelTurn {
        text: "Report complete".into(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    let output = policy
        .derive_model_output_envelope(&turn, &authorized.input_envelopes)
        .unwrap();
    session
        .conversation
        .iter_mut()
        .find(|m| m.message_id == "answer")
        .unwrap()
        .data_envelope = Some(output.clone());
    let encoded = session.encode_json_for_storage().unwrap();
    let mut updated: agent_session::ActiveModel = row.into();
    updated.state_json = Set(encoded.clone());
    updated.update(&store.db).await.unwrap();
    let mut frozen: rehearsal::ActiveModel = rehearsal.clone().into();
    frozen.completed_session_sha256 = Set(Some(digest(&encoded)));
    frozen.update(&store.db).await.unwrap();
    let definition = TaskContract {
        schema_version: 1,
        schedule_id: rehearsal.schedule_id.clone(),
        task_revision: rehearsal.task_revision as u64,
        contract_revision: 1,
        target_device_id: rehearsal.target_device_id.clone(),
        prompt_sha256: rehearsal.prompt_sha256.clone(),
        permissions: vec![],
        steps: vec![],
        exception_mode: TaskExceptionMode::Deny,
        budget: TaskBudget {
            max_runs_per_utc_day: 1,
            max_calls_per_run: 4,
            max_model_tokens_per_run: 10000,
            max_runtime_seconds: 60,
        },
    };
    let contract = validate_contract(&definition).unwrap();
    model
        .record_dispatch_intent(
            "source-model".into(),
            policy.export_authorization_id.clone(),
            if compressed { 3 } else { 1 },
            &authorized.audit,
            &authorized.input_envelopes,
        )
        .await
        .unwrap();
    let txn = store.db.begin().await.unwrap();
    assert!(
        ScheduleStore::read_rehearsal_model_sources_on(
            &txn,
            1,
            &rehearsal.rehearsal_id,
            &contract,
            "answer"
        )
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
    model.mark_succeeded("source-model", &output).await.unwrap();
    let txn = store.db.begin().await.unwrap();
    let sources = ScheduleStore::read_rehearsal_model_sources_on(
        &txn,
        1,
        &rehearsal.rehearsal_id,
        &contract,
        "answer",
    )
    .await
    .unwrap();
    let reads = ScheduleStore::read_rehearsal_reads_on(&txn, 1, &rehearsal.rehearsal_id)
        .await
        .unwrap();
    let mut expected = reads.reads[0].authority.resources.clone();
    expected.push(task_input_scope(&contract));
    expected.sort();
    expected.dedup();
    assert_eq!(sources.scopes, expected);
    assert!(
        ScheduleStore::read_rehearsal_model_sources_on(
            &txn,
            2,
            &rehearsal.rehearsal_id,
            &contract,
            "answer"
        )
        .await
        .is_err()
    );
    let mut wrong = definition.clone();
    wrong.task_revision += 1;
    assert!(
        ScheduleStore::read_rehearsal_model_sources_on(
            &txn,
            1,
            &rehearsal.rehearsal_id,
            &validate_contract(&wrong).unwrap(),
            "answer"
        )
        .await
        .is_err()
    );
    assert!(
        ScheduleStore::read_rehearsal_model_sources_on(
            &txn,
            1,
            &rehearsal.rehearsal_id,
            &contract,
            "missing-output"
        )
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn generation_uses_verified_reads_and_does_not_save_or_authorize() {
    for success in [true, false] {
        let (store, rehearsal) = fixture(success).await;
        let task = store.read(1, &rehearsal.schedule_id).await.unwrap();
        let candidate = store
            .generate_task_contract(1, &task.schedule_id, task.revision)
            .await;
        assert_eq!(candidate.is_ok(), success, "{candidate:?}");
        if let Ok(candidate) = candidate {
            assert_eq!(candidate.permissions.len(), 1);
            assert_eq!(candidate.permissions[0].tool_name, "read_system_info");
            assert!(matches!(
                candidate.permissions[0].input,
                desk_agent_protocol::schedule::contract::TaskInputConstraint::ScopedRead
            ));
        }
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), task);
        assert!(
            store
                .generate_task_contract(2, &task.schedule_id, task.revision)
                .await
                .is_err()
        );
        if success {
            let schema = Schema::new(store.db.get_database_backend());
            store
                .db
                .execute(
                    &schema.create_table_from_entity(crate::entity::agent_task_contract::Entity),
                )
                .await
                .unwrap();
            use desk_agent_protocol::schedule::management::{
                ScheduleManagementRequest as Request, ScheduleManagementResponse as Response,
            };
            let Response::TaskContract {
                task: saved,
                contract: Some(contract),
                ..
            } = crate::schedule_management::manage(
                &store.db,
                1,
                Request::GenerateTaskContract {
                    schedule_id: task.schedule_id.clone(),
                    expected_revision: task.revision,
                },
            )
            .await
            .unwrap()
            else {
                panic!("generated contract response")
            };
            assert_eq!(saved.revision, task.revision + 1);
            assert_eq!(contract.target_device_id, task.target_device_id);
            assert!(saved.next_run_at.is_none());
            let persisted = store.read(1, &task.schedule_id).await.unwrap();
            assert!(persisted.authorization_revision.is_none());
            assert!(
                crate::schedule_management::manage(
                    &store.db,
                    1,
                    Request::GenerateTaskContract {
                        schedule_id: task.schedule_id.clone(),
                        expected_revision: task.revision // stale before the first generation
                    }
                )
                .await
                .is_err()
            );
            assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), persisted);
        }
    }
}
