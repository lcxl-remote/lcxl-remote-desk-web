//! What the daemon does with a worker's answer to a published policy.
//!
//! Kept out of `tests.rs`, which is Windows-only because it covers the launch
//! path's token selection: policy distribution runs on every platform and the
//! host forms that exercise it most (portable, desk-server) are not Windows
//! services at all.

use super::*;
use desk_signal_facade::model::policy_snapshot::PolicyGenerations;

/// A bare manager, with no worker installed. Nothing here publishes; these
/// tests are about the answer coming back.
fn test_manager() -> (WorkerManager, WorkerMessageReceiver) {
    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
        crate::model::settings::Settings::default(),
    )));
    WorkerManager::new(settings, PcRegistry::new())
}

/// A worker confirming a policy needs nothing further, and the sequence it
/// confirmed is what the daemon then believes is in force.
#[tokio::test]
async fn a_confirmed_policy_asks_for_nothing() {
    let (manager, _worker_rx) = test_manager();

    let resync_wanted = manager
        .note_policy_applied(&SecurityPolicyAppliedPayload {
            operation_id: "op-1".into(),
            outcome: PolicyApplyOutcome::Applied {
                seq: 7,
                generations: PolicyGenerations::default(),
            },
        })
        .await;

    assert!(!resync_wanted);
    assert_eq!(manager.policy_applied_seq(), 7);
}

/// A worker that could not reconcile a policy has tightened locally and cannot
/// leave that state on its own: the tightening moved its own sequence past the
/// daemon's, so every policy the daemon already holds reads as stale to it.
/// Saying so upwards is what gets the policy republished. Without it the worker
/// keeps prompting for capabilities the operator has already allowed and
/// nothing on the host explains why.
#[tokio::test]
async fn a_worker_that_cannot_reconcile_asks_to_be_resynchronized() {
    let (manager, _worker_rx) = test_manager();

    let resync_wanted = manager
        .note_policy_applied(&SecurityPolicyAppliedPayload {
            operation_id: "op-2".into(),
            outcome: PolicyApplyOutcome::NeedsResync { seq: 9 },
        })
        .await;

    assert!(resync_wanted);
    assert_eq!(
        manager.policy_applied_seq(),
        0,
        "a locally tightened policy is not one the daemon published, so nothing applied it",
    );
}

/// The waiter `publish_security_policy` parks on is released by either answer —
/// a worker that reports a contradiction has still answered, and leaving the
/// publisher to time out would report a live worker as unreachable.
#[tokio::test]
async fn either_answer_releases_the_publisher() {
    for outcome in [
        PolicyApplyOutcome::Applied {
            seq: 3,
            generations: PolicyGenerations::default(),
        },
        PolicyApplyOutcome::NeedsResync { seq: 3 },
    ] {
        let (manager, _worker_rx) = test_manager();
        let (tx, rx) = oneshot::channel();
        manager
            .policy_acks
            .lock()
            .unwrap()
            .insert("op-3".to_string(), tx);

        let _ = manager
            .note_policy_applied(&SecurityPolicyAppliedPayload {
                operation_id: "op-3".into(),
                outcome: outcome.clone(),
            })
            .await;

        assert_eq!(
            rx.await.expect("the publisher must be answered").outcome,
            outcome,
        );
        assert!(
            manager.policy_acks.lock().unwrap().is_empty(),
            "an answered operation must not stay in the waiting set",
        );
    }
}
