use super::*;
use desk_diagnose_core::provider_preflight::ObservedCapabilityAuthority;

#[test]
fn capability_preparation_fits_production_thread_stack() {
    std::thread::Builder::new()
        .name("capability-preparation-stack".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            actix_web::rt::System::new().block_on(Box::pin(
                observed_scope_stays_exact_through_prepare_intent_completion_and_reopen(),
            ));
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn observed_scope_stays_exact_through_prepare_intent_completion_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("observed-authority.db");
    let db = file_db(&path).await;
    insert_session(&db, 1, 1).await;
    // Reproduce the persisted transcript size seen in the desktop-action crash.
    let row = agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session.append_event_if_absent("large-evidence", &"x".repeat(320 * 1024), "t1");
    let mut active: agent_session::ActiveModel = row.into();
    active.state_json = Set(session.encode_json_for_storage().unwrap());
    active.update(&db).await.unwrap();
    let store = SignalCapabilityGrantStore::new(db.clone());
    let mut broad = grant(2);
    broad.resource_scope.push("root:unused".into());
    broad.operation_scope.push("read".into());
    store.issue(&broad).await.unwrap();
    let resources = vec!["root:selected".into()];
    let operations = vec!["create_new".into()];
    let canonical_json = r#"{"path":"report.txt"}"#;
    let canonical = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
    let original = || {
        request(
            "call-observed",
            canonical_json,
            &canonical,
            &resources,
            &operations,
            1,
        )
    };
    let expected = ObservedCapabilityAuthority::from_call(&original().call);
    let prepared = store.prepare(original()).await.unwrap();
    let work = agent_action_item::Entity::find_by_id(prepared.work_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let payload: PreparedCapabilityPayload = serde_json::from_str(&work.payload_json).unwrap();
    assert_eq!(payload.observed_authority, expected);
    assert_ne!(payload.observed_authority.resources, broad.resource_scope);
    assert_ne!(payload.observed_authority.operations, broad.operation_scope);
    assert!(work.result_json.is_none());
    let changed_resources = vec!["root:unused".into()];
    let changed_operations = vec!["read".into()];
    let changed_envelopes = vec!["unexpected-envelope".into()];
    let changed_digests = vec!["a".repeat(64)];
    for case in 0..8 {
        let changed = || {
            let mut next = original();
            match case {
                0 => next.call.resource_scope = &changed_resources,
                1 => next.call.operation_scope = &changed_operations,
                2 => next.call.target_session_id = Some("different-session"),
                3 => next.call.tool_schema_version = 2,
                4 => next.call.effect = CapabilityEffect::ReadFile,
                5 => next.call.risk_tier = CapabilityRiskTier::R3,
                6 => next.call.envelope_ids = &changed_envelopes,
                _ => next.call.content_digests_sha256 = &changed_digests,
            }
            next
        };
        assert!(
            store.prepare(changed()).await.is_err(),
            "prepare case {case}"
        );
        assert!(
            store.record_dispatch_intent(changed()).await.is_err(),
            "intent case {case}"
        );
    }
    assert_eq!(
        agent_action_item::Entity::find_by_id(work.id)
            .one(&db)
            .await
            .unwrap(),
        Some(work)
    );
    assert_eq!(
        agent_capability_dispatch_outbox::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
    let mut later = original();
    later.call.now_unix_ms += 1;
    assert!(store.prepare(later).await.unwrap().idempotent_replay);
    let DispatchIntentResult::Recorded {
        dispatch_id,
        outbox_id,
        ..
    } = store.record_dispatch_intent(original()).await.unwrap()
    else {
        panic!("expected dispatch intent")
    };
    let outbox = agent_capability_dispatch_outbox::Entity::find_by_id(outbox_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let dispatched: CapabilityDispatchPayload = serde_json::from_str(&outbox.payload_json).unwrap();
    assert_eq!(dispatched.observed_authority, expected);
    let mut changed = original();
    changed.call.resource_scope = &changed_resources;
    assert!(store.record_dispatch_intent(changed).await.is_err());
    assert!(matches!(
        store.claim_dispatch(&dispatch_id, 600).await.unwrap(),
        DispatchClaimResult::Claimed(_)
    ));
    let completion = CapabilityDispatchCompletion {
        dispatch_id: dispatch_id.clone(),
        call_id: prepared.call_id,
        generation: 1,
        outcome: CapabilityDispatchOutcome::Succeeded,
        result_digest_sha256: "b".repeat(64),
    };
    store
        .record_dispatch_completion(&completion, 700)
        .await
        .unwrap();
    drop(store);
    db.close().await.unwrap();
    let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
        .await
        .unwrap();
    let work = agent_action_item::Entity::find_by_id(prepared.work_id)
        .one(&reopened)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(work.status, CAPABILITY_WORK_SUCCEEDED);
    let payload: PreparedCapabilityPayload = serde_json::from_str(&work.payload_json).unwrap();
    assert_eq!(payload.observed_authority, expected);
    let recorded: CapabilityDispatchCompletion =
        serde_json::from_str(work.result_json.as_deref().unwrap()).unwrap();
    assert_eq!(recorded, completion);
    let issued = agent_capability_grant::Entity::find()
        .one(&reopened)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(issued.remaining_uses, 1);
    let missing = serde_json::json!({"grant_id":"grant-1"});
    assert!(serde_json::from_value::<PreparedCapabilityPayload>(missing).is_err());
}
