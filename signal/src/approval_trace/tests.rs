use super::*;
use sea_orm::{ActiveModelTrait, Schema};
async fn database() -> DatabaseConnection {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    let schema = Schema::new(db.get_database_backend());
    for statement in [
        schema.create_table_from_entity(crate::entity::agent_session::Entity),
        schema.create_table_from_entity(crate::entity::agent_subagent_run::Entity),
        schema.create_table_from_entity(crate::entity::agent_run_event::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_review::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_trace::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_trace_part::Entity),
        schema.create_table_from_entity(crate::entity::agent_capability_grant::Entity),
        schema.create_table_from_entity(crate::entity::agent_action_item::Entity),
    ] {
        db.execute(&statement).await.unwrap();
    }
    db
}
async fn session(db: &DatabaseConnection, id: &str, owner: &str) {
    let mut session = PersistedAgentSession::new(
        id,
        owner,
        "device",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        "2026-10-12T00:00:00Z",
    );
    session.surface = desk_diagnose_core::session::AgentSessionSurface::AiAssistant;
    let now = chrono::Utc::now();
    agent_session::ActiveModel {
        conversation_id: Set(id.into()),
        actor_id: Set(owner.into()),
        device_id: Set("device".into()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(session.version),
        lease_token: Set(0),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}
async fn candidate(db: &DatabaseConnection, id: &str, conversation: &str, owner: &str) {
    let now = chrono::Utc::now().timestamp_millis();
    let tracking = ReviewTracking::queued(
        owner,
        "device",
        conversation,
        conversation,
        "delegation",
        1,
        "permission_item",
        id,
        now,
    )
    .settled("denied", None, now + 100);
    review::ActiveModel {
        candidate_id: Set(id.into()),
        conversation_id: Set(conversation.into()),
        actor_id: Set(owner.into()),
        device_id: Set("device".into()),
        delegation_id: Set("delegation".into()),
        model_config_revision: Set(1),
        source_kind: Set("permission_item".into()),
        source_id: Set(id.into()),
        action_sha256: Set("a".repeat(64)),
        context_hmac_sha256: Set("b".repeat(64)),
        status: Set("denied".into()),
        lease_epoch: Set(1),
        reserved_tokens: Set(0),
        expires_at: Set(now + 60000),
        created_at: Set(now),
        updated_at: Set(now),
        statistics_json: Set(Some(serde_json::to_string(&tracking).unwrap())),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}
fn identity(id: &str, conversation: &str, owner: &str) -> TraceIdentity {
    TraceIdentity {
        id: id.into(),
        owner_id: owner.into(),
        conversation_id: conversation.into(),
        root_conversation_id: conversation.into(),
        device_id: "device".into(),
        source_kind: "permission_item".into(),
        source_id: id.into(),
        lease_epoch: 1,
        model_revision: 1,
        created_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}
#[tokio::test]
async fn private_approval_details_enforce_owner_and_delete_fences() {
    let db = database().await;
    session(&db, "own", "1").await;
    session(&db, "other", "2").await;
    candidate(&db, "mine", "own", "1").await;
    candidate(&db, "theirs", "other", "2").await;
    let store = Store::new(db.clone());
    let id = identity("mine", "own", "1");
    let settings = ApprovalTraceSettings::defaults(false);
    store.begin(&id, &settings).await.unwrap();
    let part = ApprovalTracePartDto {
        sequence: 0,
        kind: "reasoning".into(),
        text: "private thought".into(),
        at_ms: 10,
    };
    store.append(&id, part.clone(), &settings).await.unwrap();
    store.append(&id, part.clone(), &settings).await.unwrap();
    assert_eq!(
        trace::Entity::find_by_id(QUOTA)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .captured_bytes,
        15
    );
    let query = ApprovalReviewQuery {
        session: "own".into(),
        id: "mine".into(),
    };
    assert_eq!(store.detail("1", &query).await.unwrap().status, "denied");
    assert!(store.detail("2", &query).await.is_err());
    assert!(
        store
            .detail(
                "1",
                &ApprovalReviewQuery {
                    session: "own".into(),
                    id: "theirs".into()
                }
            )
            .await
            .is_err()
    );
    let parts = ApprovalPartsQuery {
        session: "own".into(),
        id: "mine".into(),
        kind: "reasoning".into(),
        after: None,
    };
    assert_eq!(store.parts("1", &parts).await.unwrap().parts.len(), 1);
    assert!(store.parts("2", &parts).await.is_err());
    store.delete_for_session("own", "1").await.unwrap();
    assert!(store.append(&id, part, &settings).await.is_err());
    store
        .finish(&id, TraceCompletion::default(), "complete")
        .await
        .unwrap();
    assert_eq!(
        store.parts("1", &parts).await.unwrap().recording_state,
        "deleted"
    );
    assert_eq!(
        trace::Entity::find_by_id(QUOTA)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .captured_bytes,
        0
    );
}
#[tokio::test]
async fn approval_settings_cas_is_independent_and_shared() {
    let db = database().await;
    let mut settings = load_settings(&db).await.unwrap();
    let stale = settings.clone();
    settings.retention_days = 3;
    assert!(save_settings(&db, settings).await.unwrap().is_some());
    assert!(save_settings(&db, stale).await.unwrap().is_none());
    assert_eq!(load_settings(&db).await.unwrap().retention_days, 3);
}
#[tokio::test]
async fn approval_cursor_is_bound_to_owner_session_and_scope() {
    let db = database().await;
    session(&db, "own", "1").await;
    session(&db, "second", "1").await;
    candidate(&db, "a", "own", "1").await;
    candidate(&db, "b", "own", "1").await;
    let store = Store::new(db);
    let mut query = ApprovalReviewsQuery {
        session: "own".into(),
        include_children: Some(false),
        cursor: None,
        limit: Some(1),
    };
    let first = store.list("1", &query).await.unwrap();
    query.cursor = first.next_cursor;
    assert!(query.cursor.is_some());
    let next = store.list("1", &query).await.unwrap();
    assert_ne!(first.reviews[0].id, next.reviews[0].id);
    query.session = "second".into();
    assert!(store.list("1", &query).await.is_err());
}

#[tokio::test]
async fn approval_expiry_and_orphan_cleanup_release_quota_without_resurrecting_bodies() {
    let db = database().await;
    session(&db, "own", "1").await;
    candidate(&db, "mine", "own", "1").await;
    let store = Store::new(db.clone());
    let id = identity("mine", "own", "1");
    let settings = load_settings(&db).await.unwrap();
    store.begin(&id, &settings).await.unwrap();
    store
        .append(
            &id,
            ApprovalTracePartDto {
                sequence: 0,
                kind: "answer".into(),
                text: "reply".into(),
                at_ms: 1,
            },
            &settings,
        )
        .await
        .unwrap();
    trace::Entity::update_many()
        .col_expr(trace::Column::ExpiresAtMs, Expr::value(1))
        .filter(trace::Column::Id.eq("mine"))
        .exec(&db)
        .await
        .unwrap();
    store.cleanup().await.unwrap();
    assert_eq!(
        trace::Entity::find_by_id(QUOTA)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .captured_bytes,
        0
    );
    assert!(
        store
            .append(
                &id,
                ApprovalTracePartDto {
                    sequence: 1,
                    kind: "answer".into(),
                    text: "late".into(),
                    at_ms: 1
                },
                &settings
            )
            .await
            .is_err()
    );
    agent_session::Entity::delete_many()
        .filter(agent_session::Column::ConversationId.eq("own"))
        .exec(&db)
        .await
        .unwrap();
    store.cleanup().await.unwrap();
    assert!(
        trace::Entity::find_by_id("mine")
            .one(&db)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn unclaimed_approval_failures_paginate_by_fixed_milliseconds_and_remain_private() {
    let db = database().await;
    session(&db, "own", "1").await;
    let now = chrono::Utc::now().timestamp_millis();
    let values: Vec<_> = (0..3)
        .map(|i| {
            ReviewTracking::queued(
                "1",
                "device",
                "own",
                "own",
                "delegation",
                1,
                "permission_item",
                &i.to_string(),
                now,
            )
            .settled(
                "unavailable",
                Some(desk_diagnose_core::approval_observation::ReviewReason::Configuration),
                now + 1,
            )
        })
        .collect();
    let row = agent_run_event::ActiveModel {
        event_id: Set("event:colon".into()),
        run_id: Set("own".into()),
        event_seq: Set(1),
        input_revision: Set(1),
        kind: Set("permission_decided".into()),
        actor_id: Set(Some("1".into())),
        source_envelope_ids_json: Set("[]".into()),
        result_envelope_ids_json: Set("[]".into()),
        payload_json: Set("{}".into()),
        payload_schema_version: Set(1),
        approval_statistics_json: Set(Some(serde_json::to_string(&values).unwrap())),
        approval_statistics_at_ms: Set(Some(now)),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    let public = serde_json::to_string(&row).unwrap();
    assert!(!public.contains("approval_statistics"));
    let store = Store::new(db);
    let mut query = ApprovalReviewsQuery {
        session: "own".into(),
        include_children: Some(false),
        cursor: None,
        limit: Some(1),
    };
    let mut ids = std::collections::BTreeSet::new();
    for _ in 0..3 {
        let page = store.list("1", &query).await.unwrap();
        assert_eq!(page.reviews.len(), 1);
        let record = &page.reviews[0];
        assert_eq!(record.status, "fault");
        ids.insert(record.id.clone());
        assert_eq!(
            store
                .detail(
                    "1",
                    &ApprovalReviewQuery {
                        session: "own".into(),
                        id: record.id.clone()
                    }
                )
                .await
                .unwrap()
                .recording_state,
            "not_sent"
        );
        assert!(
            store
                .detail(
                    "2",
                    &ApprovalReviewQuery {
                        session: "own".into(),
                        id: record.id.clone()
                    }
                )
                .await
                .is_err()
        );
        query.cursor = page.next_cursor;
    }
    assert_eq!(ids.len(), 3);
    assert!(query.cursor.is_none());
}
