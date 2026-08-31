use super::*;
use desk_diagnose_core::input_read_context::live_read::LiveReadTarget;
use sea_orm::PaginatorTrait;

fn live_input() -> AppendUserFollowupParams {
    let mut params = input("live-input", vec![]);
    params.current_scope.granted = vec![Capability::DocumentLiveInspect];
    let selection = params.read_context.as_mut().unwrap();
    selection.tool_names = vec!["inspect_live_document".into()];
    selection.live_targets = vec![LiveReadTarget {
        tool_name: "inspect_live_document".into(),
        object_ref: ObjectRef {
            token: "original-document".into(),
            snapshot_id: "snapshot-1".into(),
            object_kind: ObjectKind::Document,
            expires_at: (Utc::now() + chrono::Duration::minutes(2)).to_rfc3339(),
        },
        interactive_session_incarnation: "worker-1".into(),
        readiness_expires_at_unix_ms: (Utc::now() + chrono::Duration::minutes(1)).timestamp_millis()
            as u64,
    }];
    params
}

#[tokio::test]
async fn live_input_without_a_durable_selection_is_rejected_at_intake() {
    let store = setup("sqlite::memory:").await;
    assert!(store.append_user_followup(live_input()).await.is_err());
    assert!(
        agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        agent_run_event::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn original_live_target_receipt_is_immutable_even_after_expiry() {
    let store = setup("sqlite::memory:").await;
    select_live_document(&store).await;
    let params = live_input();
    let first = store.append_user_followup(params.clone()).await.unwrap();
    let row = agent_run_event::Entity::find()
        .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::UserFollowup.as_str()))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.payload_schema_version, 4);
    assert_eq!(
        store
            .original_read_context(subject(), first.input_revision)
            .await
            .unwrap(),
        params.read_context
    );
    validate(&store, &params, first.input_revision)
        .await
        .unwrap();
    let mut retry = params.clone();
    retry.created_at = (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
    let repeated = store.append_user_followup(retry).await.unwrap();
    assert_eq!(repeated.event_seq, first.event_seq);
    assert!(!repeated.newly_appended);
    for variant in 0..4 {
        let mut changed = params.clone();
        let target = &mut changed.read_context.as_mut().unwrap().live_targets[0];
        match variant {
            0 => target.object_ref.token = "other-document".into(),
            1 => target.interactive_session_incarnation = "other-worker".into(),
            2 => target.readiness_expires_at_unix_ms += 1,
            _ => target.object_ref.snapshot_id = "other-snapshot".into(),
        }
        assert!(store.append_user_followup(changed).await.is_err());
    }
    let deadline =
        params.read_context.as_ref().unwrap().live_targets[0].readiness_expires_at_unix_ms;
    assert!(
        store
            .validate_object_read(
                subject(),
                first.input_revision,
                params.read_context.as_ref().unwrap(),
                &destination(),
                deadline
            )
            .await
            .is_err()
    );
    let mut changed_model = destination();
    if let DestinationIdentity::Model {
        profile_revision, ..
    } = &mut changed_model
    {
        *profile_revision += 1;
    }
    assert!(
        store
            .validate_object_read(
                subject(),
                first.input_revision,
                params.read_context.as_ref().unwrap(),
                &changed_model,
                Utc::now().timestamp_millis() as u64
            )
            .await
            .is_err()
    );
    assert_eq!(
        agent_run_event::Entity::find()
            .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::UserFollowup.as_str()))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .payload_json,
        row.payload_json
    );
}

#[tokio::test]
async fn expired_live_input_rolls_back_and_corrupt_versions_cannot_replay() {
    let store = setup("sqlite::memory:").await;
    select_live_document(&store).await;
    let params = live_input();
    let before = state(&store).await;
    let mut expired = params.clone();
    expired.read_context.as_mut().unwrap().live_targets[0].readiness_expires_at_unix_ms = 1;
    assert!(store.append_user_followup(expired).await.is_err());
    assert_eq!(state(&store).await, before);
    assert_eq!(
        agent_run_event::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        1
    );
    let receipt = store.append_user_followup(params.clone()).await.unwrap();
    agent_run_event::Entity::update_many()
        .col_expr(
            agent_run_event::Column::PayloadSchemaVersion,
            Expr::value(3),
        )
        .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::UserFollowup.as_str()))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(store.append_user_followup(params.clone()).await.is_err());
    assert!(
        validate(&store, &params, receipt.input_revision)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn live_target_survives_sqlite_reopen_and_new_input_supersedes_it() {
    let path =
        std::env::temp_dir().join(format!("signal-live-input-{}.sqlite", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let store = setup(&url).await;
    select_live_document(&store).await;
    let params = live_input();
    let receipt = store.append_user_followup(params.clone()).await.unwrap();
    store.db.close().await.unwrap();
    let reopened = setup(&url).await;
    assert_eq!(
        reopened
            .original_read_context(subject(), receipt.input_revision)
            .await
            .unwrap(),
        params.read_context
    );
    let mut next = params.clone();
    next.event_id = "next-input".into();
    next.message.message_id = "next-input".into();
    next.read_context.as_mut().unwrap().tool_names.clear();
    next.read_context.as_mut().unwrap().live_targets.clear();
    reopened.append_user_followup(next).await.unwrap();
    assert!(
        validate(&reopened, &params, receipt.input_revision)
            .await
            .is_err()
    );
    reopened.db.close().await.unwrap();
    std::fs::remove_file(path).unwrap();
}
