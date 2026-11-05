use super::*;
use crate::entity::agent_session;
use desk_diagnose_core::{
    issue_reply::ReplyOrigin,
    session::{AgentSessionSurface, PersistedAgentSession, TurnState},
};
use desk_signal_facade::mcp_client::{
    settings::IssueConnectionsUpdate,
    test_support::{IssueFixture, reply_artifact_for},
};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, sea_query::Expr};
use std::sync::atomic::{AtomicBool, Ordering};

async fn database() -> DatabaseConnection {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();

    db
}

async fn other_work(db: &DatabaseConnection) -> agent_session::Model {
    let mut b = PersistedAgentSession::new(
        "work-b",
        "1",
        "",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        &chrono::Utc::now().to_rfc3339(),
    );
    b.surface = AgentSessionSurface::AiAssistant;
    b.input_revision = 2;
    b.current_turn_id = Some("turn-b".into());
    b.turn_state = TurnState::Running;
    b.chain_id = "chain-b".into();
    b.automation_turns_used = 3;
    b.review_denials.insert("execute".into(), 2);
    agent_session::ActiveModel {
        conversation_id: Set("work-b".into()),
        actor_id: Set("1".into()),
        device_id: Set(String::new()),
        conversation_schema_version: Set(Some(i32::from(
            desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
        ))),
        state_json: Set(b.encode_json_for_storage().unwrap()),
        version: Set(b.version),
        lease_token: Set(b.lease_token as i64),
        lease_deadline: Set(Some(chrono::Utc::now() + chrono::Duration::minutes(2))),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap()
}

#[actix_web::test]
async fn durable_replies_send_once_and_do_not_resume_other_work() {
    use desk_agent_protocol::external_tools::IssuePlatform;
    for (platform, case) in [IssuePlatform::Gitea, IssuePlatform::Github]
        .into_iter()
        .flat_map(|platform| (0..16).map(move |case| (platform, case)))
    {
        let db = database().await;
        let b = other_work(&db).await;
        let fixture = IssueFixture::start_for(platform).await;
        fixture.enable_replies(case == 1 || case == 10);
        issue_connections::update(&db, &fixture.update("1"))
            .await
            .unwrap();
        let settings = issue_connections::read(&db).await.unwrap();
        let saved = settings.connections[0].clone();
        let client = tokio::task::spawn_blocking(move || saved.client())
            .await
            .unwrap()
            .unwrap();
        let access = issue_connections::select_issues(&db, "1", &[fixture.selection()])
            .await
            .unwrap()
            .remove(0);
        let remote = client.reopen_issue(&access.target).await.unwrap();
        let signature = if case == 9 {
            "f".repeat(64)
        } else {
            remote.comment_signature().unwrap().to_owned()
        };
        let admitted = desk_signal_facade::mcp_client::test_support::admitted_issue_for(
            "1",
            "root",
            access.clone(),
        );
        if case == 0 || case >= 13 {
            desk_assistant_data_store::issue_authority::put(
                &db,
                admitted.subscription.clone(),
                0,
                0,
            )
            .await
            .unwrap();
            desk_assistant_data_store::issue_authority::admit(&db, &admitted)
                .await
                .unwrap();
            if case == 15 {
                desk_assistant_data_store::issue_authority::stop_request(
                    &db,
                    "1",
                    &admitted.request_id,
                    chrono::Utc::now().timestamp_millis(),
                )
                .await
                .unwrap();
            }
        }
        let artifact = reply_artifact_for(
            ReplyOrigin {
                actor_id: "1".into(),
                root_conversation_id: "root".into(),
                request_id: if case == 0 || case >= 13 {
                    admitted.request_id.clone()
                } else {
                    "request-a".into()
                },
                execution_conversation_id: "work-a".into(),
                tool_call_id: "publish-1".into(),
                input_revision: 1,
                control_revision: 0,
                authorization_id: if case == 0 || case >= 13 {
                    admitted.authorization_id.clone()
                } else {
                    "grant-a".into()
                },
                authorization_revision: 1,
            },
            access,
            signature,
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "gateway".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        );
        let now = chrono::Utc::now().timestamp_millis();
        issue_reply::freeze(&db, &artifact, DeliveryMode::Automatic, now)
            .await
            .unwrap();
        let authorized = AtomicBool::new(case != 2);
        let check = async |_: &DatabaseTransaction, candidate: &FrozenIssueReply| {
            assert_eq!(candidate.origin.request_id, "request-a");
            assert_eq!(candidate.origin.authorization_id, "grant-a");
            Ok(authorized.load(Ordering::SeqCst).then_some(2))
        };
        let reads_before = fixture.reads();
        if (3..=6).contains(&case) || case == 11 || case == 13 || case == 14 {
            fixture.pause_next_read();
        }
        if case == 7 || case == 8 || case == 12 {
            fixture.pause_next_reply();
        }
        let work = async {
            if case == 0 || case >= 13 {
                deliver_next(&db, "node-a", "claim-a").await
            } else {
                deliver_next_with_authority(&db, "node-a", "claim-a", &check).await
            }
        };
        if (3..=8).contains(&case) || case == 11 || case == 12 || case == 13 || case == 14 {
            let interrupt = async {
                tokio::time::timeout(std::time::Duration::from_secs(10), fixture.wait_for_read())
                    .await
                    .unwrap();
                match case {
                    3 => authorized.store(false, Ordering::SeqCst),
                    4 | 7 => {
                        issue_reply::cancel(
                            &db,
                            "1",
                            &artifact.artifact_id,
                            chrono::Utc::now().timestamp_millis(),
                        )
                        .await
                        .unwrap();
                    }
                    5 => {
                        agent_issue_reply::Entity::update_many()
                            .filter(agent_issue_reply::Column::ArtifactId.eq(&artifact.artifact_id))
                            .col_expr(
                                agent_issue_reply::Column::ClaimToken,
                                Expr::value("new-owner"),
                            )
                            .col_expr(agent_issue_reply::Column::Attempt, Expr::value(2))
                            .exec(&db)
                            .await
                            .unwrap();
                    }
                    6 => {
                        issue_connections::update(
                            &db,
                            &IssueConnectionsUpdate {
                                expected_revision: 1,
                                connections: vec![],
                            },
                        )
                        .await
                        .unwrap();
                    }
                    8 => {
                        agent_issue_reply::Entity::update_many()
                            .filter(agent_issue_reply::Column::ArtifactId.eq(&artifact.artifact_id))
                            .col_expr(
                                agent_issue_reply::Column::LeaseExpiresAtMs,
                                Expr::value(chrono::Utc::now().timestamp_millis() - 1),
                            )
                            .exec(&db)
                            .await
                            .unwrap();
                    }
                    11 | 12 => {
                        use sea_orm::TransactionTrait;
                        let txn = db.begin().await.unwrap();
                        issue_reply::lifecycle::retire_conversation_on(
                            &txn,
                            "1",
                            "root",
                            chrono::Utc::now().timestamp_millis(),
                        )
                        .await
                        .unwrap();
                        txn.commit().await.unwrap();
                    }
                    13 | 14 => {
                        let mut changed = admitted.subscription.clone();
                        if case == 13 {
                            changed.enabled = false;
                        } else {
                            changed.allowed_sender_ids.push(8);
                        }
                        desk_assistant_data_store::issue_authority::put(
                            &db,
                            changed,
                            1,
                            chrono::Utc::now().timestamp_millis(),
                        )
                        .await
                        .unwrap();
                    }
                    _ => unreachable!(),
                }
                if case == 7 || case == 8 || case == 12 {
                    assert!(
                        deliver_next_with_authority(&db, "node-b", "claim-b", async |_, _| panic!(
                            "Reconciliation must not open a new write authorization"
                        ))
                        .await
                        .unwrap()
                    );
                }
                fixture.release_read();
            };
            let (result, ()) = tokio::join!(work, interrupt);
            assert!(result.unwrap());
        } else {
            assert!(work.await.unwrap());
        }
        let mut row = issue_reply::load(&db, "1", &artifact.artifact_id)
            .await
            .unwrap()
            .unwrap();
        match case {
            0 | 7 | 8 | 12 | 14 => {
                assert_eq!(row.state, issue_reply::DELIVERED);
                assert_eq!(fixture.writes(), 1);
            }
            1 | 10 => {
                assert_eq!(row.state, issue_reply::UNKNOWN);
                assert_eq!(fixture.writes(), 1);
                if case == 10 {
                    use sea_orm::TransactionTrait;
                    let txn = db.begin().await.unwrap();
                    issue_reply::lifecycle::retire_conversation_on(
                        &txn,
                        "1",
                        "root",
                        chrono::Utc::now().timestamp_millis(),
                    )
                    .await
                    .unwrap();
                    txn.commit().await.unwrap();
                }
                assert!(
                    deliver_next_with_authority(&db, "node-b", "claim-b", async |_, _| panic!(
                        "Unknown effects may only be reconciled"
                    ))
                    .await
                    .unwrap()
                );
                row = issue_reply::load(&db, "1", &artifact.artifact_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(row.state, issue_reply::DELIVERED);
                assert_eq!(row.comment_id, Some(90));
                assert_eq!(fixture.writes(), 1);
            }
            2 | 3 | 9 | 13 | 15 => {
                assert_eq!(row.state, issue_reply::INTERNAL);
                assert_eq!(fixture.writes(), 0);
            }
            4 | 11 => {
                assert_eq!(row.state, issue_reply::NOT_SENT);
                assert_eq!(fixture.writes(), 0);
            }
            5 => {
                assert_eq!(row.state, issue_reply::CLAIMED);
                assert_eq!(row.claim_token.as_deref(), Some("new-owner"));
                assert_eq!(fixture.writes(), 0);
            }
            6 => {
                assert_eq!(row.state, issue_reply::READY);
                assert_eq!(fixture.writes(), 0);
            }
            _ => unreachable!(),
        }
        if case == 2 {
            assert_eq!(fixture.reads(), reads_before);
        }
        assert_eq!(row.request_id, artifact.origin.request_id);
        if (10..=12).contains(&case) {
            assert!(row.artifact_json.is_none());
            assert!(row.retired_at_ms.is_some());
        }

        if row.state == issue_reply::DELIVERED || row.state == issue_reply::UNKNOWN {
            assert_eq!(
                row.dispatched_authorization_revision,
                Some(if case == 0 { 1 } else { 2 })
            );
        } else {
            assert!(row.dispatched_authorization_revision.is_none());
        }
        assert_eq!(
            agent_session::Entity::find_by_id(b.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            b
        );
        fixture.stop().await;
    }
}
