//! Tree invalidations remain live while periodic identity checks are pending.
use super::super::{Result, error};
use futures_util::{Stream, StreamExt};
use std::{future::Future, time::Duration};

pub(super) async fn observe<S, F, R, I>(
    mut events: S,
    mut recheck: R,
    mut invalidate: I,
    interval: Duration,
    deadline: Duration,
) -> Result<()>
where
    S: Stream<Item = Result<bool>> + Unpin,
    F: Future<Output = Result<()>>,
    R: FnMut() -> F,
    I: FnMut() -> Result<()>,
{
    loop {
        // Do not restart the timer or the lookup on each event. A busy tree
        // cannot postpone the identity check or extend its deadline.
        let probe = async {
            tokio::time::sleep(interval).await;
            tokio::time::timeout(deadline, recheck())
                .await
                .map_err(|_| error("AT-SPI lifecycle identity check timed out"))?
        };
        tokio::pin!(probe);
        loop {
            tokio::select! {
                biased;
                result = &mut probe => { result?; break; }
                event = events.next() => {
                    if event.ok_or_else(|| error("AT-SPI event stream ended"))?? {
                        invalidate()?;
                    }
                    // A continuously ready stream must still let Tokio service
                    // timers and other workers, including the probe deadline.
                    tokio::task::yield_now().await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::{Notify, mpsc};

    #[tokio::test]
    async fn tree_change_is_processed_while_identity_lookup_is_stalled() {
        let (send, recv) = mpsc::unbounded_channel();
        let stream = futures_util::stream::unfold(recv, |mut recv| async {
            recv.recv().await.map(|event| (event, recv))
        })
        .boxed();
        let entered = Arc::new(Notify::new());
        let invalidated = Arc::new(Notify::new());
        let checks = Arc::new(AtomicUsize::new(0));
        let worker = {
            let entered = entered.clone();
            let invalidated = invalidated.clone();
            let checks = checks.clone();
            tokio::spawn(observe(
                stream,
                move || {
                    checks.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    std::future::pending::<Result<()>>()
                },
                move || {
                    invalidated.notify_one();
                    Ok(())
                },
                Duration::ZERO,
                Duration::from_secs(5),
            ))
        };
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        send.send(Ok(true)).unwrap();
        let observed = tokio::time::timeout(Duration::from_secs(1), invalidated.notified()).await;
        // Close the real event stream rather than leave a task detached on failure.
        drop(send);
        let result = tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(
            observed.is_ok(),
            "tree invalidation waited for the blocked lookup"
        );
        assert!(result.unwrap_err().message.contains("event stream ended"));
        assert_eq!(
            checks.load(Ordering::SeqCst),
            1,
            "an event restarted the lookup"
        );
    }

    #[tokio::test]
    async fn continuous_events_cannot_hide_a_stalled_identity_deadline() {
        let mut changes = 0;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            observe(
                futures_util::stream::repeat_with(|| Ok(true)),
                || std::future::pending::<Result<()>>(),
                || {
                    changes += 1;
                    Ok(())
                },
                Duration::ZERO,
                Duration::from_millis(20),
            ),
        )
        .await
        .unwrap();
        assert!(changes > 0);
        assert!(
            result
                .unwrap_err()
                .message
                .contains("identity check timed out")
        );
    }

    #[tokio::test]
    async fn identity_failure_ends_monitor_even_without_tree_events() {
        let result = observe(
            futures_util::stream::pending(),
            || async { Err(error("identity replaced")) },
            || Ok(()),
            Duration::ZERO,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.unwrap_err().message, "identity replaced");
    }
}
