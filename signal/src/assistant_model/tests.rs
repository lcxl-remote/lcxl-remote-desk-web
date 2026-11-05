use super::*;

#[actix_web::test]
async fn publication_gateway_keeps_the_exact_original_issue_proof() {
    let run_id = desk_diagnose_core::conversation_key::derive_assistant_conversation_key(
        "1",
        desk_diagnose_core::conversation_key::AssistantConversationScope::Personal,
        "issue-model-gateway",
    );

    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let shared = captured.clone();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = actix_web::HttpServer::new(move || {
        let captured = shared.clone();
        actix_web::App::new().route("/v1/chat/completions", actix_web::web::post().to(move |body: actix_web::web::Json<serde_json::Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push(body.into_inner());
                let delta = serde_json::json!({"choices":[{"delta":{"content":"The issue is under investigation."},"finish_reason":"stop"}]});
                actix_web::HttpResponse::Ok().content_type("text/event-stream").body(format!("data: {delta}\n\ndata: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":2}}}}\n\ndata: [DONE]\n\n"))
            }
        }))
    }).workers(1).listen(listener).unwrap().run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("fake-model".into()),
            base_url: Some(format!("http://{address}/v1")),
            api_key: Some("fixture-key".into()),
            max_context_bytes: Some(131_072),
            runtime_max_output_tokens: 4096,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let config = crate::model_provider::load(&db).await.unwrap();
    let model = MeteredModel {
        fresh_task: None,
        inner: SignalModelSeam::from_config(&config)
            .unwrap()
            .with_context_db(db.clone()),
        db: db.clone(),
        destination: config.destination_identity().unwrap(),
        selected_source_tools: Default::default(),
        export_authorization_id: "ordinary-export".into(),
        permission_resume: false,
        completed_compression_receipt: std::cell::RefCell::new(None),
        model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
    };
    use desk_diagnose_core::{
        external_tools::{IssueAccess, IssueTarget},
        issue_reply::{FrozenIssueReply, ReplyOrigin, publication_export_id},
    };
    let mut earlier = desk_diagnose_core::seam::ModelRequest::text_only(
        vec![
            desk_diagnose_core::model_message_labels::model_bound_user_message(
                "private-input".into(),
                "PRIVATE_PREVIOUS_INPUT".into(),
                model.destination.clone(),
            )
            .unwrap(),
        ],
        desk_diagnose_core::prompt::ResponseFormatSpec::None,
    );
    earlier.context_conversation_id = Some(run_id.clone());
    model
        .call(earlier, &mut desk_diagnose_core::seam::NullTurnSink)
        .await
        .unwrap();
    let planning_context = crate::model_egress_store::SignalModelEgressStore::new(db.clone())
        .latest_context(&run_id)
        .await
        .unwrap();
    assert!(planning_context.is_some());
    let access = IssueAccess {
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
            private: false,
            internal: false,
        },
    };
    let (held, admitted) =
        seed_native_publication(&db, access.clone(), model.destination.clone()).await;
    let origin = ReplyOrigin {
        actor_id: held.actor_id.clone(),
        root_conversation_id: held.conversation_id.clone(),
        request_id: admitted.request_id.clone(),
        execution_conversation_id: held.conversation_id.clone(),
        tool_call_id: "publish-1".into(),
        input_revision: held.input_revision,
        control_revision: held.control_revision,
        authorization_id: admitted.authorization_id.clone(),
        authorization_revision: 1,
    };
    let policy = model.model_egress_policy().unwrap().unwrap();
    let mut request = policy
        .prepare_issue_reply_call(
            origin.clone(),
            access.clone(),
            vec![admitted.publication_material(&policy.destination).unwrap()],
        )
        .unwrap();
    request.bind_issue_reply_native_input(&held).unwrap();
    let mut missing_skill = held.clone();
    missing_skill.skills.reset_for_input(0);
    let mut without_skill = policy
        .prepare_issue_reply_call(
            origin.clone(),
            access.clone(),
            vec![admitted.publication_material(&policy.destination).unwrap()],
        )
        .unwrap();
    without_skill
        .bind_issue_reply_native_input(&missing_skill)
        .unwrap();
    assert!(model.prepare_request(&without_skill).is_ok());
    assert!(
        model
            .call(without_skill, &mut desk_diagnose_core::seam::NullTurnSink)
            .await
            .is_err()
    );
    let mut changed = request.clone();
    changed
        .messages
        .push(desk_diagnose_core::chat::ChatMessage::text(
            "private-history",
            desk_diagnose_core::chat::ChatRole::User,
            "PRIVATE_WORKER_HISTORY",
        ));
    assert!(model.prepare_request(&changed).is_err());
    assert!(
        model
            .call(changed, &mut desk_diagnose_core::seam::NullTurnSink)
            .await
            .is_err()
    );
    let mut forged_snapshot = admitted.clone();
    forged_snapshot.workset.body = "UNADMITTED_COMMENT".into();
    let mut forged = policy
        .prepare_issue_reply_call(
            origin.clone(),
            access.clone(),
            vec![
                forged_snapshot
                    .publication_material(&policy.destination)
                    .unwrap(),
            ],
        )
        .unwrap();
    forged.bind_issue_reply_native_input(&held).unwrap();
    assert!(
        model
            .call(forged, &mut desk_diagnose_core::seam::NullTurnSink)
            .await
            .is_err()
    );
    let mut another_holder = held.clone();
    another_holder.lease_token += 1;
    let mut stale = request.clone();
    stale
        .bind_issue_reply_native_input(&another_holder)
        .unwrap();
    assert!(model.prepare_request(&stale).is_ok());
    assert!(
        model
            .call(stale, &mut desk_diagnose_core::seam::NullTurnSink)
            .await
            .is_err()
    );
    let prepared = model.prepare_request(&request).unwrap().unwrap();
    prepared.require_fit().unwrap();
    assert!(
        model
            .model_input_token_upper_bound(&request)
            .unwrap()
            .is_some()
    );
    let turn = model
        .call(request, &mut desk_diagnose_core::seam::NullTurnSink)
        .await
        .unwrap();
    let output = turn.provider_meta.data_envelope.clone().unwrap();
    let sources = crate::model_egress_store::SignalModelEgressStore::read_context_output_on(
        &db, &run_id, &output,
    )
    .await
    .unwrap();
    assert_eq!(
        sources.evidence.model_export_authorization_id,
        publication_export_id(&origin, &access)
    );
    let artifact = FrozenIssueReply::freeze(
        origin,
        access,
        turn.text,
        output,
        sources,
        "b".repeat(64),
        &[9; 16],
    )
    .unwrap();
    artifact
        .authorize_delivery(chrono::Utc::now().timestamp_millis() as u64)
        .unwrap();
    assert_eq!(
        model
            .model_egress_policy()
            .unwrap()
            .unwrap()
            .export_authorization_id,
        policy.export_authorization_id
    );

    assert_eq!(
        crate::model_egress_store::SignalModelEgressStore::new(db.clone())
            .latest_context(&run_id)
            .await
            .unwrap(),
        planning_context
    );
    let mut paused = admitted.subscription.clone();
    paused.enabled = false;
    desk_assistant_data_store::issue_authority::put(
        &db,
        paused,
        1,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .unwrap();
    let mut revoked = policy
        .prepare_issue_reply_call(
            artifact.origin.clone(),
            artifact.access.clone(),
            vec![admitted.publication_material(&policy.destination).unwrap()],
        )
        .unwrap();
    revoked.bind_issue_reply_native_input(&held).unwrap();
    assert!(
        model
            .call(revoked, &mut desk_diagnose_core::seam::NullTurnSink)
            .await
            .is_err()
    );
    let requests = captured.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let body = requests[1].to_string();
    assert!(!body.contains("PRIVATE_PREVIOUS_INPUT"));
    assert!(body.contains("macOS problem"));
    assert!(body.contains("# Issue analysis"));
    assert!(body.contains("# Investigation and reply checklist"));
    assert!(!body.contains("PRIVATE_WORKER_HISTORY"));
    assert!(!body.contains("issue_reply"));
    assert!(!body.contains(&admitted.authorization_id));
    assert!(!body.contains(&admitted.request_id));
    assert!(!body.contains("PRIVATE_OWNER_REQUIREMENT"));
    assert!(!body.contains("data_envelope"));
    handle.stop(true).await;
}

async fn seed_native_publication(
    db: &crate::config::connection::DatabaseConnection,
    access: desk_diagnose_core::external_tools::IssueAccess,
    destination: desk_agent_protocol::data_lineage::DestinationIdentity,
) -> (
    desk_diagnose_core::session::PersistedAgentSession,
    desk_diagnose_core::issue_authority::IssueAdmission,
) {
    use desk_diagnose_core::{
        chat::{ChatMessage, ToolCallRef},
        input_read_context::ReadContextSelection,
        issue_authority::IssueSourceRef,
        seam::{ClaimTurnParams, SessionSeam},
        session::{AgentSessionSurface, TriggerOrigin},
    };

    let run_id = desk_diagnose_core::conversation_key::derive_assistant_conversation_key(
        "1",
        desk_diagnose_core::conversation_key::AssistantConversationScope::Personal,
        "issue-model-gateway",
    );
    let admitted = desk_signal_facade::mcp_client::test_support::admitted_issue_for(
        "1",
        &run_id,
        access.clone(),
    );
    desk_assistant_data_store::issue_authority::put(db, admitted.subscription.clone(), 0, 0)
        .await
        .unwrap();
    desk_assistant_data_store::issue_authority::admit(db, &admitted)
        .await
        .unwrap();
    crate::issue_connections::update(
        db,
        &desk_signal_facade::mcp_client::settings::IssueConnectionsUpdate {
            expected_revision: 0,
            connections: vec![
                desk_signal_facade::mcp_client::settings::IssueConnectionUpdate {
                    platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
                    connection_ref: access.connection_ref.clone(),
                    name: "Gateway issue fixture".into(),
                    enabled: true,
                    instance_url: access.target.instance.clone(),
                    mcp_url: format!("{}mcp", access.target.instance),
                    allowed_actor_ids: vec!["1".into()],
                    token: Some("fixture-token".into()),
                    extra_ca_pem: None,
                },
            ],
        },
    )
    .await
    .unwrap();
    let scope = desk_agent_protocol::AgentScope {
        granted: vec![],
        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let mut selected = ReadContextSelection::default();
    selected.external_issues.push(access.clone());
    selected.external_issue_sources.push(IssueSourceRef {
        authorization_id: admitted.authorization_id.clone(),
        subscription_revision: 1,
        tool_ref: access.tool_ref(),
    });
    let now = chrono::Utc::now().to_rfc3339();
    let params = crate::agent_run_event_store::AppendUserFollowupParams {
        client_timezone: "UTC".into(),
        temporary_conversation: None,
        run_id: run_id.clone(),
        actor_id: "1".into(),
        device_id: String::new(),
        target_refs: vec![],
        client_conversation_id: Some("issue-model-gateway".into()),
        policy_revision: 1,
        current_scope: scope.clone(),
        read_context: Some(selected),
        message: desk_diagnose_core::model_message_labels::model_bound_user_message(
            "owner-input".into(),
            "PRIVATE_OWNER_REQUIREMENT".into(),
            destination,
        )
        .unwrap(),
        created_at: now.clone(),
        event_id: "issue-event".into(),
        surface: AgentSessionSurface::AiAssistant,
    };
    let store = crate::agent_run_event_store::SignalAgentRunEventStore::new(db.clone());
    let receipt = store.append_user_followup(&params).await.unwrap();
    let claim = ClaimTurnParams {
        conversation_id: run_id,
        actor_id: "1".into(),
        device_id: String::new(),
        turn_id: "issue-turn".into(),
        request_id: None,
        connection_id: None,
        policy_revision: 1,
        current_pdp_scope: scope,
        now,
        trigger_origin: TriggerOrigin::User,
    };
    let mut held = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(
            Some("issue-model-gateway".into()),
            AgentSessionSurface::AiAssistant,
        )
        .with_expected_input_revision(receipt.input_revision)
        .claim_turn(claim)
        .await
        .unwrap();
    for resource in [None, Some("references/checklist.md")] {
        held.skills
            .apply(
                held.input_revision,
                &desk_diagnose_core::chat::ToolCall {
                    id: if resource.is_some() {
                        "native-skill-reference"
                    } else {
                        "native-skill-load"
                    }
                    .into(),
                    name: desk_diagnose_core::skills::LOAD_TOOL.into(),
                    arguments_json:
                        serde_json::json!({"skill_ref":"issue-analysis","resource":resource})
                            .to_string(),
                },
            )
            .unwrap();
    }
    held.conversation.push(ChatMessage::assistant_tool_calls(
        "publisher",
        "",
        vec![ToolCallRef {
            id: "publish-1".into(),
            name: desk_diagnose_core::issue_reply::PREPARE_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"tool_ref":access.tool_ref()}).to_string(),
        }],
    ));
    crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(
            Some("issue-model-gateway".into()),
            AgentSessionSurface::AiAssistant,
        )
        .save(&mut held)
        .await
        .unwrap();
    (held, admitted)
}
