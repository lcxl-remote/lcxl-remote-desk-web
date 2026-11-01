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

/// Construct a type-erased execution phase without adding a runtime task.
#[inline(never)]
#[track_caller]
pub fn phase<'a, F, T>(create: F) -> Pin<Box<dyn Future<Output = T::Output> + 'a>>
where
    F: FnOnce() -> T,
    T: Future + 'a,
{
    #[cfg(any(test, feature = "stack-budget"))]
    assert!(
        std::mem::size_of::<T>() <= 16 * 1024,
        "phase future grew to {} bytes before boxing",
        std::mem::size_of::<T>()
    );
    boxed(create)
}

/// A phase that preserves a caller's existing cross-thread task contract.
#[inline(never)]
#[track_caller]
pub fn send_phase<'a, F, T>(create: F) -> Pin<Box<dyn Future<Output = T::Output> + Send + 'a>>
where
    F: FnOnce() -> T,
    T: Future + Send + 'a,
{
    #[cfg(any(test, feature = "stack-budget"))]
    assert!(
        std::mem::size_of::<T>() <= 16 * 1024,
        "phase future grew to {} bytes before boxing",
        std::mem::size_of::<T>()
    );
    boxed(create)
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

    #[test]
    fn erased_phase_retains_immediate_construction_and_drop_ownership() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let state = OnDrop(dropped.clone());
        let mut child = phase(|| async move {
            let _state = state;
            std::future::pending::<()>().await;
        });
        assert!(
            child
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        drop(child);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn send_phase_preserves_send_and_cancellation_ownership() {
        fn require_send<T: Send>(_: &T) {}
        let dropped = Arc::new(AtomicUsize::new(0));
        let state = OnDrop(dropped.clone());
        let mut child = send_phase(|| async move {
            let _state = state;
            std::future::pending::<()>().await;
        });
        require_send(&child);
        assert!(
            child
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(child);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[should_panic(expected = "phase future grew")]
    fn raw_phase_size_guard_observes_state_before_erasure() {
        let oversized = [0u8; 16 * 1024 + 1];
        drop(phase(|| async move {
            std::future::pending::<()>().await;
            std::hint::black_box(oversized);
        }));
    }

    #[test]
    fn security_state_layout_stays_within_its_stack_budget() {
        let sizes = [
            std::mem::size_of::<crate::session::PersistedAgentSession>(),
            std::mem::size_of::<crate::command_confirmation::CommandConfirmation>(),
            std::mem::size_of::<desk_agent_protocol::ExecInput>(),
        ];
        eprintln!("session/confirmation/exec layout bytes: {sizes:?}");
        for (actual, maximum) in sizes.into_iter().zip([3568, 576, 120]) {
            assert!(
                actual <= maximum,
                "security state layout grew to {actual} bytes (budget {maximum})"
            );
        }
    }
}
