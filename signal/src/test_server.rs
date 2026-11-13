//! Test-only teardown for in-process fixture HTTP servers.
use actix_web::dev::ServerHandle;
use std::{io, time::Duration};
use tokio::task::JoinHandle;

const SETTLE: Duration = Duration::from_secs(5);

/// Stops a fixture server once its assertions have completed.
///
/// A graceful stop waits for every worker to acknowledge. Under parallel tests
/// that wait has been observed to stay pending after the worker and accept
/// threads had already exited, parking the test forever. Fixtures assert
/// protocol and durable outcomes, not HTTP drain behavior, so stop at once and
/// bound the wait.
pub(crate) async fn stop(handle: &ServerHandle) {
    tokio::time::timeout(SETTLE, handle.stop(false))
        .await
        .expect("fixture server stop did not settle");
}

/// Stops a fixture server and joins the task that ran it.
pub(crate) async fn stop_and_join(handle: &ServerHandle, task: JoinHandle<io::Result<()>>) {
    stop(handle).await;
    tokio::time::timeout(SETTLE, task)
        .await
        .expect("fixture server task did not settle")
        .expect("fixture server task panicked")
        .expect("fixture server failed");
}
