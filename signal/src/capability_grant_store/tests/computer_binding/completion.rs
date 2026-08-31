use super::*;
use crate::capability_grant_store::computer_completion::CompletionObservation;
use desk_agent_protocol::computer_use::{
    ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass,
    ComputerActionStepFact,
};
use desk_diagnose_core::seam::{ExecOutcome, WaitOutcome};

mod projections;
mod retention;

pub(super) fn failed(plan: &SealedComputerActionPlan) -> ComputerActionCompleted {
    ComputerActionCompleted {
        work_id: plan.work_id.clone(),
        action_request_id: plan.action_request_id.clone(),
        execution_generation: plan.execution_generation.clone(),
        result: ComputerActionResultClass::Failed,
        facts: vec![],
        message: Some("synthetic native failure".into()),
        output: None,
    }
}

pub(super) fn verified(plan: &SealedComputerActionPlan) -> ComputerActionCompleted {
    let output = serde_json::from_value(serde_json::json!({
        "schema_version":1,"call_id":plan.action_request_id,"outcome":"page_opened",
        "page":{"schema_version":1,"adapter":{"engine":"chrome_devtools_mcp","device_id":plan.device_id,
            "os_session_id":"desktop-1","browser_major_version":145,"browser_version":"145","adapter_id":"fixture","adapter_version":"1",
            "profile_incarnation":"profile-1","connection_revision":1},
            "page_id":"page-1","page_incarnation":"page-first","origin":{"kind":"https","host_ascii":"example.test","port":443},
            "document_revision":1,"url_sha256":"a".repeat(64),"observed_at_unix_ms":1000},
        "snapshot":null,"form_readback":[],"completed_at_unix_ms":1000
    })).unwrap();
    ComputerActionCompleted {
        result: ComputerActionResultClass::Verified,
        facts: vec![ComputerActionStepFact {
            index: 0,
            changed: true,
            verified: true,
            summary: "opened".into(),
        }],
        message: None,
        output: Some(ComputerActionOutput::Browser(output)),
        ..failed(plan)
    }
}

async fn observe(
    f: &Fixture,
    native: &ComputerActionCompleted,
) -> Result<CompletionObservation, DbErr> {
    f.store
        .accept_computer_completion(
            "host-original",
            "device-1",
            &f.plan.execution_generation,
            native,
        )
        .await
}

async fn work(f: &Fixture) -> agent_action_item::Model {
    agent_action_item::Entity::find_by_id(f.plan.work_id.parse::<i64>().unwrap())
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn terminal_receipt_survives_reopen_consumption_and_timeout_without_new_authority() {
    for succeeds in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.db");
        let f = Fixture::new(file_db(&path).await).await;
        f.bind().await;
        let native = if succeeds {
            verified(&f.plan)
        } else {
            failed(&f.plan)
        };
        assert_eq!(
            observe(&f, &native).await.unwrap(),
            CompletionObservation::Stored
        );
        let first_work = work(&f).await;
        let first_outbox = f.outbox().await;
        assert_eq!(first_work.result_schema_version, Some(2));
        assert_eq!(first_work.completion_delivery_state, "pending");
        assert_eq!(
            observe(&f, &native).await.unwrap(),
            CompletionObservation::Duplicate
        );
        assert_eq!(work(&f).await, first_work);
        assert_eq!(f.outbox().await, first_outbox);
        f.store
            .mark_dispatch_outcome_unknown(
                &f.plan.execution_generation,
                &f.plan.action_request_id,
                1,
                Utc::now().timestamp_millis() as u64,
            )
            .await
            .unwrap();
        assert_eq!(work(&f).await, first_work);
        let reopened = SignalCapabilityGrantStore::new(
            Database::connect(format!("sqlite://{}?mode=rw", path.display()))
                .await
                .unwrap(),
        );
        let result = reopened
            .read_computer_result(&f.plan.execution_generation, "run-1", "actor-1", "device-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.original_call_id, f.call.id);
        assert_eq!(result.receipt.envelope.sensitivity, Sensitivity::Secret);
        assert_eq!(
            result.receipt.action.execution_id,
            f.plan.execution_generation
        );
        assert_eq!(
            result.receipt.action.action_request_id,
            f.plan.action_request_id
        );
        assert!(result.receipt.envelope.allowed_destinations.is_empty());
        assert_eq!(
            matches!(result.into_exec(), ExecOutcome::Executed { .. }),
            succeeds
        );
        let wait = reopened
            .wait_computer_result(
                &f.plan.action_request_id,
                &f.plan.execution_generation,
                "run-1",
                "actor-1",
                "device-1",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            matches!(wait, WaitOutcome::CompletedWithReceipt { .. }),
            succeeds
        );
        assert_eq!(
            matches!(wait, WaitOutcome::FailedWithReceipt { .. }),
            !succeeds
        );
        assert!(
            reopened
                .consume_computer_result(
                    &first_work.completion_event_id,
                    "run-1",
                    "wrong",
                    "device-1"
                )
                .await
                .is_err()
        );
        for _ in 0..2 {
            assert!(
                reopened
                    .consume_computer_result(
                        &first_work.completion_event_id,
                        "run-1",
                        "actor-1",
                        "device-1"
                    )
                    .await
                    .unwrap()
            );
        }
        assert_eq!(work(&f).await.completion_delivery_state, "consumed");
        assert_eq!(work(&f).await.result_json, first_work.result_json);
        assert!(
            f.store
                .claim_dispatch(
                    &f.plan.execution_generation,
                    Utc::now().timestamp_millis() as u64
                )
                .await
                .is_err()
        );
        let grant = agent_capability_grant::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(grant.remaining_uses, 0);
    }
}

#[tokio::test]
async fn unknown_observation_refines_to_terminal_without_refreshing_first_observation() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("unknown.db")).await).await;
    f.bind().await;
    let unknown = ComputerActionCompleted {
        result: ComputerActionResultClass::PartiallyApplied,
        facts: vec![ComputerActionStepFact {
            index: 0,
            changed: true,
            verified: false,
            summary: "partial".into(),
        }],
        ..failed(&f.plan)
    };
    assert_eq!(
        observe(&f, &unknown).await.unwrap(),
        CompletionObservation::Unknown
    );
    let first = work(&f).await;
    assert_eq!(
        observe(&f, &unknown).await.unwrap(),
        CompletionObservation::Duplicate
    );
    assert_eq!(work(&f).await, first);
    let WaitOutcome::UnknownWithIdentity {
        original_call_id, ..
    } = f
        .store
        .wait_computer_result(
            &f.plan.action_request_id,
            &f.plan.execution_generation,
            "run-1",
            "actor-1",
            "device-1",
        )
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("unknown required")
    };
    assert_eq!(original_call_id, f.call.id);
    let native = verified(&f.plan);
    assert_eq!(
        observe(&f, &native).await.unwrap(),
        CompletionObservation::Stored
    );
    let terminal = work(&f).await;
    let before: serde_json::Value =
        serde_json::from_str(first.result_json.as_deref().unwrap()).unwrap();
    let after: serde_json::Value =
        serde_json::from_str(terminal.result_json.as_deref().unwrap()).unwrap();
    assert_eq!(before["unknown"], after["unknown"]);
    assert_eq!(
        observe(&f, &failed(&f.plan)).await.unwrap(),
        CompletionObservation::Stale
    );
    assert_eq!(work(&f).await, terminal);
}

#[tokio::test]
async fn wrong_completion_identity_type_and_facts_do_not_change_original_work() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("invalid.db")).await).await;
    f.bind().await;
    let before = work(&f).await;
    for case in [
        "connection",
        "audience",
        "frame",
        "work",
        "action",
        "generation",
        "facts",
        "index",
        "browser_call",
        "output",
        "large",
    ] {
        let mut native = verified(&f.plan);
        let mut connection = "host-original";
        let mut audience = "device-1";
        let mut frame = f.plan.execution_generation.as_str();
        match case {
            "connection" => connection = "reconnected",
            "audience" => audience = "wrong",
            "frame" => frame = "wrong",
            "work" => native.work_id = "999".into(),
            "action" => native.action_request_id = "wrong".into(),
            "generation" => native.execution_generation = "wrong".into(),
            "facts" => native.facts.clear(),
            "index" => native.facts[0].index = 2,
            "browser_call" => {
                if let Some(ComputerActionOutput::Browser(b)) = &mut native.output {
                    b.call_id = "wrong".into();
                }
            }
            "output" => native.output = None,
            "large" => native.message = Some("x".repeat(4097)),
            _ => unreachable!(),
        }
        assert!(
            f.store
                .accept_computer_completion(connection, audience, frame, &native)
                .await
                .is_err(),
            "{case}"
        );
        assert_eq!(work(&f).await, before, "{case}");
    }
}

#[tokio::test]
async fn concurrent_completions_commit_one_immutable_receipt_and_reject_storage_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("concurrent.db")).await).await;
    f.bind().await;
    let native = verified(&f.plan);
    let results = futures_util::future::join_all((0..8).map(|_| observe(&f, &native))).await;
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Ok(CompletionObservation::Stored)))
            .count(),
        1
    );
    assert!(results.iter().all(|r| matches!(
        r,
        Ok(CompletionObservation::Stored | CompletionObservation::Duplicate)
    )));
    let original = work(&f).await;
    for field in ["binding_sha256", "terminal"] {
        let mut value: serde_json::Value =
            serde_json::from_str(original.result_json.as_deref().unwrap()).unwrap();
        if field == "binding_sha256" {
            value[field] = serde_json::json!("a".repeat(64));
        } else {
            value[field]["projection"]["content"] = serde_json::json!("altered");
        }
        let mut active: agent_action_item::ActiveModel = original.clone().into();
        active.result_json = Set(Some(value.to_string()));
        active.update(&f.store.db).await.unwrap();
        assert!(
            f.store
                .read_computer_result(&f.plan.execution_generation, "run-1", "actor-1", "device-1")
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn semantic_projection_preserves_idempotent_success_and_unknown_effects() {
    use crate::remote_tool_edge::completion::project;
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("semantic.db")).await).await;
    let mut plan = f.plan.clone();
    plan.adapter.kind = ComputerUseAdapterKind::MacosAccessibility;
    plan.actions[0].target.object_kind = ObjectKind::UiElement;
    plan.actions[0].action =
        ComputerActionKind::Ui(desk_agent_protocol::computer_use::UiSemanticAction::Focus);
    let mut native = failed(&plan);
    native.result = ComputerActionResultClass::Verified;
    native.facts = vec![ComputerActionStepFact {
        index: 0,
        changed: false,
        verified: true,
        summary: "already selected".into(),
    }];
    assert_eq!(
        project(&plan, "execute_confirmed_ui_action", "run-1", "{}", &native)
            .unwrap()
            .unwrap()
            .outcome,
        CapabilityDispatchOutcome::Succeeded
    );
    for class in [
        ComputerActionResultClass::OutcomeUnknown,
        ComputerActionResultClass::ChangedButUnverified,
        ComputerActionResultClass::PartiallyApplied,
        ComputerActionResultClass::RollbackUnsafe,
    ] {
        native.result = class;
        native.facts[0].verified = false;
        assert!(
            project(&plan, "execute_confirmed_ui_action", "run-1", "{}", &native)
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn timeout_and_authenticated_completion_converge_on_the_original_terminal_receipt() {
    for _ in 0..12 {
        let dir = tempfile::tempdir().unwrap();
        let f = Fixture::new(file_db(&dir.path().join("race.db")).await).await;
        f.bind().await;
        let native = verified(&f.plan);
        let (unknown, completed) = tokio::join!(
            f.store.mark_dispatch_outcome_unknown(
                &f.plan.execution_generation,
                &f.plan.action_request_id,
                1,
                Utc::now().timestamp_millis() as u64
            ),
            observe(&f, &native),
        );
        unknown.unwrap();
        assert_eq!(completed.unwrap(), CompletionObservation::Stored);
        assert!(
            f.store
                .read_computer_result(&f.plan.execution_generation, "run-1", "actor-1", "device-1")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(work(&f).await.status, CAPABILITY_WORK_SUCCEEDED);
    }
}
