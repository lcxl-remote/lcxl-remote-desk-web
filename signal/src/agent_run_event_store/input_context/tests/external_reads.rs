use super::*;
use desk_diagnose_core::{
    chat::ToolCallRef,
    external_tools::{IssueAccess, IssueTarget},
};

#[actix_web::test]
async fn native_issue_transport_saves_lineage_and_rechecks_revocation_and_takeover() {
    native_issue_transport(desk_agent_protocol::external_tools::IssuePlatform::Gitea).await;
}

#[actix_web::test]
async fn github_native_issue_transport_saves_lineage_and_rechecks_revocation_and_takeover() {
    native_issue_transport(desk_agent_protocol::external_tools::IssuePlatform::Github).await;
}

async fn native_issue_transport(platform: desk_agent_protocol::external_tools::IssuePlatform) {
    use crate::{
        issue_connections,
        remote_tool_edge::{SignalAiAssistantTools, SignalRemoteToolPendingStore},
    };
    use desk_diagnose_core::{
        external_tools,
        model_egress::ModelEgressPolicy,
        seam::{ReadOutcome, ToolSeam},
    };
    use desk_signal_facade::{
        mcp_client::test_support::IssueFixture, model::connection::SharedConnectionMap,
    };
    use std::sync::Arc;
    let fixture = IssueFixture::start_for(platform).await;
    for interruption in 0..5 {
        let store = setup("sqlite::memory:").await;
        issue_connections::update(&store.db, &fixture.update("1"))
            .await
            .unwrap();
        let access = issue_connections::select_issues(&store.db, "1", &[fixture.selection()])
            .await
            .unwrap();
        let mut params = input("issue-input", vec![]);
        params.device_id.clear();
        params.current_scope.granted.clear();
        params.current_scope.mode = ExecutionMode::ReadOnly;
        params.created_at = chrono::Utc::now().to_rfc3339();
        params.run_id = desk_diagnose_core::conversation_key::derive_assistant_conversation_key(
            &params.actor_id,
            desk_diagnose_core::conversation_key::AssistantConversationScope::Personal,
            params.client_conversation_id.as_deref().unwrap(),
        );
        let mut original = ReadContextSelection::capture(&[], &params.current_scope).unwrap();
        original.external_issues = access.clone();
        let admitted = desk_signal_facade::mcp_client::test_support::admitted_issue_for(
            &params.actor_id,
            &params.run_id,
            access[0].clone(),
        );
        desk_assistant_data_store::issue_authority::put(
            &store.db,
            admitted.subscription.clone(),
            0,
            0,
        )
        .await
        .unwrap();
        desk_assistant_data_store::issue_authority::admit(&store.db, &admitted)
            .await
            .unwrap();
        original
            .external_issue_sources
            .push(desk_diagnose_core::issue_authority::IssueSourceRef {
                authorization_id: admitted.authorization_id,
                subscription_revision: 1,
                tool_ref: access[0].tool_ref(),
            });
        params.read_context = Some(original.clone());
        let receipt = store.append_user_followup(&params).await.unwrap();
        let mut held = sessions(&store)
            .with_expected_input_revision(receipt.input_revision)
            .claim_turn(ClaimTurnParams {
                conversation_id: params.run_id.clone(),
                actor_id: params.actor_id.clone(),
                device_id: String::new(),
                policy_revision: params.policy_revision,
                current_pdp_scope: params.current_scope.clone(),
                turn_id: "turn".into(),
                request_id: None,
                connection_id: None,
                now: params.created_at.clone(),
                trigger_origin: desk_diagnose_core::session::TriggerOrigin::User,
            })
            .await
            .unwrap();
        let call = ToolCall {
            id: "read-1".into(),
            name: external_tools::TOOL_NAME.into(),
            arguments_json:
                serde_json::json!({"tool_ref":access[0].tool_ref(),"args":{"method":"get"}})
                    .to_string(),
        };
        held.conversation.push(ChatMessage::assistant_tool_calls(
            "model",
            "",
            vec![call.to_ref()],
        ));
        sessions(&store).save(&mut held).await.unwrap();
        let tools = SignalAiAssistantTools::new(
            store.db.clone(),
            desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
            Arc::new(SharedConnectionMap::new_oss()),
            Arc::new(SignalRemoteToolPendingStore::default()),
            String::new(),
            String::new(),
            "1".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            vec![],
            vec![],
            vec![],
            None,
            None,
            params.message.text.clone(),
            params.run_id.clone(),
            "turn".into(),
            params.policy_revision,
            0,
            vec![],
            30_000,
        );
        tools
            .bind_original_input(
                held.input_revision,
                original.clone(),
                destination(),
                params.client_conversation_id.clone(),
            )
            .unwrap();
        assert_eq!(tools.external_issue_access(&held).await.unwrap(), access);
        let mut forged = call.clone();
        forged.arguments_json =
            serde_json::json!({"tool_ref":access[0].tool_ref(),"args":{"method":"add_comment"}})
                .to_string();
        let reads = fixture.reads();
        assert!(tools.run_external_read(&held, &forged).await.is_err());
        assert_eq!(fixture.reads(), reads);
        if interruption == 4 {
            let mut offline = fixture.update("1");
            offline.expected_revision = 1;
            offline.connections[0].mcp_url = format!("{}/missing-mcp", fixture.base_url);
            issue_connections::update(&store.db, &offline)
                .await
                .unwrap();
        }
        let result = if matches!(interruption, 0 | 4) {
            tools.run_external_read(&held, &call).await
        } else {
            fixture.pause_next_issue_check();
            let revoke = async {
                fixture.wait_for_read().await;
                if interruption == 1 {
                    issue_connections::update(
                        &store.db,
                        &desk_signal_facade::mcp_client::settings::IssueConnectionsUpdate {
                            expected_revision: 1,
                            connections: vec![],
                        },
                    )
                    .await
                    .unwrap();
                } else if interruption == 3 {
                    let mut paused = admitted.subscription.clone();
                    paused.enabled = false;
                    desk_assistant_data_store::issue_authority::put(
                        &store.db,
                        paused,
                        1,
                        chrono::Utc::now().timestamp_millis(),
                    )
                    .await
                    .unwrap();
                } else {
                    agent_session::Entity::update_many()
                        .col_expr(
                            agent_session::Column::LeaseToken,
                            Expr::value(held.lease_token as i64 + 1),
                        )
                        .filter(agent_session::Column::ConversationId.eq(&held.conversation_id))
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                fixture.release_read();
            };
            let (result, ()) = tokio::join!(tools.run_external_read(&held, &call), revoke);
            result
        };
        if matches!(interruption, 0 | 4) {
            let ReadOutcome::Completed {
                output,
                data_envelope: Some(label),
                ok: true,
                ..
            } = result.unwrap()
            else {
                panic!("Expected a labeled native read");
            };
            assert_eq!(fixture.reads(), reads);
            assert!(output.content.contains("Reproduce the macOS problem"));
            assert!(output.content.contains("Linux succeeds"));
            assert!(external_tools::is_issue_read_label(&label));
            let mut material = ChatMessage::text(
                "issue-material",
                desk_diagnose_core::chat::ChatRole::UntrustedOutput,
                output.content.clone(),
            );
            material.data_envelope = Some(label.clone());
            let policy = ModelEgressPolicy {
                destination: destination(),
                selected_source_tools: Default::default(),
                export_authorization_id: "issue-publication".into(),
                now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
                byte_cap: 1024 * 1024,
                permission_resume: false,
            };
            assert!(
                policy
                    .authorize_issue_reply_request(
                        &access[0].target.destination("forge".into()),
                        vec![material]
                    )
                    .is_ok()
            );
            let mut message =
                ChatMessage::tool_result("external-result", &call.id, &output.content);
            message.data_envelope = Some(label.clone());
            held.conversation.push(message);
            sessions(&store).save(&mut held).await.unwrap();
            let row = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&held.conversation_id))
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            let restored = PersistedAgentSession::decode_json(&row.state_json).unwrap();
            let result = restored
                .conversation
                .iter()
                .find(|message| message.message_id == "external-result")
                .unwrap();
            assert_eq!(result.text, output.content);
            assert_eq!(result.data_envelope.as_ref(), Some(&label));
            if interruption == 4 {
                let mut restored = fixture.update("1");
                restored.expected_revision = 2;
                issue_connections::update(&store.db, &restored)
                    .await
                    .unwrap();
            }
            freeze_publication(&store, &mut held, &tools, access[0].clone(), destination()).await;
        } else {
            assert!(result.is_err());
            if interruption == 1 {
                assert!(tools.external_issue_access(&held).await.unwrap().is_empty());
            }
        }
        assert_eq!(fixture.writes(), 0);
    }
    fixture.stop().await;
}

#[tokio::test]
async fn external_read_input_is_native_device_independent_and_rejects_a_stale_lease() {
    let store = setup("sqlite::memory:").await;
    let now = chrono::Utc::now();
    let mut params = input("issue-input", vec![]);
    params.device_id.clear();
    params.run_id = desk_diagnose_core::conversation_key::derive_assistant_conversation_key(
        &params.actor_id,
        desk_diagnose_core::conversation_key::AssistantConversationScope::Personal,
        params.client_conversation_id.as_deref().unwrap(),
    );
    params.current_scope.granted.clear();
    params.current_scope.mode = ExecutionMode::ReadOnly;
    params.created_at = now.to_rfc3339();
    let mut original = ReadContextSelection::capture(&[], &params.current_scope).unwrap();
    original.external_issues.push(IssueAccess {
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
    params.read_context = Some(original.clone());
    let receipt = store.append_user_followup(&params).await.unwrap();
    let claims = ClaimTurnParams {
        conversation_id: params.run_id.clone(),
        actor_id: params.actor_id.clone(),
        device_id: String::new(),
        policy_revision: params.policy_revision,
        current_pdp_scope: params.current_scope.clone(),
        turn_id: "turn".into(),
        request_id: None,
        connection_id: None,
        now: now.to_rfc3339(),
        trigger_origin: desk_diagnose_core::session::TriggerOrigin::User,
    };
    let mut held = sessions(&store)
        .with_expected_input_revision(receipt.input_revision)
        .claim_turn(claims)
        .await
        .unwrap();
    assert_eq!(
        store
            .checked_external_read_input(&held, &destination(), None)
            .await
            .unwrap(),
        original
    );
    assert!(
        store
            .checked_external_read_input(&held, &destination(), Some("read-1"))
            .await
            .is_err()
    );
    held.conversation.push(ChatMessage::assistant_tool_calls(
        "model",
        "",
        vec![ToolCallRef {
            id: "read-1".into(),
            name: desk_diagnose_core::external_tools::TOOL_NAME.into(),
            arguments_json: "{}".into(),
        }],
    ));
    sessions(&store).save(&mut held).await.unwrap();
    assert_eq!(
        store
            .checked_external_read_input(&held, &destination(), Some("read-1"))
            .await
            .unwrap(),
        original
    );
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::LeaseToken,
            Expr::value(held.lease_token as i64 + 1),
        )
        .filter(agent_session::Column::ConversationId.eq(&held.conversation_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        store
            .checked_external_read_input(&held, &destination(), Some("read-1"))
            .await
            .is_err()
    );
}

async fn freeze_publication(
    store: &SignalAgentRunEventStore,
    held: &mut PersistedAgentSession,
    tools: &impl desk_diagnose_core::seam::ToolSeam,
    access: IssueAccess,
    destination: desk_agent_protocol::data_lineage::DestinationIdentity,
) {
    use desk_assistant_data_store::issue_reply::{self, ReplyPreparation};
    use desk_diagnose_core::issue_reply::{FrozenIssueReply, PREPARE_TOOL_NAME, ReplyOrigin};
    held.conversation.push(ChatMessage::assistant_tool_calls(
        "publication-model",
        "",
        vec![ToolCallRef {
            id: "publish-1".into(),
            name: PREPARE_TOOL_NAME.into(),
            arguments_json: serde_json::json!({"tool_ref":access.tool_ref()}).to_string(),
        }],
    ));
    sessions(store).save(held).await.unwrap();
    let publish = desk_diagnose_core::chat::ToolCall {
        id: "publish-1".into(),
        name: PREPARE_TOOL_NAME.into(),
        arguments_json: serde_json::json!({"tool_ref":access.tool_ref()}).to_string(),
    };
    assert_eq!(
        tools.issue_reply_access(held).await.unwrap(),
        vec![access.clone()]
    );
    let desk_diagnose_core::issue_reply::ReplyGeneration::Generate(generation) =
        tools.prepare_issue_reply(held, &publish).await.unwrap()
    else {
        panic!("Expected original workset generation");
    };
    assert_eq!(generation.materials.len(), 1);
    assert!(generation.materials[0].text.contains("Linux succeeds"));
    assert!(desk_diagnose_core::issue_authority::is_issue_workset_label(
        generation.materials[0].data_envelope.as_ref().unwrap()
    ));
    let admitted = desk_signal_facade::mcp_client::test_support::admitted_issue_for(
        &held.actor_id,
        &held.conversation_id,
        access.clone(),
    );
    let sample = desk_signal_facade::mcp_client::test_support::reply_artifact_for(
        ReplyOrigin {
            actor_id: held.actor_id.clone(),
            root_conversation_id: held.conversation_id.clone(),
            request_id: admitted.request_id,
            execution_conversation_id: held.conversation_id.clone(),
            tool_call_id: "publish-1".into(),
            input_revision: held.input_revision,
            control_revision: held.control_revision,
            authorization_id: admitted.authorization_id,
            authorization_revision: 1,
        },
        access,
        generation.publication.comment_tool_signature.clone(),
        destination,
    );
    fn prepared<'a>(
        held: &'a PersistedAgentSession,
        sample: &FrozenIssueReply,
    ) -> ReplyPreparation<'a> {
        ReplyPreparation {
            held,
            origin: sample.origin.clone(),
            access: sample.access.clone(),
            body: sample.body.clone(),
            output: sample.output.clone(),
            comment_tool_signature: sample.comment_tool_signature.clone(),
            random_marker: [7; 16],
        }
    }
    let before = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&held.conversation_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        crate::issue_replies::freeze(&store.db, prepared(held, &sample))
            .await
            .is_err()
    );

    use crate::model_egress_store::SignalModelEgressStore;
    let txn = store.db.begin().await.unwrap();
    SignalModelEgressStore::record_snapshot_intent_on(
        &txn,
        "publication-call".into(),
        sample
            .native_sources
            .evidence
            .model_export_authorization_id
            .clone(),
        1,
        &sample.native_sources.input_snapshot,
    )
    .await
    .unwrap();
    SignalModelEgressStore::link_context_dispatch_on(
        &txn,
        "publication-call",
        &held.conversation_id,
        true,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("publication-call", &sample.output)
        .await
        .unwrap();

    let receipt = tools
        .freeze_issue_reply(
            held,
            generation.publication,
            desk_diagnose_core::chat::ModelTurn {
                text: sample.body.clone(),
                tool_calls: vec![],
                stop_reason: desk_diagnose_core::chat::StopReason::EndTurn,
                usage: Default::default(),
                provider_meta: {
                    let mut meta =
                        desk_diagnose_core::replay::ProviderResponseMeta::without_reasoning(
                            desk_diagnose_core::chat::StopReason::EndTurn,
                        );
                    meta.data_envelope = Some(sample.output.clone());
                    meta
                },
            },
        )
        .await
        .unwrap();
    let row = issue_reply::load(&store.db, &held.actor_id, &receipt.artifact_id)
        .await
        .unwrap()
        .unwrap();
    let replay = tools.prepare_issue_reply(held, &publish).await.unwrap();
    assert!(
        matches!(replay, desk_diagnose_core::issue_reply::ReplyGeneration::Existing(saved) if saved == receipt)
    );
    assert_eq!(row.state, issue_reply::READY);
    assert_eq!(row.actor_id, held.actor_id);
    let artifact = issue_reply::decode(&row).unwrap();
    assert_eq!(artifact.body, sample.body);
    artifact
        .authorize_delivery(chrono::Utc::now().timestamp_millis() as u64)
        .unwrap();
    let mut retry = prepared(held, &sample);
    retry.random_marker = [8; 16];
    assert_eq!(
        crate::issue_replies::freeze(&store.db, retry)
            .await
            .unwrap(),
        row
    );
    let mut forged = prepared(held, &sample);
    forged.body.push_str("Changed");
    assert!(
        crate::issue_replies::freeze(&store.db, forged)
            .await
            .is_err()
    );
    let after = agent_session::Entity::find()
        .filter(agent_session::Column::Id.eq(before.id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before, after);
    agent_session::Entity::update_many()
        .filter(agent_session::Column::Id.eq(before.id))
        .col_expr(
            agent_session::Column::LeaseToken,
            Expr::value(before.lease_token + 1),
        )
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        crate::issue_replies::freeze(&store.db, prepared(held, &sample))
            .await
            .is_err()
    );
    assert_eq!(
        issue_reply::load(&store.db, &held.actor_id, &row.artifact_id)
            .await
            .unwrap()
            .unwrap(),
        row
    );
}
