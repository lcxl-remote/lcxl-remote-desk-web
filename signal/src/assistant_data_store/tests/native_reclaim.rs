use super::retrieval_native::Frame;
use super::*;
use crate::entity::{
    agent_assistant_history as history, agent_schedule as timer, agent_session as session_row,
};
use sea_orm::{ActiveModelTrait, sea_query::Expr};

#[tokio::test]
async fn native_reclaim_retains_the_actual_expired_timer_state_before_execution_gc() {
    let mut frame = Frame::new("native-reclaim-snapshot", true).await;
    crate::db::ensure_lifecycle_tables(&frame.store.db).await;
    frame.session.turn_state = desk_diagnose_core::session::TurnState::Idle;
    let cutoff = chrono::Utc::now() - chrono::Duration::days(1);
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::StateJson,
            Expr::value(frame.session.encode_json_for_storage().unwrap()),
        )
        .col_expr(
            session_row::Column::LeaseDeadline,
            Expr::value(Option::<chrono::NaiveDateTime>::None),
        )
        .col_expr(
            session_row::Column::UpdatedAt,
            Expr::value((cutoff - chrono::Duration::days(1)).naive_utc()),
        )
        .filter(session_row::Column::ConversationId.eq(&frame.session.conversation_id))
        .exec(&frame.store.db)
        .await
        .unwrap();
    let at = chrono::Utc::now().timestamp_millis();
    timer::ActiveModel {
        schedule_id: Set("native-expiring-timer".into()),
        owner_user_id: Set(1),
        target_device_id: Set(frame.session.device_id.clone()),
        kind: Set("conversation_resume".into()),
        status: Set("active".into()),
        title: Set("Continue".into()),
        prompt: Set("Continue".into()),
        spec_json: Set(
            r#"{"schema_version":1,"rule":{"kind":"once","at":"2030-01-01T00:00:00Z"}}"#.into(),
        ),
        calc_version: Set(desk_diagnose_core::schedule::SCHEDULE_CALC_VERSION.into()),
        revision: Set(1),
        task_revision: Set(1),
        next_run_at: Set(Some(at + 86400000)),
        grace_seconds: Set(60),
        creation_source: Set("manual".into()),
        creation_identity: Set("native-expiry-identity".into()),
        creation_payload_sha256: Set("0".repeat(64)),
        source_conversation_id: Set(Some(frame.session.conversation_id.clone())),
        requirement_revision: Set(Some(1)),
        failure_state_json: Set(serde_json::to_string(
            &desk_diagnose_core::schedule::lifecycle::FailureState::default(),
        )
        .unwrap()),
        created_at: Set(at),
        updated_at: Set(at),
        ..Default::default()
    }
    .insert(&frame.store.db)
    .await
    .unwrap();
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&frame.session.conversation_id))
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    // A second real owner input exercises one final quota phase for a
    // multi-root SQLite cleanup batch, without copying the first root's grants.
    let mut second = desk_diagnose_core::session::PersistedAgentSession::new(
        "native-reclaim-second",
        "1",
        frame.session.device_id.clone(),
        frame.session.policy_revision,
        frame.session.scope_snapshot.clone(),
        &chrono::Utc::now().to_rfc3339(),
    );
    second.surface = desk_diagnose_core::session::AgentSessionSurface::AiAssistant;
    second.version = 1;
    second.input_revision = 1;
    second.latest_input_seq = 0;
    second.begin_focus_epoch(1, Vec::new()).unwrap();
    second.conversation.push(
        desk_diagnose_core::model_message_labels::model_bound_user_message(
            "second-owner-input".into(),
            "Continue the second task".into(),
            frame
                .session
                .conversation
                .iter()
                .filter_map(|message| message.data_envelope.as_ref())
                .flat_map(|envelope| &envelope.allowed_destinations)
                .find(|destination| {
                    matches!(
                        destination,
                        desk_agent_protocol::data_lineage::DestinationIdentity::Model { .. }
                    )
                })
                .unwrap()
                .clone(),
        )
        .unwrap(),
    );
    let txn = frame.store.db.begin().await.unwrap();
    super::super::accept_input_on(&txn, &mut second, "UTC", None, frame.now)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let mut second_row: session_row::ActiveModel = row.clone().into();
    second_row.id = sea_orm::NotSet;
    second_row.conversation_id = Set(second.conversation_id.clone());
    second_row.state_json = Set(second.encode_json_for_storage().unwrap());
    second_row.lease_token = Set(second.lease_token as i64);
    second_row.insert(&frame.store.db).await.unwrap();
    let mut second_timer: timer::ActiveModel = timer::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap()
        .into();
    second_timer.id = sea_orm::NotSet;
    second_timer.schedule_id = Set("native-second-timer".into());
    second_timer.creation_identity = Set("native-second-identity".into());
    second_timer.source_conversation_id = Set(Some(second.conversation_id.clone()));
    second_timer.insert(&frame.store.db).await.unwrap();
    let quota_before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        crate::usage_retention::cleanup_agent_sessions(&frame.store.db, cutoff, chrono::Utc::now())
            .await
            .unwrap()
            .1,
        2
    );
    assert!(
        session_row::Entity::find_by_id(row.id)
            .one(&frame.store.db)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        session_row::Entity::find()
            .all(&frame.store.db)
            .await
            .unwrap()
            .is_empty()
    );
    let quota_after = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    // Each admitted unit performs one original quota CAS inside the
    // shared final write phase; both roots are prepared before that phase.
    assert_eq!(quota_after.revision, quota_before.revision + 2);
    let second_history = history::Entity::find()
        .filter(history::Column::RootConversationId.eq(&second.conversation_id))
        .filter(history::Column::Kind.eq("native_status"))
        .all(&frame.store.db)
        .await
        .unwrap();
    assert!(
        second_history
            .iter()
            .any(|row| row.content.contains("native-second-timer")
                && row.content.contains("expired"))
    );
    let timer = timer::Entity::find()
        .filter(timer::Column::ScheduleId.eq("native-expiring-timer"))
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(timer.status, "expired");
    let snapshots = history::Entity::find()
        .filter(history::Column::Kind.eq("native_status"))
        .filter(history::Column::RootConversationId.eq(&frame.session.conversation_id))
        .all(&frame.store.db)
        .await
        .unwrap();
    assert!(
        !snapshots.is_empty(),
        "the real original GC must retain its final observed state"
    );
    assert!(snapshots.iter().any(
        |row| row.content.contains("native-expiring-timer") && row.content.contains("expired")
    ));
    assert!(
        snapshots
            .iter()
            .all(|row| !row.content.contains("List my task state"))
    );
}
