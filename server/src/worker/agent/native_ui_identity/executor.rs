//! Bounded waiting for a serial native executor; timeout never means cancellation.
use super::error;
use desk_agent_protocol::AgentError;
use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    time::Duration,
};

const QUEUED: u8 = 0;
const RUNNING: u8 = 1;
const FINISHED: u8 = 2;
const EXPIRED: u8 = 3;
const CANCELLED: u8 = 4;
type Job = Box<dyn FnOnce() + Send>;
pub(super) type MutationGuard = Box<dyn Fn() -> Result<(), AgentError> + Send>;

#[derive(Debug)]
pub(crate) struct NativeExecutionError {
    pub(crate) error: AgentError,
    pub(crate) started: bool,
}
impl NativeExecutionError {
    pub(crate) fn maybe_started(error: AgentError) -> Self {
        Self {
            error,
            started: true,
        }
    }
    pub(super) fn not_started(error: AgentError) -> Self {
        Self {
            error,
            started: false,
        }
    }
    pub(crate) fn result_class(
        &self,
    ) -> desk_agent_protocol::computer_use::ComputerActionResultClass {
        use desk_agent_protocol::computer_use::ComputerActionResultClass;
        if self.started {
            ComputerActionResultClass::OutcomeUnknown
        } else {
            ComputerActionResultClass::DefinitelyNotStarted
        }
    }
}
struct Context {
    state: Arc<AtomicU8>,
    guards: Vec<MutationGuard>,
}
thread_local! { static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) }; }

pub(super) fn is_current() -> bool {
    CONTEXT.with(|context| context.borrow().is_some())
}

pub(super) fn with_guard<T>(
    guard: MutationGuard,
    operation: impl FnOnce() -> Result<T, AgentError>,
) -> Result<T, AgentError> {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            CONTEXT.with(|context| {
                if let Some(context) = context.borrow_mut().as_mut() {
                    context.guards.pop();
                }
            });
        }
    }
    CONTEXT.with(|context| {
        context
            .borrow_mut()
            .as_mut()
            .expect("native context")
            .guards
            .push(guard)
    });
    let _restore = Restore;
    check_mutation()?;
    operation()
}

pub(crate) fn check_mutation() -> Result<(), AgentError> {
    CONTEXT.with(|context| {
        let context = context.borrow();
        if let Some(context) = context.as_ref() {
            if context.state.load(Ordering::Acquire) != RUNNING {
                return Err(error(
                    "native UI request expired before mutation; this mutation was not executed",
                ));
            }
            for guard in &context.guards {
                guard()?;
            }
            if context.state.load(Ordering::Acquire) != RUNNING {
                return Err(error(
                    "native UI request expired during authority checks; mutation was not executed",
                ));
            }
        }
        Ok(())
    })
}

struct Current;
impl Drop for Current {
    fn drop(&mut self) {
        CONTEXT.with(|context| {
            if let Some(context) = context.borrow_mut().take() {
                context.state.store(FINISHED, Ordering::Release);
            }
        });
    }
}

pub(super) struct Executor {
    sender: mpsc::SyncSender<Job>,
    stalled: Mutex<Option<Arc<AtomicU8>>>,
}
impl Executor {
    pub(super) fn new(initialize: impl FnOnce() -> bool + Send + 'static) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel::<Job>(8);
        std::thread::Builder::new()
            .name("native-ui-identity".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                if !initialize() {
                    return;
                }
                while let Ok(job) = receiver.recv() {
                    job();
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            sender,
            stalled: Mutex::new(None),
        })
    }

    #[cfg(test)]
    pub(super) fn run<T: Send + 'static>(
        &self,
        timeout: Duration,
        operation: impl FnOnce() -> Result<T, AgentError> + Send + 'static,
        guard: Option<MutationGuard>,
    ) -> Result<T, AgentError> {
        self.run_classified(timeout, operation, guard)
            .map_err(|failure| failure.error)
    }

    pub(super) fn run_classified<T: Send + 'static>(
        &self,
        timeout: Duration,
        operation: impl FnOnce() -> Result<T, AgentError> + Send + 'static,
        guard: Option<MutationGuard>,
    ) -> Result<T, NativeExecutionError> {
        let state = Arc::new(AtomicU8::new(QUEUED));
        let queued_state = Arc::clone(&state);
        let (reply, result) = mpsc::sync_channel(1);
        {
            let mut stalled = self.stalled.lock().map_err(|_| {
                NativeExecutionError::not_started(error("native UI admission unavailable"))
            })?;
            if stalled
                .as_ref()
                .is_some_and(|state| state.load(Ordering::Acquire) != FINISHED)
            {
                return Err(NativeExecutionError::not_started(error(
                    "previous native UI request is still running; this request was not executed",
                )));
            }
            *stalled = None;
            self.sender
                .try_send(Box::new(move || {
                    if queued_state
                        .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        return;
                    }
                    CONTEXT.with(|context| {
                        *context.borrow_mut() = Some(Context {
                            state: queued_state,
                            guards: guard.into_iter().collect(),
                        })
                    });
                    let _current = Current;
                    let output = check_mutation()
                        .map_err(NativeExecutionError::not_started)
                        .and_then(|_| operation().map_err(NativeExecutionError::maybe_started));
                    let _ = reply.send(output);
                }))
                .map_err(|_| {
                    NativeExecutionError::not_started(error(
                        "native UI worker is busy or unavailable; this request was not executed",
                    ))
                })?;
        }
        match result.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let failure = error("native UI worker stopped before returning a result");
                if state.load(Ordering::Acquire) == QUEUED {
                    Err(NativeExecutionError::not_started(failure))
                } else {
                    Err(NativeExecutionError::maybe_started(failure))
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if state
                    .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Err(NativeExecutionError::not_started(error(
                        "native UI request expired in the queue; this request was not executed",
                    )));
                }
                let _ =
                    state.compare_exchange(RUNNING, EXPIRED, Ordering::AcqRel, Ordering::Acquire);
                *self.stalled.lock().map_err(|_| {
                    NativeExecutionError::maybe_started(error("native UI admission unavailable"))
                })? = Some(state);
                Err(NativeExecutionError::maybe_started(error(
                    "native UI request timed out after starting; outcome unknown; do not replay the operation",
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn identical_errors_are_classified_by_execution_stage_not_message() {
        let executor = Executor::new(|| true).unwrap();
        let rejected = executor
            .run_classified(
                Duration::from_secs(1),
                || Ok(()),
                Some(Box::new(|| Err(error("same error")))),
            )
            .unwrap_err();
        let started = executor
            .run_classified::<()>(Duration::from_secs(1), || Err(error("same error")), None)
            .unwrap_err();
        assert_eq!(rejected.error.message, started.error.message);
        assert_eq!(
            rejected.result_class(),
            desk_agent_protocol::computer_use::ComputerActionResultClass::DefinitelyNotStarted
        );
        assert_eq!(
            started.result_class(),
            desk_agent_protocol::computer_use::ComputerActionResultClass::OutcomeUnknown
        );
    }

    #[test]
    fn failed_initialization_never_reports_started() {
        let executor = Executor::new(|| false).unwrap();
        let result = executor
            .run_classified::<()>(Duration::from_secs(1), || panic!("must not execute"), None)
            .unwrap_err();
        assert!(!result.started);
    }

    #[test]
    fn expired_queue_entry_never_executes_after_the_worker_recovers() {
        let executor = Arc::new(Executor::new(|| true).unwrap());
        let (started, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let first = Arc::clone(&executor);
        let handle = std::thread::spawn(move || {
            first.run(
                Duration::from_secs(2),
                move || {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok(())
                },
                None,
            )
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let late = Arc::clone(&called);
        let error = executor
            .run_classified(
                Duration::from_millis(20),
                move || {
                    late.store(true, Ordering::Release);
                    Ok(())
                },
                None,
            )
            .unwrap_err();
        assert!(!error.started);
        assert_eq!(
            error.result_class(),
            desk_agent_protocol::computer_use::ComputerActionResultClass::DefinitelyNotStarted
        );
        assert!(error.error.message.contains("expired in the queue"));
        release.send(()).unwrap();
        handle.join().unwrap().unwrap();
        executor
            .run(Duration::from_secs(1), || Ok(()), None)
            .unwrap();
        assert!(!called.load(Ordering::Acquire));
    }

    #[test]
    fn started_timeout_blocks_admission_and_prevents_later_mutation() {
        let executor = Executor::new(|| true).unwrap();
        let (release, wait) = mpsc::channel();
        let (checked, outcome) = mpsc::channel();
        let error = executor
            .run_classified(
                Duration::from_millis(30),
                move || {
                    wait.recv().unwrap();
                    checked.send(check_mutation().is_err()).unwrap();
                    Ok(())
                },
                None,
            )
            .unwrap_err();
        assert!(error.started);
        assert_eq!(
            error.result_class(),
            desk_agent_protocol::computer_use::ComputerActionResultClass::OutcomeUnknown
        );
        assert!(error.error.message.contains("outcome unknown"));
        assert!(!error.error.retryable);
        assert!(
            executor
                .run(Duration::from_secs(1), || Ok(()), None)
                .unwrap_err()
                .message
                .contains("still running")
        );
        release.send(()).unwrap();
        assert!(outcome.recv_timeout(Duration::from_secs(1)).unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while executor
            .stalled
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .load(Ordering::Acquire)
            != FINISHED
        {
            assert!(
                std::time::Instant::now() < deadline,
                "native job did not finish"
            );
            std::thread::yield_now();
        }
        executor
            .run(Duration::from_secs(1), || Ok(()), None)
            .unwrap();
    }

    #[test]
    fn mutation_rechecks_the_current_authority() {
        let executor = Executor::new(|| true).unwrap();
        let valid = Arc::new(AtomicBool::new(true));
        let guard_valid = Arc::clone(&valid);
        let result = executor
            .run(
                Duration::from_secs(1),
                move || {
                    with_guard(Box::new(|| Ok(())), || {
                        valid.store(false, Ordering::Release);
                        check_mutation()
                    })
                },
                Some(Box::new(move || {
                    if guard_valid.load(Ordering::Acquire) {
                        Ok(())
                    } else {
                        Err(error("writer lease revoked"))
                    }
                })),
            )
            .unwrap_err();
        assert_eq!(result.message, "writer lease revoked");
    }

    #[test]
    fn authority_check_that_finishes_after_timeout_cannot_start_the_operation() {
        let executor = Executor::new(|| true).unwrap();
        let (release, wait) = mpsc::channel();
        let called = Arc::new(AtomicBool::new(false));
        let operation_called = Arc::clone(&called);
        let error = executor
            .run(
                Duration::from_millis(100),
                move || {
                    operation_called.store(true, Ordering::Release);
                    Ok(())
                },
                Some(Box::new(move || {
                    wait.recv().unwrap();
                    Ok(())
                })),
            )
            .unwrap_err();
        assert!(error.message.contains("outcome unknown"));
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while executor
            .stalled
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .load(Ordering::Acquire)
            != FINISHED
        {
            assert!(
                std::time::Instant::now() < deadline,
                "native job did not finish"
            );
            std::thread::yield_now();
        }
        assert!(!called.load(Ordering::Acquire));
        executor
            .run(Duration::from_secs(1), || Ok(()), None)
            .unwrap();
    }
}
