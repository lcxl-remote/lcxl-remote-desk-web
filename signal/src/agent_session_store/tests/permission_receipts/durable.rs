use super::*;
use crate::entity::agent_permission_resume as resume;

async fn candidate(store: &SignalAgentSessionStore) -> resume::Model {
    resume::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
}

async fn prepare(store: &SignalAgentSessionStore) -> (SignalAgentSessionStore, ClaimTurnParams) {
    let candidate = candidate(store).await;
    let session = store
        .pending_permission_resume(&candidate, Utc::now())
        .await
        .unwrap()
        .unwrap();
    let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(store.db.clone())
        .list_for_subject("conversation-1", "1", "device-1")
        .await
        .unwrap();
    let claimant = store
        .clone()
        .with_client_metadata(session.client_conversation_id.clone(), session.surface)
        .with_expected_input_revision(session.input_revision)
        .with_permission_resume(candidate.request_id.clone(), session.version, grants);
    let mut params = claim(&candidate.permission_id);
    params.trigger_origin = TriggerOrigin::PermissionDecision;
    params.request_id = None;
    params.connection_id = None;
    (claimant, params)
}

#[tokio::test]
async fn abrupt_process_exit_preserves_pending_and_started_fences() {
    const CHILD_DATABASE: &str = "LRD_TEST_PERMISSION_RESUME_DATABASE";
    const CHILD_PHASE: &str = "LRD_TEST_PERMISSION_RESUME_PHASE";
    if let Ok(url) = std::env::var(CHILD_DATABASE) {
        let (store, decisions) = seed(Database::connect(&url).await.unwrap()).await;
        decide(&store, &decisions, true).await.unwrap();
        if std::env::var(CHILD_PHASE).unwrap() == "started" {
            let (claimant, params) = prepare(&store).await;
            claimant.claim_turn(params).await.unwrap();
            // Advance the lease boundary without delaying the subprocess test.
            agent_session::Entity::update_many()
                .col_expr(
                    agent_session::Column::LeaseDeadline,
                    Expr::value(Some(Utc::now() - Duration::minutes(2))),
                )
                .exec(&store.db)
                .await
                .unwrap();
        }
        // Exit without pool close, runtime teardown, or Rust destructors.
        std::process::exit(73);
    }

    let test_name = format!(
        "{}::abrupt_process_exit_preserves_pending_and_started_fences",
        module_path!().split_once("::").unwrap().1
    );
    for phase in ["pending", "started"] {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("resume.db").display()
        );
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &test_name, "--nocapture"])
            .env(CHILD_DATABASE, &url)
            .env(CHILD_PHASE, phase)
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(20), command.output())
            .await
            .expect("permission resume subprocess exceeded its deadline")
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "{phase}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let store = SignalAgentSessionStore::new(Database::connect(&url).await.unwrap());
        crate::db::initialize_schema(&store.db).await.unwrap();
        let original = candidate(&store).await;
        let original_grants = state(&store).await.2;
        assert_eq!(original.state, phase);
        assert_eq!(original_grants.len(), 1);
        if phase == "pending" {
            let (claimant, params) = prepare(&store).await;
            claimant.claim_turn(params.clone()).await.unwrap();
            assert!(claimant.claim_turn(params).await.is_err());
            assert_eq!(candidate(&store).await.state, "started");
        } else {
            assert!(
                store
                    .pending_permission_resume(&original, Utc::now())
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(candidate(&store).await.state, "settled");
            assert!(
                store
                    .permission_resume_candidates(0, 32)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let row = find(&store.db, "conversation-1").await.unwrap().unwrap();
            let session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
            assert!(!session.turn_state.is_active());
            assert_eq!(session.current_turn_id, original.turn_id);
        }
        assert_eq!(state(&store).await.2, original_grants);
        store.db.close().await.unwrap();
    }
}

#[tokio::test]
async fn decide_reopen_concurrent_claim_and_settlement_never_reclaim_a_started_resume() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("resume.db").display()
    );
    let (store, decisions) = seed(Database::connect(&url).await.unwrap()).await;
    decide(&store, &decisions, true).await.unwrap();
    let pending = candidate(&store).await;
    assert_eq!(pending.state, "pending");
    assert_eq!(pending.turn_id, None);
    store.db.close().await.unwrap();
    let first = SignalAgentSessionStore::new(Database::connect(&url).await.unwrap());
    let second = SignalAgentSessionStore::new(Database::connect(&url).await.unwrap());
    let (a, ca) = prepare(&first).await;
    let (b, cb) = prepare(&second).await;
    let (left, right) = tokio::join!(a.claim_turn(ca.clone()), b.claim_turn(cb));
    assert_ne!(left.is_ok(), right.is_ok());
    let mut session = left.or(right).unwrap();
    assert_eq!(
        session.current_turn_id.as_deref(),
        Some(pending.permission_id.as_str())
    );
    assert_eq!(candidate(&first).await.state, "started");
    assert!(
        first
            .pending_permission_resume(&pending, Utc::now())
            .await
            .unwrap()
            .is_none()
    );
    assert!(a.claim_turn(ca.clone()).await.is_err());
    session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
    first.save(&mut session).await.unwrap();
    first
        .pending_permission_resume(&pending, Utc::now())
        .await
        .unwrap();
    assert_eq!(candidate(&first).await.state, "settled");
    let saved = state(&first).await;
    assert!(
        !decide(&second, &decisions, false)
            .await
            .unwrap()
            .newly_recorded
    );
    assert!(a.claim_turn(ca).await.is_err());
    assert_eq!(state(&first).await, saved);
    assert!(
        first
            .permission_resume_candidates(0, 32)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn claim_and_resume_marker_rollback_together() {
    let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
    decide(&store, &decisions, true).await.unwrap();
    let (claimant, params) = prepare(&store).await;
    let pending = candidate(&store).await;
    let saved = state(&store).await;
    store.db.execute_unprepared("CREATE TRIGGER reject_claim BEFORE UPDATE ON agent_permission_resume BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END").await.unwrap();
    assert!(claimant.claim_turn(params.clone()).await.is_err());
    assert_eq!(state(&store).await, saved);
    assert_eq!(candidate(&store).await, pending);
    store
        .db
        .execute_unprepared("DROP TRIGGER reject_claim")
        .await
        .unwrap();
    assert!(claimant.claim_turn(params).await.is_ok());
}

#[tokio::test]
async fn new_input_supersedes_pending_but_preserves_original_receipt() {
    let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
    decide(&store, &decisions, true).await.unwrap();
    let (claimant, params) = prepare(&store).await;
    let pending = candidate(&store).await;
    SignalAgentRunEventStore::new(store.db.clone())
        .append_user_followup(followup("new-input", "new-user", "replace the task"))
        .await
        .unwrap();
    let saved = state(&store).await;
    assert!(claimant.claim_turn(params).await.is_err());
    assert_eq!(candidate(&store).await.state, "superseded");
    assert!(
        store
            .pending_permission_resume(&pending, Utc::now())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        replay(&store, &decisions).await.unwrap(),
        Some(PermissionRequestState::PartiallyApproved)
    );
    assert_eq!(state(&store).await, saved);
}

#[tokio::test]
async fn old_candidate_cannot_recover_a_new_input_turn_even_if_its_lease_lapsed() {
    let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
    decide(&store, &decisions, true).await.unwrap();
    let pending = candidate(&store).await;
    SignalAgentRunEventStore::new(store.db.clone())
        .append_user_followup(followup("new-input", "new-user", "new task"))
        .await
        .unwrap();
    store.claim_turn(claim("new-turn")).await.unwrap();
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::LeaseDeadline,
            Expr::value(Some(Utc::now() - Duration::minutes(2))),
        )
        .exec(&store.db)
        .await
        .unwrap();
    let saved = state(&store).await;
    assert!(
        store
            .pending_permission_resume(&pending, Utc::now())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(candidate(&store).await.state, "superseded");
    assert_eq!(state(&store).await, saved);
}

#[tokio::test]
async fn revoke_during_preflight_and_wrong_claim_identity_cannot_take_the_resume() {
    let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
    decide(&store, &decisions, true).await.unwrap();
    let (claimant, params) = prepare(&store).await;
    for change in ["turn", "actor", "device", "trigger", "unbound"] {
        let mut changed = params.clone();
        match change {
            "turn" => changed.turn_id = "another-turn".into(),
            "actor" => changed.actor_id = "other".into(),
            "device" => changed.device_id = "other".into(),
            "trigger" => changed.trigger_origin = TriggerOrigin::User,
            "unbound" => {}
            _ => unreachable!(),
        }
        let result = if change == "unbound" {
            store.claim_turn(changed).await
        } else {
            claimant.claim_turn(changed).await
        };
        assert!(result.is_err(), "{change}");
    }
    let grants = state(&store).await.2;
    crate::capability_grant_store::SignalCapabilityGrantStore::new(store.db.clone())
        .revoke(&grants[0].grant_id, "1", "device-1", 2_000, "Owner revoked")
        .await
        .unwrap();
    let saved = state(&store).await;
    assert!(claimant.claim_turn(params).await.is_err());
    assert_eq!(candidate(&store).await.state, "pending");
    assert_eq!(state(&store).await, saved);
}

#[tokio::test]
async fn process_loss_after_claim_recovers_the_original_lease_without_repeating_model_work() {
    let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
    decide(&store, &decisions, true).await.unwrap();
    let (claimant, params) = prepare(&store).await;
    let started = claimant.claim_turn(params.clone()).await.unwrap();
    let pending = candidate(&store).await;
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::LeaseDeadline,
            Expr::value(Some(Utc::now() - Duration::minutes(2))),
        )
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        store
            .pending_permission_resume(&pending, Utc::now())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(candidate(&store).await.state, "settled");
    let row = find(&store.db, "conversation-1").await.unwrap().unwrap();
    let recovered = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert!(!recovered.turn_state.is_active());
    assert_eq!(recovered.current_turn_id, started.current_turn_id);
    assert!(claimant.claim_turn(params).await.is_err());
    assert_eq!(state(&store).await.2.len(), 1);
}

#[tokio::test]
async fn schema_upgrade_keeps_old_decisions_readable_without_inventing_pending_work() {
    let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
    decide(&store, &decisions, true).await.unwrap();
    let saved = state(&store).await;
    store
        .db
        .execute_unprepared("DROP TABLE agent_permission_resume; PRAGMA user_version = 9")
        .await
        .unwrap();
    crate::db::initialize_schema(&store.db).await.unwrap();
    crate::db::initialize_schema(&store.db).await.unwrap();
    assert_eq!(state(&store).await, saved);
    assert!(
        store
            .permission_resume_candidates(0, 32)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !decide(&store, &decisions, false)
            .await
            .unwrap()
            .newly_recorded
    );
    assert!(
        store
            .permission_resume_candidates(0, 32)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .db
        .execute_unprepared("ALTER TABLE agent_permission_resume DROP COLUMN decision_event_id")
        .await
        .unwrap();
    assert!(crate::db::initialize_schema(&store.db).await.is_err());
}

#[tokio::test]
async fn corrupted_resume_metadata_is_not_a_claim_or_recovery_authority() {
    for change in ["event", "revision", "actor", "run", "state", "turn"] {
        let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
        decide(&store, &decisions, true).await.unwrap();
        let (claimant, params) = prepare(&store).await;
        let mut row: resume::ActiveModel = candidate(&store).await.into();
        match change {
            "event" => row.decision_event_id = Set("other-event".into()),
            "revision" => row.input_revision = Set(99),
            "actor" => row.actor_id = Set("other".into()),
            "run" => row.run_id = Set("other-run".into()),
            "state" => row.state = Set("unknown".into()),
            "turn" => row.turn_id = Set(Some("other-turn".into())),
            _ => unreachable!(),
        }
        row.update(&store.db).await.unwrap();
        let saved = state(&store).await;
        assert!(
            store
                .pending_permission_resume(&candidate(&store).await, Utc::now())
                .await
                .is_err(),
            "{change}"
        );
        assert!(claimant.claim_turn(params).await.is_err(), "{change}");
        assert_eq!(state(&store).await, saved);
    }
}
