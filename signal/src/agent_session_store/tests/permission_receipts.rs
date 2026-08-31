//! Original decision replay through production event/session/grant storage.
use super::*;
use actix_web::web;

mod http;

async fn seed(db: DatabaseConnection) -> (SignalAgentSessionStore, Vec<PermissionDecisionItem>) {
    crate::db::initialize_schema(&db).await.unwrap();
    let store = SignalAgentSessionStore::new(db).with_client_metadata(
        Some("client-conversation-1".into()),
        AgentSessionSurface::DeviceAssistant,
    );
    SignalAgentRunEventStore::new(store.db.clone())
        .append_user_followup(followup("input-1", "user-1", "inspect the target"))
        .await
        .unwrap();
    let mut session = store.claim_turn(claim("permission-turn")).await.unwrap();
    let request = PermissionRequest {
        schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
        request_id: "permission-1".into(),
        input_revision: 1,
        state: PermissionRequestState::Pending,
        items: ["first", "second"]
            .into_iter()
            .map(|id| GrantRequestItem {
                item_id: id.into(),
                provider_id: "desktop.session".into(),
                tool_name: "inspect_desktop_session".into(),
                expected_effect: CapabilityEffect::ReadDevice,
                resource_scope: vec!["target:device-1".into()],
                operation_scope: vec!["observe".into()],
                export_destinations: vec![],
                canonical_input_json: None,
                canonical_input_digest_sha256: None,
                suggested_ttl_seconds: 300,
                suggested_max_uses: 2,
                reason: "Inspect selected target".into(),
            })
            .collect(),
        created_at: Utc::now().to_rfc3339(),
    };
    session.add_permission_request(request.clone()).unwrap();
    session.last_event_seq += 1;
    let event = PermissionRequestedEvent {
        event: AgentRunEvent {
            schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
            event_id: "requested-1".into(),
            run_id: session.conversation_id.clone(),
            event_seq: session.last_event_seq,
            input_revision: 1,
            kind: AgentRunEventKind::PermissionRequested,
            correlation_id: Some(request.request_id.clone()),
            source_envelope_ids: vec![],
            result_envelope_ids: vec![],
            created_at: request.created_at.clone(),
        },
        request,
    };
    store
        .save_permission_request(&mut session, &event)
        .await
        .unwrap();
    session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
    store.save(&mut session).await.unwrap();
    let decisions = vec![
        PermissionDecisionItem {
            item_id: "first".into(),
            decision: PermissionItemDecision::Approve {
                resource_scope: vec!["target:device-1".into()],
                operation_scope: vec!["observe".into()],
                export_destinations: vec![],
                ttl_seconds: 120,
                max_uses: 1,
            },
        },
        PermissionDecisionItem {
            item_id: "second".into(),
            decision: PermissionItemDecision::Deny,
        },
    ];
    (store, decisions)
}

async fn decide(
    store: &SignalAgentSessionStore,
    decisions: &[PermissionDecisionItem],
    ready: bool,
) -> Result<PermissionDecisionOutcome, AgentError> {
    let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
    let inventory = [CapabilityAvailability {
        provider_id: "desktop.session".into(),
        capability_id: "desktop.session.inspect".into(),
        tool_name: "inspect_desktop_session".into(),
        compiled: true,
        enabled: true,
        connected: ready,
        ready,
        reason: None,
    }];
    store
        .decide_permission_request(
            "conversation-1",
            "1",
            "device-1",
            "permission-1",
            decisions.to_vec(),
            PermissionGrantIssuanceContext {
                surface: ProductSurface::OssPersonalOwner,
                registry: &registry,
                inventory: &inventory,
                readiness_revision: u64::from(ready),
                now_unix_ms: if ready { 1_000 } else { 999_999 },
                implicit_fresh_object_refs: &[],
            },
            &Utc::now().to_rfc3339(),
        )
        .await
}

async fn replay(
    store: &SignalAgentSessionStore,
    decisions: &[PermissionDecisionItem],
) -> Result<Option<PermissionRequestState>, AgentError> {
    store
        .replay_permission_decision("conversation-1", "1", "device-1", "permission-1", decisions)
        .await
}

async fn state(
    store: &SignalAgentSessionStore,
) -> (
    Vec<agent_session::Model>,
    Vec<agent_run_event::Model>,
    Vec<agent_capability_grant::Model>,
) {
    (
        agent_session::Entity::find()
            .order_by_asc(agent_session::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
        agent_run_event::Entity::find()
            .order_by_asc(agent_run_event::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
        agent_capability_grant::Entity::find()
            .order_by_asc(agent_capability_grant::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn original_decision_replays_after_revoke_expiry_new_input_and_active_turn_without_writes() {
    let (store, mut decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
    assert_eq!(replay(&store, &decisions).await.unwrap(), None);
    let first = decide(&store, &decisions, true).await.unwrap();
    assert!(first.newly_recorded);
    assert_eq!(first.state, PermissionRequestState::PartiallyApproved);
    decisions.reverse();
    let saved = state(&store).await;
    assert_eq!(replay(&store, &decisions).await.unwrap(), Some(first.state));
    assert_eq!(state(&store).await, saved);
    let grant = &saved.2[0];
    crate::capability_grant_store::SignalCapabilityGrantStore::new(store.db.clone())
        .revoke(
            &grant.grant_id,
            "1",
            "device-1",
            2_000,
            "Owner revoked access",
        )
        .await
        .unwrap();
    SignalAgentRunEventStore::new(store.db.clone())
        .append_user_followup(followup("input-2", "user-2", "do something else"))
        .await
        .unwrap();
    store.claim_turn(claim("new-turn")).await.unwrap();
    let saved = state(&store).await;
    let replayed = decide(&store, &decisions, false).await.unwrap();
    assert!(!replayed.newly_recorded);
    assert_eq!(replayed.state, first.state);
    assert_eq!(replay(&store, &decisions).await.unwrap(), Some(first.state));
    assert_eq!(state(&store).await, saved);
}

#[tokio::test]
async fn changed_or_incomplete_decisions_and_foreign_subjects_cannot_replay() {
    let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
    decide(&store, &decisions, true).await.unwrap();
    let saved = state(&store).await;
    for change in [
        "deny",
        "scope",
        "operation",
        "ttl",
        "uses",
        "duplicate",
        "missing",
    ] {
        let mut changed = decisions.clone();
        if change == "deny" {
            changed[0].decision = PermissionItemDecision::Deny;
        } else if change == "duplicate" {
            changed[1] = changed[0].clone();
        } else if change == "missing" {
            changed.pop();
        } else if let PermissionItemDecision::Approve {
            resource_scope,
            operation_scope,
            ttl_seconds,
            max_uses,
            ..
        } = &mut changed[0].decision
        {
            match change {
                "scope" => resource_scope.clear(),
                "operation" => operation_scope.clear(),
                "ttl" => *ttl_seconds = 60,
                "uses" => *max_uses = 2,
                _ => unreachable!(),
            }
        }
        assert!(replay(&store, &changed).await.is_err(), "{change}");
        assert!(decide(&store, &changed, true).await.is_err(), "{change}");
    }
    for (run, actor, device) in [
        ("other-run", "1", "device-1"),
        ("conversation-1", "other", "device-1"),
        ("conversation-1", "1", "other-device"),
    ] {
        assert!(
            store
                .replay_permission_decision(run, actor, device, "permission-1", &decisions)
                .await
                .is_err()
        );
    }
    assert_eq!(state(&store).await, saved);
}

#[tokio::test]
async fn damaged_or_missing_original_facts_never_manufacture_a_receipt() {
    for change in [
        "version",
        "actor",
        "sequence",
        "sources",
        "timestamp",
        "payload",
        "state",
        "requested",
        "missing",
        "duplicate",
        "subject_column",
    ] {
        let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
        decide(&store, &decisions, true).await.unwrap();
        let saved = state(&store).await;
        let row = saved
            .1
            .iter()
            .find(|row| row.kind == "permission_decided")
            .unwrap();
        if change == "missing" {
            agent_run_event::Entity::delete_by_id(row.id)
                .exec(&store.db)
                .await
                .unwrap();
        } else if change == "requested" {
            agent_run_event::Entity::delete_many()
                .filter(agent_run_event::Column::Kind.eq("permission_requested"))
                .exec(&store.db)
                .await
                .unwrap();
        } else if change == "subject_column" {
            agent_session::Entity::update_many()
                .col_expr(agent_session::Column::DeviceId, Expr::value("other-device"))
                .exec(&store.db)
                .await
                .unwrap();
        } else {
            let mut changed: agent_run_event::ActiveModel = row.clone().into();
            match change {
                "version" => changed.payload_schema_version = Set(99),
                "actor" => changed.actor_id = Set(Some("other".into())),
                "sequence" => changed.input_revision = Set(99),
                "sources" => changed.source_envelope_ids_json = Set("[\"other\"]".into()),
                "timestamp" => changed.created_at = Set(Utc::now() + Duration::hours(1)),
                "payload" => changed.payload_json = Set("{}".into()),
                "state" => {
                    changed.payload_json =
                        Set(row.payload_json.replace("partially_approved", "approved"))
                }
                "duplicate" => {
                    changed.id = sea_orm::ActiveValue::NotSet;
                    changed.event_id = Set("duplicate".into());
                    changed.event_seq = Set(99);
                }
                _ => unreachable!(),
            }
            if change == "duplicate" {
                changed.insert(&store.db).await.unwrap();
            } else {
                changed.update(&store.db).await.unwrap();
            }
        }
        let corrupted = state(&store).await;
        if change == "missing" {
            assert_eq!(replay(&store, &decisions).await.unwrap(), None);
        } else {
            assert!(replay(&store, &decisions).await.is_err(), "{change}");
        }
        assert!(decide(&store, &decisions, true).await.is_err(), "{change}");
        assert_eq!(state(&store).await, corrupted);
    }
}

#[tokio::test]
async fn decision_receipt_and_grants_rollback_together() {
    for table in ["agent_run_event", "agent_capability_grant"] {
        let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
        store.db.execute_unprepared(&format!("CREATE TRIGGER reject_decision BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END")).await.unwrap();
        let saved = state(&store).await;
        assert!(decide(&store, &decisions, true).await.is_err());
        assert_eq!(state(&store).await, saved);
        assert_eq!(replay(&store, &decisions).await.unwrap(), None);
        store
            .db
            .execute_unprepared("DROP TRIGGER reject_decision")
            .await
            .unwrap();
        assert!(
            decide(&store, &decisions, true)
                .await
                .unwrap()
                .newly_recorded
        );
    }
}

#[tokio::test]
async fn independent_sqlite_pools_and_reopen_keep_one_original_decision() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("permissions.db").display()
    );
    let (first, decisions) = seed(Database::connect(&url).await.unwrap()).await;
    let second = SignalAgentSessionStore::new(Database::connect(&url).await.unwrap());
    let (left, right) = tokio::join!(
        decide(&first, &decisions, true),
        decide(&second, &decisions, true)
    );
    assert!(left.is_ok() || right.is_ok());
    assert_eq!(
        [left, right]
            .iter()
            .filter(|result| result.as_ref().is_ok_and(|outcome| outcome.newly_recorded))
            .count(),
        1
    );
    let saved = state(&first).await;
    assert_eq!(saved.2.len(), 1);
    assert_eq!(
        saved
            .1
            .iter()
            .filter(|row| row.kind == "permission_decided")
            .count(),
        1
    );
    assert!(
        !decide(&second, &decisions, false)
            .await
            .unwrap()
            .newly_recorded
    );
    first.db.close().await.unwrap();
    second.db.close().await.unwrap();
    let reopened = SignalAgentSessionStore::new(Database::connect(&url).await.unwrap());
    assert_eq!(
        replay(&reopened, &decisions).await.unwrap(),
        Some(PermissionRequestState::PartiallyApproved)
    );
    assert!(
        !decide(&reopened, &decisions, false)
            .await
            .unwrap()
            .newly_recorded
    );
    assert_eq!(state(&reopened).await, saved);
}

#[tokio::test]
async fn conflicting_first_decisions_from_independent_pools_keep_only_the_winner() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("conflict.db").display()
    );
    let (first, decisions) = seed(Database::connect(&url).await.unwrap()).await;
    let second = SignalAgentSessionStore::new(Database::connect(&url).await.unwrap());
    let mut different = decisions.clone();
    if let PermissionItemDecision::Approve { ttl_seconds, .. } = &mut different[0].decision {
        *ttl_seconds = 60;
    }
    let (left, right) = tokio::join!(
        decide(&first, &decisions, true),
        decide(&second, &different, true)
    );
    assert_ne!(left.is_ok(), right.is_ok());
    let (winner, loser) = if left.is_ok() {
        (&decisions, &different)
    } else {
        (&different, &decisions)
    };
    let saved = state(&first).await;
    assert_eq!(saved.2.len(), 1);
    assert_eq!(
        saved
            .1
            .iter()
            .filter(|row| row.kind == "permission_decided")
            .count(),
        1
    );
    assert!(!decide(&second, winner, false).await.unwrap().newly_recorded);
    assert!(decide(&first, loser, true).await.is_err());
    assert_eq!(state(&first).await, saved);
}
