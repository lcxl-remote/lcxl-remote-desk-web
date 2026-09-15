//! Isolate poll frames while retaining cancellation ownership.
use std::future::Future;

pub(crate) async fn run<F>(future: F) -> Result<F::Output, tokio::task::JoinError>
where
    F: Future + 'static,
    F::Output: 'static,
{
    // Dropping this set aborts the continuation when its owning scan is cancelled.
    let mut task = tokio::task::JoinSet::new();
    task.spawn_local(future);
    task.join_next().await.expect("one owned continuation")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    struct Dropped(Option<oneshot::Sender<()>>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }

    #[actix_web::test]
    async fn cancelling_scan_drops_started_continuation() {
        let (started, ready) = oneshot::channel();
        let (dropped, finished) = oneshot::channel();
        let scan = actix_web::rt::spawn(run(async move {
            let _lifetime = Dropped(Some(dropped));
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        scan.abort();
        assert!(scan.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), finished)
            .await
            .unwrap()
            .unwrap();
    }

    #[actix_web::test]
    async fn completed_continuation_returns_its_result() {
        assert_eq!(run(async { 42 }).await.unwrap(), 42);
    }

    #[actix_web::test]
    async fn panicked_continuation_does_not_become_success() {
        let result = run(async { panic!("synthetic continuation failure") }).await;
        assert!(result.unwrap_err().is_panic());
    }
}
