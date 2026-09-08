use super::*;
use desk_diagnose_core::session::TriggerOrigin;

#[tokio::test]
async fn missing_scheduled_occurrence_blocks_each_dispatch_boundary_without_advancing_outbox() {
    for boundary in 0..3 {
        let directory = tempfile::tempdir().unwrap();
        let db = file_db(&directory.path().join("scheduled.db")).await;
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(crate::entity::agent_schedule_run::Entity))
            .await
            .unwrap();
        insert_session(&db, 1, 1).await;
        let store = SignalCapabilityGrantStore::new(db.clone());
        store.issue(&grant(1)).await.unwrap();
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let json = r#"{"path":"scheduled.txt"}"#;
        let digest = format!("{:x}", Sha256::digest(json.as_bytes()));
        let current = || request("scheduled-call", json, &digest, &resources, &operations, 1);
        store.prepare(current()).await.unwrap();
        let dispatch = if boundary > 0 {
            let DispatchIntentResult::Recorded { dispatch_id, .. } =
                store.record_dispatch_intent(current()).await.unwrap()
            else {
                panic!("intent must be recorded");
            };
            if boundary == 2 {
                store.claim_dispatch(&dispatch_id, 600).await.unwrap();
            }
            Some(dispatch_id)
        } else {
            None
        };
        let before = agent_capability_dispatch_outbox::Entity::find()
            .all(&db)
            .await
            .unwrap();
        let row = agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        session.trigger_origin = TriggerOrigin::ScheduledContinuation;
        session.current_request_id = Some("missing-occurrence".into());
        session.version = row.version;
        let mut row: agent_session::ActiveModel = row.into();
        row.state_json = Set(session.encode_json_for_storage().unwrap());
        row.update(&db).await.unwrap();
        let error = match boundary {
            0 => store.record_dispatch_intent(current()).await.unwrap_err(),
            1 => store
                .claim_dispatch(dispatch.as_deref().unwrap(), 600)
                .await
                .unwrap_err(),
            _ => store
                .validate_claimed_dispatch(dispatch.as_deref().unwrap(), current())
                .await
                .unwrap_err(),
        };
        assert!(
            error.to_string().contains("scheduled action"),
            "{boundary}: {error}"
        );
        assert_eq!(
            agent_capability_dispatch_outbox::Entity::find()
                .all(&db)
                .await
                .unwrap(),
            before
        );
        db.close().await.unwrap();
    }
}
