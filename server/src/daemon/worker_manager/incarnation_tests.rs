//! Telling a worker's messages apart from its replacement's.
//!
//! Replacing a worker does not silence it. Whatever it had already put on the
//! wire, and whatever its bridge had already queued, arrives after the
//! replacement is installed — so the daemon has to be able to look at a message
//! and say which worker it came from. These cover that decision; the handlers
//! downstream all assume it has already been made.
//!
//! Platform-independent by design: all three host forms (portable, desk-server,
//! service-daemon) run the same daemon-worker link, differing only in whether
//! the bridge spans processes.

use super::*;

fn test_manager() -> (WorkerManager, WorkerMessageReceiver) {
    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
        crate::model::settings::Settings::default(),
    )));
    WorkerManager::new(settings, PcRegistry::new())
}

/// Installs a worker and returns its incarnation. The command receiver is
/// dropped: these tests never look at what the daemon sends downwards.
async fn install_worker(manager: &WorkerManager) -> WorkerIncarnation {
    let (ipc_tx, _ipc_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
    manager.install_active_for_test(ipc_tx).await
}

/// The ordinary case: the worker that is running is heard.
#[tokio::test]
async fn the_running_worker_is_heard() {
    let (manager, _worker_rx) = test_manager();
    let worker = install_worker(&manager).await;

    assert!(manager.note_message_from(worker).await);
}

/// A worker that has been replaced is not. Its message describes a process the
/// daemon has already torn down — acting on it means letting a worker that is
/// gone overwrite what the daemon believes about the one that took its place.
#[tokio::test]
async fn a_replaced_worker_is_not() {
    let (manager, _worker_rx) = test_manager();
    let outgoing = install_worker(&manager).await;
    let incoming = install_worker(&manager).await;

    assert_ne!(
        outgoing, incoming,
        "a replacement must be a different worker, not the same slot reused",
    );
    assert!(!manager.note_message_from(outgoing).await);
    assert!(manager.note_message_from(incoming).await);
}

/// Every message counts as a sign of life, but only for the worker that sent
/// it. A replaced worker's backlog arriving must not stand in for a replacement
/// that has never spoken — that is exactly the case the watchdog exists to
/// catch, and crediting the backlog would keep it quiet through the whole
/// timeout.
#[tokio::test]
async fn a_replaced_workers_backlog_is_not_its_successors_heartbeat() {
    let (manager, _worker_rx) = test_manager();
    let outgoing = install_worker(&manager).await;
    let incoming = install_worker(&manager).await;

    let installed_at = manager
        .active_worker_snapshot()
        .await
        .expect("a worker is installed")
        .3;

    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(!manager.note_message_from(outgoing).await);
    assert_eq!(
        manager
            .active_worker_snapshot()
            .await
            .expect("a worker is installed")
            .3,
        installed_at,
        "the replacement has said nothing, so its last sign of life must not move",
    );

    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(manager.note_message_from(incoming).await);
    assert!(
        manager
            .active_worker_snapshot()
            .await
            .expect("a worker is installed")
            .3
            > installed_at,
        "the replacement speaking for itself is what counts",
    );
}

/// A pipe server waits up to fifteen seconds for its worker to dial in, and a
/// desktop switch inside that window installs a replacement. When the wait then
/// fails, restarting on the abandoned worker's behalf would kill the one that
/// is actually running.
#[tokio::test]
async fn a_replaced_worker_does_not_get_to_order_a_restart() {
    let (manager, _worker_rx) = test_manager();
    let abandoned = install_worker(&manager).await;
    let running = install_worker(&manager).await;

    assert!(!manager.restart_is_still_wanted(abandoned).await);
    assert!(manager.restart_is_still_wanted(running).await);
}

/// A start that failed before installing anything leaves no worker to compare
/// against. The pipe server reporting the failure afterwards is the only thing
/// that will bring a worker back, so it has to be believed.
#[tokio::test]
async fn a_failed_start_stays_recoverable() {
    let (manager, _worker_rx) = test_manager();
    let never_installed = manager.mint_worker().incarnation();

    assert!(manager.restart_is_still_wanted(never_installed).await);
}

/// Incarnations are minted, never recycled. A reused number would make a late
/// message from a long-dead worker indistinguishable from a live one.
#[tokio::test]
async fn incarnations_are_never_reused() {
    let (manager, _worker_rx) = test_manager();

    let minted: Vec<_> = (0..8)
        .map(|_| manager.mint_worker().incarnation())
        .collect();
    let distinct: std::collections::HashSet<_> = minted.iter().copied().collect();

    assert_eq!(distinct.len(), minted.len());
}

/// The sink is what makes the whole scheme work: a bridge serves one worker, so
/// it stamps what it forwards without the worker having to know its own name.
#[tokio::test]
async fn a_sink_stamps_what_it_forwards() {
    let (manager, mut worker_rx) = test_manager();
    let first = manager.mint_worker();
    let second = manager.mint_worker();

    assert_ne!(
        first.incarnation(),
        second.incarnation(),
        "two workers sharing one name would leave the stamp saying nothing",
    );
    assert!(first.send(WorkerToService::Ready));
    assert!(second.send(WorkerToService::Ready));

    assert_eq!(
        worker_rx.recv().await.expect("first message").incarnation,
        first.incarnation(),
    );
    assert_eq!(
        worker_rx.recv().await.expect("second message").incarnation,
        second.incarnation(),
    );
}
