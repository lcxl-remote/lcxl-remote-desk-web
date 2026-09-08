use super::*;
use desk_agent_protocol::schedule::contract::{TaskBudget, TaskExceptionMode};
use sea_orm::{Database, PaginatorTrait, Schema};

pub(crate) struct Verifier(pub(crate) bool);
#[async_trait::async_trait]
impl TaskPublicationVerifier for Verifier {
    async fn lock_subject(
        &self,
        _: &DatabaseTransaction,
        _: &entity::Model,
    ) -> Result<(), ScheduleStoreError> {
        // Storage-only fixtures do not represent current device authorization.
        Ok(())
    }

    async fn verify(
        &self,
        txn: &DatabaseTransaction,
        task: &entity::Model,
        contract: &ValidatedTaskContract,
        rehearsal: &str,
        _: Option<i64>,
    ) -> Result<TaskRehearsalEvidence, ScheduleStoreError> {
        if !self.0 {
            return Err(ScheduleStoreError::Invalid);
        }
        assert_eq!(contract.contract().schedule_id, task.schedule_id);
        assert_eq!(contract.contract().task_revision, task.task_revision as u64);
        Ok(TaskRehearsalEvidence {
            rehearsal_run_id: rehearsal.into(),
            conversation_id: "rehearsal-conversation".into(),
            input_revision: 1,
            evidence_sha256: "e".repeat(64),
            finished_at: database_now(txn).await? - 60_000,
        })
    }
}

fn definition(task: &entity::Model) -> TaskContract {
    TaskContract {
        schema_version: 1,
        schedule_id: task.schedule_id.clone(),
        task_revision: task.task_revision as u64,
        contract_revision: 999,
        target_device_id: task.target_device_id.clone(),
        prompt_sha256: digest(&task.prompt),
        permissions: vec![],
        steps: vec![],
        exception_mode: TaskExceptionMode::Deny,
        budget: TaskBudget {
            max_runs_per_utc_day: 24,
            max_calls_per_run: 10,
            max_model_tokens_per_run: 10000,
            max_runtime_seconds: 300,
        },
    }
}
async fn fixture() -> (
    ScheduleStore,
    entity::Model,
    contract_row::Model,
    PublishTask,
) {
    fixture_on(Database::connect("sqlite::memory:").await.unwrap()).await
}

pub(crate) async fn fixture_on(
    db: sea_orm::DatabaseConnection,
) -> (
    ScheduleStore,
    entity::Model,
    contract_row::Model,
    PublishTask,
) {
    let schema = Schema::new(db.get_database_backend());
    for mut table in [
        schema.create_table_from_entity(crate::entity::schedule_budget_policy::Entity),
        schema.create_table_from_entity(entity::Entity),
        schema.create_table_from_entity(contract_row::Entity),
        schema.create_table_from_entity(authorization::Entity),
        schema.create_table_from_entity(crate::entity::agent_schedule_run::Entity),
        schema.create_table_from_entity(crate::entity::agent_task_budget_reservation::Entity),
    ] {
        db.execute(table.if_not_exists()).await.unwrap();
    }
    for mut index in schema
        .create_index_from_entity(entity::Entity)
        .into_iter()
        .chain(schema.create_index_from_entity(contract_row::Entity))
        .chain(schema.create_index_from_entity(authorization::Entity))
        .chain(schema.create_index_from_entity(crate::entity::agent_schedule_run::Entity))
        .chain(
            schema.create_index_from_entity(crate::entity::agent_task_budget_reservation::Entity),
        )
    {
        db.execute(index.if_not_exists()).await.unwrap();
    }
    let store = ScheduleStore::new(db);
    let now = database_now(&store.db).await.unwrap();
    let task = store
        .create_draft(1, &super::super::tests::draft(), now)
        .await
        .unwrap();
    let contract = store
        .save_contract(1, task.revision, &definition(&task))
        .await
        .unwrap();
    let task = store.read(1, &task.schedule_id).await.unwrap();
    let input = PublishTask {
        schedule_id: task.schedule_id.clone(),
        expected_revision: task.revision,
        contract_revision: contract.contract_revision,
        contract_sha256: contract.digest_sha256.clone(),
        rehearsal_run_id: "rehearsal-1".into(),
        expires_at: Some(now + 86_400_000),
        client_publish_key: "publish-1".into(),
    };
    (store, task, contract, input)
}

#[tokio::test]
async fn contract_save_is_immutable_owner_scoped_and_never_grants_authority() {
    let (store, task, first, _) = fixture().await;
    assert_eq!(first.contract_revision, 1);
    assert_eq!(task.status, "draft");
    assert!(task.next_run_at.is_none());
    assert!(task.authorization_revision.is_none());
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    assert!(matches!(
        store.read_contract(2, &task.schedule_id, 1).await,
        Err(ScheduleStoreError::NotFound)
    ));
    let second = store
        .save_contract(1, task.revision, &definition(&task))
        .await
        .unwrap();
    assert_eq!(second.contract_revision, 2);
    assert_eq!(
        store.read_contract(1, &task.schedule_id, 1).await.unwrap(),
        first
    );
    assert!(matches!(
        store
            .save_contract(1, task.revision, &definition(&task))
            .await,
        Err(ScheduleStoreError::Conflict)
    ));
    let current = store.read(1, &task.schedule_id).await.unwrap();
    let mut changed = definition(&current);
    changed.target_device_id = "other-device".into();
    assert!(matches!(
        store.save_contract(1, current.revision, &changed).await,
        Err(ScheduleStoreError::Conflict)
    ));
}

#[tokio::test]
async fn publication_requires_exact_review_and_verified_evidence_then_replays_without_reapproval() {
    let (store, before, contract, input) = fixture().await;
    let mut wrong = input.clone();
    wrong.contract_sha256 = "f".repeat(64);
    assert!(matches!(
        store.publish_task(1, &wrong, &Verifier(true)).await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert!(matches!(
        store.publish_task(1, &input, &Verifier(false)).await,
        Err(ScheduleStoreError::Invalid)
    ));
    assert_eq!(store.read(1, &before.schedule_id).await.unwrap(), before);
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    let published = store
        .publish_task(1, &input, &Verifier(true))
        .await
        .unwrap();
    assert_eq!(published.contract_sha256, contract.digest_sha256);
    assert_eq!(published.authorization_revision, 1);
    let current = store.read(1, &before.schedule_id).await.unwrap();
    assert_eq!(current.status, "active");
    assert_eq!(current.authorization_revision, Some(1));
    assert!(current.next_run_at.unwrap() > published.approved_at);
    assert_eq!(
        store
            .publish_task(1, &input, &Verifier(false))
            .await
            .unwrap(),
        published
    );
    assert_eq!(store.read(1, &before.schedule_id).await.unwrap(), current);
    let mut conflicting = input.clone();
    conflicting.expires_at = None;
    assert!(matches!(
        store.publish_task(1, &conflicting, &Verifier(true)).await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert!(matches!(
        store
            .read_authorization(2, &published.authorization_id)
            .await,
        Err(ScheduleStoreError::NotFound)
    ));
}

#[tokio::test]
async fn revocation_cancels_queued_run_and_publication_replay_never_reenables() {
    let (store, task, _, input) = fixture().await;
    let published = store
        .publish_task(1, &input, &Verifier(true))
        .await
        .unwrap();
    let queued = store
        .enqueue_manual(1, &task.schedule_id, "manual-1")
        .await
        .unwrap();
    let revoked = store
        .revoke_task_authorization(1, &published.authorization_id, "owner revoked")
        .await
        .unwrap();
    assert!(revoked.revoked_at.is_some());
    assert_eq!(revoked.version, 2);
    let paused = store.read(1, &task.schedule_id).await.unwrap();
    assert_eq!(paused.status, "paused");
    assert!(paused.next_run_at.is_none());
    assert!(paused.active_run_id.is_none());
    let run = crate::entity::agent_schedule_run::Entity::find_by_id(queued.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "cancelled");
    assert!(run.failure_accounted);
    assert_eq!(
        store
            .publish_task(1, &input, &Verifier(true))
            .await
            .unwrap(),
        revoked
    );
    assert_eq!(
        store
            .revoke_task_authorization(1, &published.authorization_id, "different reason")
            .await
            .unwrap(),
        revoked
    );
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), paused);
    assert!(matches!(
        store.enqueue_manual(1, &task.schedule_id, "manual-2").await,
        Err(ScheduleStoreError::Conflict)
    ));
}

#[tokio::test]
async fn unknown_effect_and_stale_contract_cannot_be_cleared_by_publication() {
    let (store, task, _, input) = fixture().await;
    let mut state = FailureState::default();
    state
        .pause_reasons
        .insert(SchedulePauseReason::UnknownSideEffect);
    entity::Entity::update_many()
        .set(entity::ActiveModel {
            failure_state_json: Set(json(&state).unwrap()),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(matches!(
        store.publish_task(1, &input, &Verifier(true)).await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    let (store, task, _, input) = fixture().await;
    store
        .save_contract(1, task.revision, &definition(&task))
        .await
        .unwrap();
    let mut stale = input;
    stale.expected_revision = store.read(1, &task.schedule_id).await.unwrap().revision;
    assert!(matches!(
        store.publish_task(1, &stale, &Verifier(true)).await,
        Err(ScheduleStoreError::Conflict)
    ));
}

#[tokio::test]
async fn revoking_running_authority_records_cancel_without_faking_completion() {
    use desk_agent_protocol::schedule::ScheduledRunStatus;
    let (store, task, _, input) = fixture().await;
    let published = store
        .publish_task(1, &input, &Verifier(true))
        .await
        .unwrap();
    let queued = store
        .enqueue_manual(1, &task.schedule_id, "manual-running")
        .await
        .unwrap();
    let leased = store
        .claim_queued(&queued.run_id, "node", 90)
        .await
        .unwrap();
    assert!(matches!(
        store
            .revoke_task_authorization(2, &published.authorization_id, "other owner")
            .await,
        Err(ScheduleStoreError::NotFound)
    ));
    store
        .revoke_task_authorization(1, &published.authorization_id, "owner revoked")
        .await
        .unwrap();
    let running = crate::entity::agent_schedule_run::Entity::find_by_id(queued.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.status, "running");
    assert!(running.cancel_requested_at.is_some());
    assert!(!running.failure_accounted);
    assert!(running.finished_at.is_none());
    assert_eq!(
        store
            .read(1, &task.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .as_deref(),
        Some(queued.run_id.as_str())
    );
    store
        .finish_run(
            &queued.run_id,
            "node",
            leased.lease_epoch,
            ScheduledRunStatus::Cancelled,
            None,
            None,
        )
        .await
        .unwrap();
    let stopped = store.read(1, &task.schedule_id).await.unwrap();
    assert_eq!(stopped.status, "paused");
    assert!(stopped.active_run_id.is_none());
    let failures: FailureState = serde_json::from_str(&stopped.failure_state_json).unwrap();
    assert!(
        failures
            .pause_reasons
            .contains(&SchedulePauseReason::AuthorizationInvalid)
    );
}

async fn running_fixture() -> (
    ScheduleStore,
    entity::Model,
    authorization::Model,
    crate::entity::agent_schedule_run::Model,
) {
    let (store, task, _, input) = fixture().await;
    let parent = store
        .publish_task(1, &input, &Verifier(true))
        .await
        .unwrap();
    let queued = store
        .enqueue_manual(1, &task.schedule_id, "authority-run")
        .await
        .unwrap();
    let work = store
        .claim_queued(&queued.run_id, "authority-node", 90)
        .await
        .unwrap();
    (store, task, parent, work)
}

async fn authority_check(
    store: &ScheduleStore,
    owner: i32,
    device: &str,
    run: &str,
    node: &str,
    epoch: i64,
) -> Result<super::super::CurrentTaskAuthority, ScheduleStoreError> {
    let txn = store.db.begin().await.unwrap();
    let result = ScheduleStore::lock_run_authority(&txn, owner, device, run, node, epoch).await;
    // This helper only inspects authority; no reservation/dispatch is committed.
    txn.rollback().await.unwrap();
    result
}

#[tokio::test]
async fn current_authority_is_owner_device_and_lease_bound_and_user_pause_is_not_cancel() {
    let (store, task, parent, work) = running_fixture().await;
    for (owner, device, node, epoch) in [
        (
            2,
            task.target_device_id.as_str(),
            "authority-node",
            work.lease_epoch,
        ),
        (1, "other-device", "authority-node", work.lease_epoch),
        (
            1,
            task.target_device_id.as_str(),
            "other-node",
            work.lease_epoch,
        ),
        (
            1,
            task.target_device_id.as_str(),
            "authority-node",
            work.lease_epoch + 1,
        ),
    ] {
        assert!(
            authority_check(&store, owner, device, &work.run_id, node, epoch)
                .await
                .is_err()
        );
    }
    let before = store.read(1, &task.schedule_id).await.unwrap();
    let authority = authority_check(
        &store,
        1,
        &task.target_device_id,
        &work.run_id,
        "authority-node",
        work.lease_epoch,
    )
    .await
    .unwrap();
    assert_eq!(
        authority.provenance().authorization_id,
        parent.authorization_id
    );
    assert_eq!(authority.provenance().scheduled_run_id, work.run_id);
    assert_eq!(authority.contract().digest(), parent.contract_sha256);
    assert_eq!(authority.run().conversation_id, work.conversation_id);
    assert!(authority.valid_until() > authority.verified_at());
    assert_eq!(authority.valid_until(), work.lease_deadline.unwrap());
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
    store
        .pause(
            1,
            &task.schedule_id,
            before.revision,
            database_now(&store.db).await.unwrap(),
        )
        .await
        .unwrap();
    assert!(
        authority_check(
            &store,
            1,
            &task.target_device_id,
            &work.run_id,
            "authority-node",
            work.lease_epoch
        )
        .await
        .is_ok()
    );
    store.cancel_run(1, &work.run_id).await.unwrap();
    assert!(
        authority_check(
            &store,
            1,
            &task.target_device_id,
            &work.run_id,
            "authority-node",
            work.lease_epoch
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn revoked_or_replaced_task_authority_cannot_be_reused_by_a_running_lease() {
    for revoke in [true, false] {
        let (store, task, parent, work) = running_fixture().await;
        if revoke {
            store
                .revoke_task_authorization(1, &parent.authorization_id, "owner revoked")
                .await
                .unwrap();
        } else {
            let current = store.read(1, &task.schedule_id).await.unwrap();
            store
                .change_prompt(1, &task.schedule_id, current.revision, "a different task")
                .await
                .unwrap();
        }
        assert!(
            authority_check(
                &store,
                1,
                &task.target_device_id,
                &work.run_id,
                "authority-node",
                work.lease_epoch
            )
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn expired_or_corrupt_parent_snapshot_and_runtime_never_authorize_actions() {
    use crate::entity::agent_schedule_run as work_row;
    for case in 0..9 {
        let (store, task, parent, work) = running_fixture().await;
        let now = database_now(&store.db).await.unwrap();
        match case {
            0 => {
                authorization::Entity::update_many()
                    .set(authorization::ActiveModel {
                        expires_at: Set(Some(now)),
                        ..Default::default()
                    })
                    .filter(authorization::Column::Id.eq(parent.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            1 => {
                work_row::Entity::update_many()
                    .set(work_row::ActiveModel {
                        lease_deadline: Set(Some(now)),
                        ..Default::default()
                    })
                    .filter(work_row::Column::Id.eq(work.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            2 => {
                work_row::Entity::update_many()
                    .set(work_row::ActiveModel {
                        recovery_epoch: Set(work.recovery_epoch + 1),
                        ..Default::default()
                    })
                    .filter(work_row::Column::Id.eq(work.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            3 => {
                let mut snapshot: entity::Model =
                    serde_json::from_str(&work.task_snapshot_json).unwrap();
                snapshot.authorization_revision = Some(999);
                work_row::Entity::update_many()
                    .set(work_row::ActiveModel {
                        task_snapshot_json: Set(json(&snapshot).unwrap()),
                        ..Default::default()
                    })
                    .filter(work_row::Column::Id.eq(work.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            4 => {
                contract_row::Entity::update_many()
                    .set(contract_row::ActiveModel {
                        digest_sha256: Set("f".repeat(64)),
                        ..Default::default()
                    })
                    .filter(contract_row::Column::ScheduleId.eq(&task.schedule_id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            5 => {
                work_row::Entity::update_many()
                    .set(work_row::ActiveModel {
                        started_at: Set(Some(now - 300_000)),
                        ..Default::default()
                    })
                    .filter(work_row::Column::Id.eq(work.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            6 => {
                authorization::Entity::update_many()
                    .set(authorization::ActiveModel {
                        contract_sha256: Set("f".repeat(64)),
                        ..Default::default()
                    })
                    .filter(authorization::Column::Id.eq(parent.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            7 => {
                authorization::Entity::update_many()
                    .set(authorization::ActiveModel {
                        revision_identity: Set("f".repeat(64)),
                        ..Default::default()
                    })
                    .filter(authorization::Column::Id.eq(parent.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            8 => {
                let mut proof: TaskRehearsalEvidence =
                    serde_json::from_str(&parent.rehearsal_evidence_json).unwrap();
                proof.evidence_sha256.clear();
                authorization::Entity::update_many()
                    .set(authorization::ActiveModel {
                        rehearsal_evidence_json: Set(json(&proof).unwrap()),
                        ..Default::default()
                    })
                    .filter(authorization::Column::Id.eq(parent.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = store.read(1, &task.schedule_id).await.unwrap();
        assert!(
            authority_check(
                &store,
                1,
                &task.target_device_id,
                &work.run_id,
                "authority-node",
                work.lease_epoch
            )
            .await
            .is_err(),
            "case {case}"
        );
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
    }
}

mod timing;
