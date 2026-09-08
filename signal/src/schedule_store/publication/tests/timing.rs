//! Expiry is evaluated after evidence verification, using the database wall clock.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

struct WaitingVerifier {
    until: i64,
    called: AtomicBool,
}

#[async_trait::async_trait]
impl TaskPublicationVerifier for WaitingVerifier {
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
        expires_at: Option<i64>,
    ) -> Result<TaskRehearsalEvidence, ScheduleStoreError> {
        self.called.store(true, Ordering::SeqCst);
        let proof = Verifier(true)
            .verify(txn, task, contract, rehearsal, expires_at)
            .await?;
        while super::super::super::authority::authority_now(txn).await? <= self.until {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(proof)
    }
}

#[tokio::test]
async fn publication_rechecks_expiry_and_once_time_after_verifier_wait() {
    for once in [false, true] {
        let (store, mut before, contract, mut input) = fixture().await;
        let now = store.database_time().await.unwrap();
        // Whole-second timestamps are the persisted schedule format.
        let deadline = (now / 1000 + 1) * 1000 + 1000;
        if once {
            use desk_agent_protocol::schedule::{ScheduleRule, ScheduleSpec};
            use sea_orm::ActiveModelTrait;
            let at = chrono::DateTime::from_timestamp_millis(deadline)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let mut changed: entity::ActiveModel = before.into();
            changed.spec_json = Set(json(&ScheduleSpec {
                schema_version: 1,
                rule: ScheduleRule::Once { at },
            })
            .unwrap());
            before = changed.update(&store.db).await.unwrap();
            input.expires_at = None;
        } else {
            input.expires_at = Some(deadline);
        }
        let verifier = WaitingVerifier {
            until: deadline,
            called: AtomicBool::new(false),
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            store.publish_task(1, &input, &verifier),
        )
        .await
        .unwrap();
        assert!(verifier.called.load(Ordering::SeqCst));
        assert!(
            result.is_err(),
            "expired publication must roll back, once={once}"
        );
        assert_eq!(store.read(1, &before.schedule_id).await.unwrap(), before);
        assert_eq!(
            store
                .read_contract(1, &before.schedule_id, contract.contract_revision)
                .await
                .unwrap(),
            contract
        );
        assert_eq!(
            authorization::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
            0
        );
    }
}

struct SubjectGate {
    mode: u8,
    locked: AtomicBool,
    verified: AtomicBool,
}

#[async_trait::async_trait]
impl TaskPublicationVerifier for SubjectGate {
    async fn lock_subject(
        &self,
        txn: &DatabaseTransaction,
        task: &entity::Model,
    ) -> Result<(), ScheduleStoreError> {
        self.locked.store(true, Ordering::SeqCst);
        assert!(!self.verified.load(Ordering::SeqCst));
        if self.mode == 1 {
            return Err(ScheduleStoreError::NotFound);
        }
        if self.mode == 2 {
            use sea_orm::ActiveModelTrait;
            let mut changed: entity::ActiveModel = task.clone().into();
            changed.target_device_id = Set("different-device".into());
            changed.update(txn).await?;
        }
        Ok(())
    }

    async fn verify(
        &self,
        txn: &DatabaseTransaction,
        task: &entity::Model,
        contract: &ValidatedTaskContract,
        rehearsal: &str,
        expires_at: Option<i64>,
    ) -> Result<TaskRehearsalEvidence, ScheduleStoreError> {
        assert!(self.locked.load(Ordering::SeqCst));
        self.verified.store(true, Ordering::SeqCst);
        Verifier(true)
            .verify(txn, task, contract, rehearsal, expires_at)
            .await
    }
}

#[tokio::test]
async fn publication_locks_subject_before_evidence_and_rejects_retarget_during_wait() {
    for mode in [0, 1, 2] {
        let (store, before, _, input) = fixture().await;
        let verifier = SubjectGate {
            mode,
            locked: AtomicBool::new(false),
            verified: AtomicBool::new(false),
        };
        let result = store.publish_task(1, &input, &verifier).await;
        assert!(verifier.locked.load(Ordering::SeqCst));
        assert_eq!(verifier.verified.load(Ordering::SeqCst), mode == 0);
        if mode == 0 {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err());
            assert_eq!(store.read(1, &before.schedule_id).await.unwrap(), before);
            assert_eq!(
                authorization::Entity::find()
                    .count(&store.db)
                    .await
                    .unwrap(),
                0
            );
        }
    }
}
