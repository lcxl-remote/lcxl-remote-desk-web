use super::*;
mod background;
mod completion;
mod wire;
use crate::capability_grant_store::computer_binding::{
    AcceptanceOutcome, ComputerAcceptance, ComputerBinding,
};
use desk_agent_protocol::{
    computer_use::{
        COMPUTER_USE_SCHEMA_VERSION, ComputerActionKind, ComputerActionStartDisposition,
        ComputerActionStarted, ComputerActionStep, ComputerUseAdapterKind, ComputerUseAdapterRef,
        ObjectKind, ObjectRef, SealedComputerActionPlan,
    },
    data_lineage::{DestinationIdentity, Sensitivity},
};
use desk_diagnose_core::{
    chat::{ChatMessage, ToolCall, ToolCallRef},
    provider_preflight::{BrowserCallPreflight, ProviderCallSubject},
    session::AgentSessionSurface,
};

struct Fixture {
    store: SignalCapabilityGrantStore,
    session: PersistedAgentSession,
    call: ToolCall,
    plan: SealedComputerActionPlan,
    started: ComputerActionStarted,
}

impl Fixture {
    async fn new(db: DatabaseConnection) -> Self {
        insert_session(&db, 1, 1).await;
        let mut row = agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        session.policy_revision =
            desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
        session.surface = AgentSessionSurface::DeviceAssistant;
        session
            .begin_turn(
                "turn-1",
                None,
                None,
                1,
                session.scope_snapshot.clone(),
                "now",
            )
            .unwrap();
        let call = ToolCall {
            id: "model-call-original".into(),
            name: "browser_open_page".into(),
            arguments_json: r#"{ "target": { "url": "https://example.test/approved", "origin": { "kind": "https", "host_ascii": "example.test", "port": 443 } } }"#.into(),
        };
        let mut user = desk_diagnose_core::model_message_labels::model_bound_user_message(
            "input-1".into(),
            "private test input".into(),
            DestinationIdentity::Model {
                connection_id: "model-1".into(),
                connection_revision: 1,
                model_id: "test".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        user.data_envelope.as_mut().unwrap().sensitivity = Sensitivity::Secret;
        let mut proposal = ChatMessage::assistant_tool_calls(
            "proposal-1",
            "open approved page",
            vec![ToolCallRef {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            }],
        );
        proposal.data_envelope =
            desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                user.data_envelope.as_ref(),
                &call.id,
                &proposal.text,
                "test_model_output",
            )
            .unwrap();
        proposal.turn_id = Some("turn-1".into());
        session.conversation.extend([user, proposal]);
        row.state_json = session.encode_json_for_storage().unwrap();
        row.lease_token = i64::try_from(session.lease_token).unwrap();
        let active: agent_session::ActiveModel = row.into();
        active.reset_all().update(&db).await.unwrap();

        let now = u64::try_from(Utc::now().timestamp_millis()).unwrap();
        let expires_at = (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        let surface = ObjectRef {
            token: "selected-browser".into(),
            snapshot_id: "snapshot-1".into(),
            object_kind: ObjectKind::BrowserSurface,
            expires_at: expires_at.clone(),
        };
        let call_id = stable_id("capability-call", &format!("run-1:turn-1:{}", call.id));
        let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
        let evaluated = BrowserCallPreflight::build(
            &registry,
            ProductSurface::OssPersonalOwner,
            &call,
            &call_id,
            &surface,
            now,
        )
        .unwrap();
        let subject = ProviderCallSubject {
            actor_id: "actor-1",
            run_id: "run-1",
            target_device_id: "device-1",
            policy_revision:
                desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
            readiness_revision: 9,
            now_unix_ms: now,
        };
        let authority = evaluated.grant_call(&subject).unwrap();
        let mut permission = grant(1);
        permission.policy_revision = subject.policy_revision;
        permission.target_session_id = authority.target_session_id.map(str::to_owned);
        permission.provider_id = authority.provider_id.into();
        permission.capability_id = authority.capability_id.into();
        permission.tool_name = authority.tool_name.into();
        permission.effect = authority.effect;
        permission.risk_tier = authority.risk_tier;
        permission.resource_scope = authority.resource_scope.to_vec();
        permission.operation_scope = authority.operation_scope.to_vec();
        permission.issued_at_unix_ms = now - 1;
        permission.expires_at_unix_ms = now + 300_000;
        let store = SignalCapabilityGrantStore::new(db);
        store.issue(&permission).await.unwrap();
        let request = || PrepareCapabilityCall {
            grant_id: "grant-1",
            call_id: &call_id,
            turn_id: "turn-1",
            input_revision: 1,
            input_watermark: 1,
            generation: 1,
            canonical_input_json: evaluated.canonical_input_json(),
            call: authority.clone(),
        };
        let prepared = store.prepare(request()).await.unwrap();
        let dispatch_id = match store.record_dispatch_intent(request()).await.unwrap() {
            DispatchIntentResult::Recorded { dispatch_id, .. } => dispatch_id,
            other => panic!("unexpected intent: {other:?}"),
        };
        assert!(matches!(
            store.claim_dispatch(&dispatch_id, now).await.unwrap(),
            DispatchClaimResult::Claimed(_)
        ));
        let plan = SealedComputerActionPlan {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            work_id: prepared.work_id.to_string(),
            action_request_id: call_id,
            execution_generation: dispatch_id,
            device_id: "device-1".into(),
            interactive_session_incarnation: "desktop-1".into(),
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::BrowserDevtoolsMcp,
                version: "1".into(),
            },
            approval_id: "grant-1".into(),
            approved_actor_id: "actor-1".into(),
            draft_hash: authority.canonical_input_digest_sha256.into(),
            expires_at,
            timeout_ms: 30_000,
            actions: vec![ComputerActionStep {
                target: surface,
                action: ComputerActionKind::Browser(evaluated.request().clone()),
                before_summary: "original selection".into(),
                after_intent: "open approved page".into(),
                verification: "read back".into(),
            }],
        };
        plan.validate().unwrap();
        let started = ComputerActionStarted {
            work_id: plan.work_id.clone(),
            action_request_id: plan.action_request_id.clone(),
            execution_generation: plan.execution_generation.clone(),
            disposition: ComputerActionStartDisposition::MayHaveStarted,
            executor_accepted: true,
            reason: None,
        };
        Self {
            store,
            session,
            call,
            plan,
            started,
        }
    }

    async fn bind(&self) {
        self.store
            .bind_computer_transport("host-original", &self.plan, &self.session, &self.call, None)
            .await
            .unwrap();
    }

    async fn accept(&self) -> Result<AcceptanceOutcome, DbErr> {
        self.store
            .accept_computer_started(
                "host-original",
                "device-1",
                &self.plan.execution_generation,
                &self.started,
            )
            .await
    }

    async fn outbox(&self) -> agent_capability_dispatch_outbox::Model {
        agent_capability_dispatch_outbox::Entity::find()
            .one(&self.store.db)
            .await
            .unwrap()
            .unwrap()
    }
}

#[tokio::test]
async fn original_transport_freezes_model_call_and_lineage_without_another_work_or_send() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(file_db(&directory.path().join("binding.db")).await).await;
    let work_before = agent_action_item::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap();
    let grant_before = agent_capability_grant::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap();
    fixture.bind().await;
    let first = fixture.outbox().await;
    let binding: ComputerBinding =
        serde_json::from_str(first.computer_binding_json.as_ref().unwrap()).unwrap();
    assert_eq!(binding.origin.tool_call_id, fixture.call.id);
    assert_ne!(binding.origin.tool_call_id, first.call_id);
    assert_eq!(binding.origin.sensitivity, Sensitivity::Secret);
    assert_eq!(binding.origin.source_envelope_ids.len(), 2);
    assert!(
        !first
            .computer_binding_json
            .as_ref()
            .unwrap()
            .contains("private test input")
    );
    assert_eq!(fixture.accept().await.unwrap(), AcceptanceOutcome::Stored);
    let accepted = fixture.outbox().await;
    fixture.bind().await;
    assert_eq!(
        fixture.accept().await.unwrap(),
        AcceptanceOutcome::Duplicate
    );
    assert_eq!(fixture.outbox().await, accepted);
    assert_eq!(
        agent_action_item::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap(),
        work_before
    );
    assert_eq!(
        agent_capability_grant::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap(),
        grant_before
    );
    assert_eq!(
        agent_action_item::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        1
    );
    assert!(
        fixture
            .store
            .claim_dispatch(&fixture.plan.execution_generation, 1000)
            .await
            .is_err()
    );
    assert!(
        fixture
            .store
            .bind_computer_transport(
                "host-reconnected",
                &fixture.plan,
                &fixture.session,
                &fixture.call,
                None
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn missing_or_changed_original_identity_never_saves_acceptance() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(file_db(&directory.path().join("identity.db")).await).await;
    assert!(fixture.accept().await.is_err());
    let mut call = fixture.call.clone();
    call.id = "other-model-call".into();
    assert!(
        fixture
            .store
            .bind_computer_transport(
                "host-original",
                &fixture.plan,
                &fixture.session,
                &call,
                None
            )
            .await
            .is_err()
    );
    let mut changed = fixture.session.clone();
    changed.input_revision += 1;
    assert!(
        fixture
            .store
            .bind_computer_transport(
                "host-original",
                &fixture.plan,
                &changed,
                &fixture.call,
                None
            )
            .await
            .is_err()
    );
    let mut changed = fixture.session.clone();
    changed.conversation.last_mut().unwrap().data_envelope = None;
    assert!(
        fixture
            .store
            .bind_computer_transport(
                "host-original",
                &fixture.plan,
                &changed,
                &fixture.call,
                None
            )
            .await
            .is_err()
    );
    fixture.bind().await;
    for (connection, audience, frame) in [
        (
            "host-reconnected",
            "device-1",
            fixture.plan.execution_generation.as_str(),
        ),
        (
            "host-original",
            "other-device",
            fixture.plan.execution_generation.as_str(),
        ),
        ("host-original", "device-1", "wrong-frame"),
    ] {
        assert!(
            fixture
                .store
                .accept_computer_started(connection, audience, frame, &fixture.started)
                .await
                .is_err()
        );
    }
    for bad in [
        ComputerActionStarted {
            work_id: "9999".into(),
            ..fixture.started.clone()
        },
        ComputerActionStarted {
            action_request_id: "different-action".into(),
            ..fixture.started.clone()
        },
        ComputerActionStarted {
            disposition: ComputerActionStartDisposition::DefinitelyNotStarted,
            ..fixture.started.clone()
        },
        ComputerActionStarted {
            reason: Some("rejected".into()),
            ..fixture.started.clone()
        },
    ] {
        assert!(
            fixture
                .store
                .accept_computer_started(
                    "host-original",
                    "device-1",
                    &fixture.plan.execution_generation,
                    &bad
                )
                .await
                .is_err()
        );
    }
    let legacy = ComputerActionStarted {
        executor_accepted: false,
        ..fixture.started.clone()
    };
    assert_eq!(
        fixture
            .store
            .accept_computer_started(
                "host-original",
                "device-1",
                &fixture.plan.execution_generation,
                &legacy
            )
            .await
            .unwrap(),
        AcceptanceOutcome::Legacy
    );
    assert!(fixture.outbox().await.computer_acceptance_json.is_none());
    assert_eq!(fixture.accept().await.unwrap(), AcceptanceOutcome::Stored);
}

#[tokio::test]
async fn concurrent_acceptance_survives_reopen_and_keeps_original_timestamp() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("restart.db");
    let fixture = Fixture::new(file_db(&path).await).await;
    fixture.bind().await;
    let (first, second) = tokio::join!(fixture.accept(), fixture.accept());
    assert!(matches!(
        (first.unwrap(), second.unwrap()),
        (AcceptanceOutcome::Stored, AcceptanceOutcome::Duplicate)
            | (AcceptanceOutcome::Duplicate, AcceptanceOutcome::Stored)
    ));
    let accepted = fixture.outbox().await;
    let receipt: ComputerAcceptance =
        serde_json::from_str(accepted.computer_acceptance_json.as_ref().unwrap()).unwrap();
    assert!(receipt.accepted_at_unix_ms > 0);
    let started = fixture.started.clone();
    fixture.store.db.close().await.unwrap();
    let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
        .await
        .unwrap();
    let store = SignalCapabilityGrantStore::new(reopened.clone());
    assert_eq!(
        store
            .accept_computer_started(
                "host-original",
                "device-1",
                &started.execution_generation,
                &started
            )
            .await
            .unwrap(),
        AcceptanceOutcome::Duplicate
    );
    assert_eq!(
        agent_capability_dispatch_outbox::Entity::find()
            .one(&reopened)
            .await
            .unwrap()
            .unwrap(),
        accepted
    );
    assert!(
        store
            .claim_dispatch(&started.execution_generation, receipt.accepted_at_unix_ms)
            .await
            .is_err()
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn unknown_work_cannot_gain_new_acceptance_but_existing_acceptance_stays_immutable() {
    for accepted_first in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(file_db(&directory.path().join("unknown.db")).await).await;
        fixture.bind().await;
        if accepted_first {
            fixture.accept().await.unwrap();
        }
        let before = fixture.outbox().await.computer_acceptance_json;
        fixture
            .store
            .mark_dispatch_outcome_unknown(
                &fixture.plan.execution_generation,
                &fixture.plan.action_request_id,
                1,
                u64::try_from(Utc::now().timestamp_millis()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            fixture.accept().await.unwrap(),
            if accepted_first {
                AcceptanceOutcome::Duplicate
            } else {
                AcceptanceOutcome::Stale
            }
        );
        assert_eq!(fixture.outbox().await.computer_acceptance_json, before);
    }
}

#[tokio::test]
async fn expired_and_terminal_actions_cannot_gain_first_acceptance() {
    for terminal in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(file_db(&directory.path().join("stale.db")).await).await;
        fixture.bind().await;
        if terminal {
            fixture
                .store
                .record_dispatch_completion(
                    &CapabilityDispatchCompletion {
                        dispatch_id: fixture.plan.execution_generation.clone(),
                        call_id: fixture.plan.action_request_id.clone(),
                        generation: 1,
                        outcome: CapabilityDispatchOutcome::Succeeded,
                        result_digest_sha256: "a".repeat(64),
                    },
                    u64::try_from(Utc::now().timestamp_millis()).unwrap(),
                )
                .await
                .unwrap();
        } else {
            let row = fixture.outbox().await;
            let mut binding: ComputerBinding =
                serde_json::from_str(row.computer_binding_json.as_ref().unwrap()).unwrap();
            binding.plan.expires_at = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
            let mut active: agent_capability_dispatch_outbox::ActiveModel = row.into();
            active.computer_binding_json = Set(Some(serde_json::to_string(&binding).unwrap()));
            active.update(&fixture.store.db).await.unwrap();
        }
        assert_eq!(fixture.accept().await.unwrap(), AcceptanceOutcome::Stale);
        assert!(fixture.outbox().await.computer_acceptance_json.is_none());
    }
}

#[tokio::test]
async fn corrupted_provenance_or_uncommitted_reservation_is_not_an_acceptance_proof() {
    for change in [
        "reservation",
        "model_call",
        "dispatch_payload",
        "acceptance_digest",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(file_db(&directory.path().join("corrupt.db")).await).await;
        fixture.bind().await;
        if change == "acceptance_digest" {
            fixture.accept().await.unwrap();
        }
        let row = fixture.outbox().await;
        let mut active: agent_capability_dispatch_outbox::ActiveModel = row.clone().into();
        match change {
            "reservation" => {
                let row = agent_grant_reservation::Entity::find()
                    .one(&fixture.store.db)
                    .await
                    .unwrap()
                    .unwrap();
                let mut active: agent_grant_reservation::ActiveModel = row.into();
                active.state = Set(RESERVATION_STATUS_RELEASED.into());
                active.update(&fixture.store.db).await.unwrap();
            }
            "model_call" => {
                let mut binding: ComputerBinding =
                    serde_json::from_str(row.computer_binding_json.as_ref().unwrap()).unwrap();
                binding.origin.tool_call_id = "another-model-call".into();
                active.computer_binding_json = Set(Some(serde_json::to_string(&binding).unwrap()));
                active.update(&fixture.store.db).await.unwrap();
            }
            "dispatch_payload" => {
                let mut payload: CapabilityDispatchPayload =
                    serde_json::from_str(&row.payload_json).unwrap();
                payload.tool_name = "another_tool".into();
                active.payload_json = Set(serde_json::to_string(&payload).unwrap());
                active.update(&fixture.store.db).await.unwrap();
            }
            "acceptance_digest" => {
                let mut receipt: ComputerAcceptance =
                    serde_json::from_str(row.computer_acceptance_json.as_ref().unwrap()).unwrap();
                receipt.binding_sha256 = "f".repeat(64);
                active.computer_acceptance_json =
                    Set(Some(serde_json::to_string(&receipt).unwrap()));
                active.update(&fixture.store.db).await.unwrap();
            }
            _ => unreachable!(),
        }
        let before = fixture.outbox().await;
        assert!(fixture.accept().await.is_err(), "{change}");
        assert_eq!(fixture.outbox().await, before);
    }
}

#[test]
fn computer_binding_crash_child() {
    let Ok(path) = std::env::var(CRASH_DB_ENV) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let fixture = Fixture::new(file_db(Path::new(&path)).await).await;
        fixture.bind().await;
        fixture.accept().await.unwrap();
        if std::env::var(CRASH_PHASE_ENV)
            .unwrap()
            .starts_with("computer_completion_")
        {
            fixture
                .store
                .accept_computer_completion(
                    "host-original",
                    "device-1",
                    &fixture.plan.execution_generation,
                    &completion::verified(&fixture.plan),
                )
                .await
                .unwrap();
        }
        std::fs::write(std::env::var(CRASH_MARKER_ENV).unwrap(), b"accepted").unwrap();
        std::future::pending::<()>().await;
    });
}

#[tokio::test]
async fn abrupt_process_loss_preserves_only_committed_original_binding_acceptance_and_completion() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for phase in [
        "computer_binding_before_commit",
        "computer_acceptance_before_commit",
        "computer_acceptance_after_commit",
        "computer_completion_before_commit",
        "computer_completion_after_commit",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("crash.db");
        let marker = directory.path().join("reached");
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "capability_grant_store::tests::computer_binding::computer_binding_crash_child",
                    "--nocapture",
                ])
                .env(CRASH_DB_ENV, &path)
                .env(CRASH_MARKER_ENV, &marker)
                .env(CRASH_PHASE_ENV, phase)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !marker.exists() {
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "fixture exited: {phase}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let db = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        let row = agent_capability_dispatch_outbox::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.computer_binding_json.is_some(),
            phase != "computer_binding_before_commit"
        );
        assert_eq!(
            row.computer_acceptance_json.is_some(),
            phase == "computer_acceptance_after_commit"
                || phase.starts_with("computer_completion_")
        );
        assert_eq!(
            row.state,
            if phase == "computer_completion_after_commit" {
                DISPATCH_OUTBOX_COMPLETED
            } else {
                DISPATCH_OUTBOX_SENDING
            }
        );
        let work = agent_action_item::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            work.result_schema_version,
            if phase == "computer_completion_after_commit" {
                Some(2)
            } else {
                None
            }
        );
        assert_eq!(
            work.result_json.is_some(),
            phase == "computer_completion_after_commit"
        );
        assert_eq!(work.completion_delivery_state, "pending");
        assert_eq!(
            agent_action_item::Entity::find().count(&db).await.unwrap(),
            1
        );
        assert_eq!(
            agent_grant_reservation::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .state,
            RESERVATION_STATUS_COMMITTED
        );
        assert_eq!(
            agent_capability_grant::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .remaining_uses,
            0
        );
        let store = SignalCapabilityGrantStore::new(db.clone());
        assert!(store.claim_dispatch(&row.dispatch_id, 1000).await.is_err());
        drop(store);
        db.close().await.unwrap();
    }
}
