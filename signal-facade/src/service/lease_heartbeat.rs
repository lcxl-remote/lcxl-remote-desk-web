//! Renewal retries remain bounded by the last confirmed lease.

use desk_agent_protocol::{AgentError, AgentErrorKind};
use std::{future::Future, time::Duration};
use tokio::time::{Instant, sleep, timeout_at};

pub fn renew_until_lost<'a, F, R>(
    interval: Duration,
    ttl: Duration,
    mut renew: F,
) -> std::pin::Pin<Box<dyn Future<Output = AgentError> + 'a>>
where
    F: FnMut() -> R + 'a,
    R: Future<Output = Result<(), AgentError>> + 'a,
{
    desk_diagnose_core::future::phase(|| async move {
        let mut deadline = Instant::now() + ttl;
        loop {
            sleep(interval.min(deadline.saturating_duration_since(Instant::now()))).await;
            let started = Instant::now();
            match timeout_at(deadline, renew()).await {
                Ok(Ok(())) if started < deadline => deadline = started + ttl,
                Ok(Err(error)) if error.retryable && Instant::now() < deadline => {
                    log::warn!("[agent-lease] renewal deferred: kind={:?}", error.kind);
                }
                Ok(Err(error)) if !error.retryable => return error,
                _ => {
                    return AgentError {
                        kind: AgentErrorKind::SessionUnavailable,
                        message: "The session lease renewal deadline elapsed.".into(),
                        retryable: false,
                        safe_for_model: false,
                        error_code: None,
                    };
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn failure(retryable: bool) -> AgentError {
        AgentError {
            kind: AgentErrorKind::TransportError,
            message: "synthetic storage failure".into(),
            retryable,
            safe_for_model: false,
            error_code: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failure_recovers_without_extending_an_unconfirmed_lease() {
        let attempts = Cell::new(0);
        let start = Instant::now();
        let error = renew_until_lost(Duration::from_secs(30), Duration::from_secs(90), || {
            attempts.set(attempts.get() + 1);
            let attempt = attempts.get();
            async move {
                if attempt == 2 {
                    Ok(())
                } else {
                    Err(failure(true))
                }
            }
        })
        .await;
        assert_eq!(attempts.get(), 5);
        assert_eq!(Instant::now() - start, Duration::from_secs(150));
        assert_eq!(error.kind, AgentErrorKind::SessionUnavailable);
    }

    #[tokio::test(start_paused = true)]
    async fn lost_lease_stops_immediately_and_stalled_storage_cannot_run_past_deadline() {
        let start = Instant::now();
        assert!(
            !renew_until_lost(Duration::from_secs(30), Duration::from_secs(90), || async {
                Err(failure(false))
            })
            .await
            .retryable
        );
        assert_eq!(Instant::now() - start, Duration::from_secs(30));
        let start = Instant::now();
        let error = renew_until_lost(
            Duration::from_secs(30),
            Duration::from_secs(90),
            std::future::pending::<Result<(), AgentError>>,
        )
        .await;
        assert_eq!(Instant::now() - start, Duration::from_secs(90));
        assert_eq!(error.kind, AgentErrorKind::SessionUnavailable);
    }
}
