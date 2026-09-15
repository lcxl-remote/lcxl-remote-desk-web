//! The actual blocking operation owns its lease until it stops, even when its
//! async waiter is cancelled. A dropped waiter does not prove mutation stopped.
use crate::worker::agent::computer_use_broker::ComputerUseBroker;
use std::sync::Arc;

struct Release {
    broker: Arc<ComputerUseBroker>,
    generation: String,
}

impl Drop for Release {
    fn drop(&mut self) {
        self.broker.release_writer_lease(&self.generation);
    }
}

pub(crate) fn spawn_writer_task<T: Send + 'static>(
    broker: Arc<ComputerUseBroker>,
    generation: String,
    operation: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    let release = Release { broker, generation };
    tokio::task::spawn_blocking(move || {
        let _release = release;
        operation()
    })
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn cancelled_waiter_keeps_writer_until_publication_stops_even_on_panic() {
        use super::*;
        use crate::model::settings::ComputerUseSettings;
        use crate::worker::agent::computer_use_writer::{WriterLeaseRequest, WriterLeaseScope};
        for panic in [false, true] {
            let broker = std::sync::Arc::new(ComputerUseBroker::new());
            let readiness = broker.readiness(&ComputerUseSettings::default(), false, false);
            let request = WriterLeaseRequest {
                scope: WriterLeaseScope::FileWorker,
                work_id: "work".into(),
                action_request_id: "call".into(),
                execution_generation: "publication".into(),
                approved_actor_id: "owner".into(),
                interactive_session_incarnation: readiness.interactive_session_incarnation,
                expires_at: chrono::Utc::now() + chrono::Duration::seconds(30),
            };
            broker.acquire_writer_lease(request.clone()).unwrap();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (stop_tx, stop_rx) = std::sync::mpsc::channel();
            let worker = broker.clone();
            let waiter = tokio::spawn(async move {
                spawn_writer_task(worker, "publication".into(), move || {
                    let _ = started_tx.send(());
                    stop_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    assert!(!panic, "simulated native unwind");
                })
                .await
            });
            started_rx.await.unwrap();
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
            broker.require_writer_lease("publication").unwrap();
            let next = WriterLeaseRequest {
                execution_generation: "next-publication".into(),
                ..request
            };
            assert!(broker.acquire_writer_lease(next.clone()).is_err());
            stop_tx.send(()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while broker.acquire_writer_lease(next.clone()).is_err() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("native completion releases the original writer");
            broker.require_writer_lease("next-publication").unwrap();
            assert!(broker.release_writer_lease("next-publication"));
        }
    }
}
