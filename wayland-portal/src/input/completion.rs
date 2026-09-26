//! Awaitable input on the same serial queue as interactive desktop input.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{PortalInputEvent, PortalInputSender, QueuedInput, dispatch};
use crate::{AuthorizationTarget, LivePortalSession, PortalError};

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const CANCELLED: u8 = 2;
const MAX_DURATION: Duration = Duration::from_secs(5);

/// A successful notification is a Portal reply, not proof of a UI effect.
#[derive(Debug)]
pub struct PortalInputReceipt {
    pub completed_at: Instant,
}

#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct PortalInputFailure {
    pub error: PortalError,
    /// Once submitted, errors must never cause automatic replay.
    pub possibly_started: bool,
}

type Authority = Box<dyn Fn() -> Result<(), PortalError> + Send + Sync>;

pub(super) struct GuardedInput {
    events: Vec<PortalInputEvent>,
    deadline: Instant,
    cancel: CancellationToken,
    authority: Authority,
    state: Arc<AtomicU8>,
    reply: oneshot::Sender<Result<PortalInputReceipt, PortalInputFailure>>,
}

struct CancelOnDrop(Arc<AtomicU8>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self
            .0
            .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire);
    }
}

impl PortalInputSender {
    /// The authority callback rechecks the caller's lease and session generation
    /// immediately before dispatch. It must be bounded and must not block.
    /// Single events cannot leave a key or button held; use a balanced batch for
    /// clicks, chords, and drags.
    pub async fn submit_guarded(
        &self,
        event: PortalInputEvent,
        timeout: Duration,
        cancel: CancellationToken,
        authority: impl Fn() -> Result<(), PortalError> + Send + Sync + 'static,
    ) -> Result<PortalInputReceipt, PortalInputFailure> {
        self.submit_guarded_batch(vec![event], timeout, cancel, authority)
            .await
    }

    /// A batch occupies the serial queue until completion or cleanup. Key and
    /// button transitions must be balanced; no held state escapes a batch.
    pub async fn submit_guarded_batch(
        &self,
        events: Vec<PortalInputEvent>,
        timeout: Duration,
        cancel: CancellationToken,
        authority: impl Fn() -> Result<(), PortalError> + Send + Sync + 'static,
    ) -> Result<PortalInputReceipt, PortalInputFailure> {
        if !super::pressed::valid_batch(&events) {
            return Err(failure(false, "Invalid or unbalanced Portal input batch"));
        }
        let deadline = Instant::now() + timeout.min(MAX_DURATION);
        let state = Arc::new(AtomicU8::new(QUEUED));
        let _cancel_on_drop = CancelOnDrop(state.clone());
        let (reply, receiver) = oneshot::channel();
        let request = GuardedInput {
            events,
            deadline,
            cancel: cancel.clone(),
            authority: Box::new(authority),
            state: state.clone(),
            reply,
        };
        self.tx
            .try_send(QueuedInput::Guarded(request))
            .map_err(|_| failure(false, "Portal input queue busy or closed"))?;
        tokio::select! {
            biased;
            result = receiver => result.unwrap_or_else(|_| Err(failure(state.load(Ordering::Acquire) == STARTED, "Portal input worker stopped"))),
            _ = cancel.cancelled() => Err(cancel_waiter(&state)),
            _ = tokio::time::sleep_until(deadline.into()) => Err(cancel_waiter(&state)),
        }
    }
}

fn failure(possibly_started: bool, reason: &str) -> PortalInputFailure {
    PortalInputFailure {
        error: PortalError::Backend(reason.into()),
        possibly_started,
    }
}

fn cancel_waiter(state: &AtomicU8) -> PortalInputFailure {
    let started = state
        .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
        .is_err_and(|current| current == STARTED);
    failure(
        started,
        if started {
            "Portal input wait ended after submission; outcome unknown; do not replay"
        } else {
            "Portal input cancelled before submission"
        },
    )
}

impl GuardedInput {
    pub(super) fn reject_interactive_hold(self) {
        let _ = self.reply.send(Err(failure(
            false,
            "Interactive input has held keys or buttons",
        )));
    }

    pub(super) async fn execute(self, session: &dyn LivePortalSession) -> bool {
        let retired = AtomicBool::new(false);
        let result = self.execute_inner(session, &retired).await;
        let _ = self.reply.send(result);
        retired.load(Ordering::Acquire)
    }

    async fn execute_inner(
        &self,
        session: &dyn LivePortalSession,
        retired: &AtomicBool,
    ) -> Result<PortalInputReceipt, PortalInputFailure> {
        let closed = session.closure_token();
        if session.target() != AuthorizationTarget::ScreenAndInput {
            return Err(PortalInputFailure {
                error: PortalError::InputDevicesNotGranted,
                possibly_started: false,
            });
        }
        let mut pressed = super::pressed::Pressed::default();
        let mut started = false;
        let mut result = Ok(());
        for event in &self.events {
            result = self.check_authority(&closed, started);
            if result.is_err() {
                break;
            }
            if !started {
                if self
                    .state
                    .compare_exchange(QUEUED, STARTED, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    result = Err(failure(false, "Portal input cancelled in queue"));
                    break;
                }
                started = true;
            }
            // Record presses before the call: even a failed reply can mean the
            // compositor accepted the press. Forget releases only after success.
            pressed.before(event);
            result = tokio::select! {
                biased;
                _ = closed.cancelled() => Err(PortalError::Cancelled),
                result = dispatch(session, event.clone()) => result,
                _ = tokio::time::sleep_until(self.deadline.into()) => {
                    retire(session, retired).await;
                    Err(PortalError::Backend("Portal input completion timed out; session retired".into()))
                }
            }.map_err(|error| PortalInputFailure { error, possibly_started: true });
            if result.is_err() {
                break;
            }
            pressed.after(event);
        }
        // Cleanup is restricted to this batch's held state and the original
        // session. Revocation forbids further actions, but must not strand keys.
        if !pressed.is_empty() && !closed.is_cancelled() {
            let cleanup = async {
                for event in pressed.releases() {
                    dispatch(session, event).await?;
                }
                Ok::<(), PortalError>(())
            };
            if !matches!(
                tokio::time::timeout(MAX_DURATION, cleanup).await,
                Ok(Ok(()))
            ) {
                retire(session, retired).await;
                result = Err(failure(
                    started,
                    "Portal input cleanup failed; session retired; outcome unknown",
                ));
            }
        }
        result?;
        Ok(PortalInputReceipt {
            completed_at: Instant::now(),
        })
    }

    fn check_authority(
        &self,
        closed: &CancellationToken,
        started: bool,
    ) -> Result<(), PortalInputFailure> {
        let expired = || {
            self.cancel.is_cancelled()
                || closed.is_cancelled()
                || Instant::now() >= self.deadline
                || self.reply.is_closed()
        };
        if expired() {
            return Err(failure(started, "Portal input authority expired"));
        }
        (self.authority)().map_err(|error| PortalInputFailure {
            error,
            possibly_started: started,
        })?;
        if expired() {
            return Err(failure(
                started,
                "Portal input authority expired during revalidation",
            ));
        }
        Ok(())
    }
}

async fn retire(session: &dyn LivePortalSession, retired: &AtomicBool) {
    retired.store(true, Ordering::Release);
    let _ = tokio::time::timeout(MAX_DURATION, session.close()).await;
    session.closure_token().cancel();
}
