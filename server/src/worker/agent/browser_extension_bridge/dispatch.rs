//! Cancellation fences the queue; once socket submission starts, outcome may be unknown.
use super::*;
use std::sync::atomic::AtomicU8;
use std::time::Instant;

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const CANCELLED: u8 = 2;
pub(super) const CAPACITY: usize = 32;
pub(super) const MAX_CALL_IDS: usize = 4096;

pub(super) struct Outbound {
    payload: String,
    pub ticket: Arc<AtomicU8>,
    deadline: Instant,
    guard: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}
impl Outbound {
    pub fn new(payload: String) -> Self {
        Self {
            payload,
            ticket: Arc::new(AtomicU8::new(QUEUED)),
            deadline: Instant::now() + BROWSER_EXTENSION_CALL_TIMEOUT,
            guard: None,
        }
    }
    pub fn guarded(payload: String, guard: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        Self {
            guard: Some(guard),
            ..Self::new(payload)
        }
    }
    pub fn try_begin(&self) -> bool {
        if self.ticket.load(Ordering::Acquire) != QUEUED {
            return false;
        }
        if self.guard.as_ref().is_some_and(|guard| !guard()) || Instant::now() >= self.deadline {
            cancel(&self.ticket);
            return false;
        }
        self.ticket
            .compare_exchange(QUEUED, STARTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}
impl std::fmt::Debug for Outbound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Outbound")
            .field("state", &self.ticket.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl std::ops::Deref for Outbound {
    type Target = str;
    fn deref(&self) -> &str {
        &self.payload
    }
}

pub(super) fn cancel(ticket: &AtomicU8) {
    let _ = ticket.compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire);
}

pub(super) fn classify_failure(
    ticket: &AtomicU8,
    error: BrowserExtensionBridgeError,
) -> BrowserExtensionBridgeError {
    // Win cancellation before claiming that a queued operation never started.
    cancel(ticket);
    if ticket.load(Ordering::Acquire) == CANCELLED {
        BrowserExtensionBridgeError::NotSubmitted(Box::new(error))
    } else {
        error
    }
}

#[derive(Debug)]
pub(super) struct Pending {
    pub reply: oneshot::Sender<Result<serde_json::Value, BrowserExtensionBridgeError>>,
    pub ticket: Arc<AtomicU8>,
}

pub(super) struct Waiter<'a> {
    pub broker: &'a BrowserExtensionBroker,
    pub id: &'a str,
    pub ticket: Arc<AtomicU8>,
}
impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        cancel(&self.ticket);
        let mut state = self
            .broker
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .pending
            .get(self.id)
            .is_some_and(|pending| Arc::ptr_eq(&pending.ticket, &self.ticket))
        {
            state.pending.remove(self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn authority_revoked_while_queued_rejects_without_socket_submission() {
        let authorized = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let current = authorized.clone();
        let outbound = Outbound::guarded(
            "private browser action".into(),
            Arc::new(move || current.load(Ordering::Acquire)),
        );
        let broker = BrowserExtensionBroker::default();
        let (reply, result) = oneshot::channel();
        broker.state.lock().unwrap().pending.insert(
            "call".into(),
            Pending {
                reply,
                ticket: outbound.ticket.clone(),
            },
        );
        authorized.store(false, Ordering::Release);
        assert!(!outbound.try_begin());
        broker.reject_queued(&outbound);
        let error = result.await.unwrap().unwrap_err();
        assert!(!error.may_have_started());
        assert!(broker.state.lock().unwrap().pending.is_empty());
        authorized.store(true, Ordering::Release);
        assert!(!outbound.try_begin());
        assert!(!format!("{outbound:?}").contains("private browser action"));
    }

    #[test]
    fn cancellation_prevents_late_dispatch_but_never_claims_to_cancel_a_started_send() {
        let queued = Outbound::new("action".into());
        cancel(&queued.ticket);
        assert!(!queued.try_begin());
        let started = Outbound::new("action".into());
        assert!(started.try_begin());
        cancel(&started.ticket);
        assert_eq!(started.ticket.load(Ordering::Acquire), STARTED);
        assert!(!started.try_begin());
    }

    #[test]
    fn failure_classification_cancels_queue_and_preserves_unknown_after_submission() {
        let queued = Outbound::new("action".into());
        let error = classify_failure(&queued.ticket, BrowserExtensionBridgeError::Timeout);
        assert!(!error.may_have_started());
        assert!(!queued.try_begin());
        let started = Outbound::new("action".into());
        assert!(started.try_begin());
        let error = classify_failure(&started.ticket, BrowserExtensionBridgeError::Disconnected);
        assert!(error.may_have_started());
        assert!(BrowserExtensionBridgeError::DuplicateRequest.may_have_started());
        assert!(!BrowserExtensionBridgeError::Busy.may_have_started());
    }

    #[tokio::test]
    async fn full_queue_rejects_extra_work_and_expired_work_does_not_start() {
        let (sender, mut receiver) = mpsc::channel(CAPACITY);
        for _ in 0..CAPACITY {
            sender.try_send(Outbound::new("action".into())).unwrap();
        }
        assert!(matches!(
            sender.try_send(Outbound::new("extra".into())),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        let mut queued = receiver.recv().await.unwrap();
        queued.deadline = Instant::now();
        assert!(!queued.try_begin());
    }

    #[test]
    fn cancelled_old_waiter_cannot_remove_a_new_connection_pending_request() {
        let broker = BrowserExtensionBroker::default();
        let old = Outbound::new("old".into());
        let new = Outbound::new("new".into());
        let (reply, _receiver) = oneshot::channel();
        broker.state.lock().unwrap().pending.insert(
            "call".into(),
            Pending {
                reply,
                ticket: new.ticket.clone(),
            },
        );
        drop(Waiter {
            broker: &broker,
            id: "call",
            ticket: old.ticket.clone(),
        });
        assert!(!old.try_begin());
        assert!(broker.state.lock().unwrap().pending.contains_key("call"));
        assert!(new.try_begin());
    }
}
