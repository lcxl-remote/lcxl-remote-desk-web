//! Heap construction boundaries for large asynchronous state machines.

use std::{future::Future, pin::Pin};

/// Construct and pin a child state outside its caller's stack frame.
///
/// The factory runs immediately. Polling, cancellation and dropping remain
/// owned by the caller; this does not create or detach a runtime task.
#[inline(never)]
pub fn boxed<F, T>(create: F) -> Pin<Box<T>>
where
    F: FnOnce() -> T,
    T: Future,
{
    Box::pin(create())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll, Waker},
    };

    struct OnDrop(Arc<AtomicUsize>);

    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn construction_is_immediate_and_cancellation_drops_the_same_state() {
        let constructed = AtomicUsize::new(0);
        let dropped = Arc::new(AtomicUsize::new(0));
        let state = OnDrop(dropped.clone());
        let mut child = boxed(|| {
            constructed.fetch_add(1, Ordering::SeqCst);
            async move {
                let _state = state;
                std::future::pending::<()>().await;
            }
        });
        assert_eq!(constructed.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert!(matches!(
            child.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        drop(child);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn result_is_returned_without_an_extra_poll_or_task() {
        let mut child = boxed(|| std::future::ready(Err::<(), _>("original error")));
        assert_eq!(
            child.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err("original error"))
        );
    }
}
