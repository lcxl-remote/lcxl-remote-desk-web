//! TURN runtime supervisor: a single-owner actor that converges this process's
//! TURN runtime to the desired configuration, with a watchdog that rebuilds the
//! actor if it ever panics.
//!
//! Where the desired state comes from is not this module's business — a host
//! derives it from its own settings, a managed node from whatever its control
//! plane publishes. What the module owns is one local runtime's lifecycle.
//!
//! Why this shape (driven by repeated review):
//!
//! - **Durable state lives outside the actor task.** [`TurnApiState`] has no
//!   `Drop` that closes the server, so if the actor task died holding the only
//!   runtime handle, the UDP socket would leak and a rebuilt actor could not
//!   reclaim the port. The runtime handle, the applied/desired revisions, the
//!   generation counter, and the desired-state channel therefore live in a
//!   stable facade ([`TurnSupervisorHandle`]); the actor only borrows them.
//!
//! - **`apply()` uses `watch::Sender::send_replace`.** A plain `send` fails (and
//!   drops the value) when there are zero receivers — exactly the window between
//!   an actor panic and the watchdog rebuilding it. `send_replace` always updates
//!   the channel's latest value, so the new actor's `borrow_and_update` sees the
//!   most recent desired state.
//!
//! - **Single lifecycle op at a time.** The actor is one task, so start/stop/close
//!   never overlap. A start that completes while the desired state has already
//!   moved on is reconciled on the next loop iteration (which closes the
//!   now-unwanted runtime), so a stale start is always explicitly closed — never
//!   orphaned.
//!
//! - **Infinite convergence with capped backoff.** A failed start/stop does not
//!   give up; it retries with exponential backoff (capped), and a new desired
//!   state interrupts the backoff immediately. `applied_revision` honestly
//!   reflects "no runtime" after a failed start, so a later equal-revision reload
//!   does not skip re-converging.
//!
//! - **Shutdown is explicit.** The watchdog holds the desired-state sender, so
//!   dropping every external handle would leave the actor and its runtime alive.
//!   [`TurnSupervisorHandle::shutdown`] is the only way to stop them, and callers
//!   whose lifetime is bounded (an embedded server, a test) must call it.
//!
//! [`TurnApiState`]: crate::model::TurnApiState

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, watch};

use crate::model::TurnInterface;

/// Parameters to start a TURN runtime in this process. Equality drives "needs
/// restart": any field change yields a different value, so the actor tears down
/// and restarts.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnRuntimeParams {
    pub realm: String,
    /// Static auth secret for the TURN REST API, when one is configured.
    pub secret: Option<String>,
    /// Every interface the runtime should serve. A list rather than one
    /// bind/external pair because a host may legitimately serve several, and
    /// collapsing them here would silently drop all but the first.
    pub interfaces: Vec<TurnInterface>,
    pub relay_min_port: u16,
    pub relay_max_port: u16,
    /// Opaque tag for the configuration these params realize.
    ///
    /// Two jobs, both of which need it to change exactly when the configuration
    /// does: it is part of the equality that forces a restart, and it is what
    /// [`SupervisorStatus::applied_identity`] reports, so an observer can tell
    /// whether the *running* runtime realizes the configuration the observer is
    /// about to act on. A managed node fills it with the fingerprint its control
    /// plane advertises, so it never publishes an endpoint whose runtime is
    /// still on the previous secret; a host with nobody to tell can leave it
    /// empty.
    pub identity: String,
}

impl fmt::Debug for TurnRuntimeParams {
    /// Hand-written so the secret cannot reach a log through a `{:?}` on the
    /// params — the supervisor and its callers print state freely, and one of
    /// these fields is a credential.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnRuntimeParams")
            .field("realm", &self.realm)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("interfaces", &self.interfaces)
            .field("relay_min_port", &self.relay_min_port)
            .field("relay_max_port", &self.relay_max_port)
            .field("identity", &self.identity)
            .finish()
    }
}

/// The state the supervisor should converge to. `params == None` means "this
/// process should not run a TURN runtime" (switched off, or no endpoint to
/// serve on).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredState {
    pub revision: u64,
    pub params: Option<TurnRuntimeParams>,
}

/// A running TURN runtime handle; closing it tears down the server + UDP socket.
#[async_trait]
pub trait TurnRuntimeHandle: Send + Sync {
    /// Tear down the runtime. Returns `Err` if the teardown did not complete, so
    /// the supervisor can retain the handle and retry rather than leak the socket
    /// ([`TurnApiState`] has no `Drop`; a dropped-but-not-closed handle would
    /// hold the UDP port forever and wedge any same-port restart).
    ///
    /// [`TurnApiState`]: crate::model::TurnApiState
    async fn close(&self) -> Result<(), String>;
}

/// Starts TURN runtimes. Abstracted for two reasons: the supervisor state
/// machine stays testable without binding real UDP sockets, and each deployment
/// brings its own auth handler — a host authenticates its own connections,
/// a managed node also enforces whatever its control plane decided.
#[async_trait]
pub trait TurnRuntimeDriver: Send + Sync {
    async fn start(&self, params: &TurnRuntimeParams)
    -> Result<Arc<dyn TurnRuntimeHandle>, String>;
}

/// Optional lifecycle observer invoked **before** an existing runtime is torn
/// down. A node in a cluster uses this to withdraw itself from the shared
/// registry before its UDP socket closes, so peers (and lagging readers still on
/// the old config snapshot) are never handed an endpoint whose runtime has
/// already gone away. Best-effort: the implementation must not block the
/// lifecycle on failure.
#[async_trait]
pub trait RuntimeObserver: Send + Sync {
    /// Called just before the current runtime's `close()`.
    async fn before_close(&self);
}

/// Backoff bounds for convergence retries.
#[derive(Debug, Clone, Copy)]
pub struct BackoffConfig {
    pub min: Duration,
    pub max: Duration,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            min: Duration::from_millis(500),
            max: Duration::from_secs(30),
        }
    }
}

struct RunningRuntime {
    handle: Arc<dyn TurnRuntimeHandle>,
    params: TurnRuntimeParams,
    revision: u64,
}

#[derive(Default)]
struct SupervisorState {
    running: Option<RunningRuntime>,
    /// Revision of the applied runtime; `None` means no runtime is running.
    applied_revision: Option<u64>,
    /// Latest desired revision the actor has observed.
    desired_revision: u64,
    /// Last convergence error, surfaced for diagnostics (degraded visibility).
    last_error: Option<String>,
    /// Monotonic generation, bumped per lifecycle op and never reset (survives
    /// actor rebuilds).
    generation: u64,
}

/// Public, read-only status of the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorStatus {
    pub running: bool,
    pub applied_revision: Option<u64>,
    /// [`TurnRuntimeParams::identity`] of the *running* runtime, or `None` when
    /// nothing is running.
    pub applied_identity: Option<String>,
    pub desired_revision: u64,
    pub last_error: Option<String>,
    pub generation: u64,
}

/// Stable, cloneable handle to the supervisor. Holds the durable state and the
/// desired-state channel; survives actor rebuilds.
#[derive(Clone)]
pub struct TurnSupervisorHandle {
    desired_tx: watch::Sender<DesiredState>,
    state: Arc<Mutex<SupervisorState>>,
    /// Set once, by [`Self::shutdown`]. The actor watches it at every park point.
    shutdown_tx: watch::Sender<bool>,
    /// Flipped by the watchdog once the actor has exited for good. A watch
    /// channel rather than a notification so a second `shutdown()` returns
    /// immediately instead of waiting for a signal that already fired.
    finished_rx: watch::Receiver<bool>,
}

impl TurnSupervisorHandle {
    /// Update the desired state. Uses `send_replace` so the value is retained even
    /// if the actor is momentarily absent (panic/rebuild window).
    pub fn apply(&self, desired: DesiredState) {
        self.desired_tx.send_replace(desired);
    }

    /// Current read-only status.
    pub async fn status(&self) -> SupervisorStatus {
        let s = self.state.lock().await;
        SupervisorStatus {
            running: s.running.is_some(),
            applied_revision: s.applied_revision,
            applied_identity: s.running.as_ref().map(|r| r.params.identity.clone()),
            desired_revision: s.desired_revision,
            last_error: s.last_error.clone(),
            generation: s.generation,
        }
    }

    /// Tear down any running runtime and stop the actor, returning once both are
    /// done. Idempotent.
    ///
    /// The teardown goes through the same convergence path as any other change,
    /// so a `close()` that fails is retried while the caller waits rather than
    /// dropping the handle and leaking the UDP port. That means a runtime that
    /// refuses to close makes this wait; a caller that cannot wait forever
    /// should wrap it in `tokio::time::timeout` and accept the leak.
    pub async fn shutdown(&self) {
        self.shutdown_tx.send_replace(true);
        let mut finished = self.finished_rx.clone();
        // `wait_for` checks the current value first, so a shutdown that already
        // completed returns without blocking.
        let _ = finished.wait_for(|done| *done).await;
    }
}

/// Spawn the supervisor: returns a stable handle and starts the watchdog +
/// actor. The watchdog rebuilds the actor (reusing the same desired channel and
/// shared state) if it panics, so a transient bug cannot permanently wedge TURN.
pub fn spawn(
    driver: Arc<dyn TurnRuntimeDriver>,
    initial: DesiredState,
    backoff: BackoffConfig,
) -> TurnSupervisorHandle {
    spawn_with_observer(driver, initial, backoff, None)
}

/// Like [`spawn`], but with a [`RuntimeObserver`] invoked before each runtime
/// teardown (the registry-withdrawal hook on a clustered node).
pub fn spawn_with_observer(
    driver: Arc<dyn TurnRuntimeDriver>,
    initial: DesiredState,
    backoff: BackoffConfig,
    observer: Option<Arc<dyn RuntimeObserver>>,
) -> TurnSupervisorHandle {
    let (desired_tx, _rx) = watch::channel(initial);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (finished_tx, finished_rx) = watch::channel(false);
    let state = Arc::new(Mutex::new(SupervisorState::default()));
    let handle = TurnSupervisorHandle {
        desired_tx: desired_tx.clone(),
        state: state.clone(),
        shutdown_tx,
        finished_rx,
    };

    tokio::spawn(watchdog(
        desired_tx,
        state,
        driver,
        backoff,
        observer,
        shutdown_rx,
        finished_tx,
    ));
    handle
}

/// Supervise the actor task, rebuilding it on panic. Reuses the same
/// `watch::Sender` so callers keep using the same handle, and the shared state
/// (including any running runtime) carries across rebuilds.
#[allow(clippy::too_many_arguments)]
async fn watchdog(
    desired_tx: watch::Sender<DesiredState>,
    state: Arc<Mutex<SupervisorState>>,
    driver: Arc<dyn TurnRuntimeDriver>,
    backoff: BackoffConfig,
    observer: Option<Arc<dyn RuntimeObserver>>,
    shutdown_rx: watch::Receiver<bool>,
    finished_tx: watch::Sender<bool>,
) {
    loop {
        let rx = desired_tx.subscribe();
        let actor = tokio::spawn(run_actor(
            rx,
            state.clone(),
            driver.clone(),
            backoff,
            observer.clone(),
            shutdown_rx.clone(),
        ));
        match actor.await {
            // Clean exit: shut down, or the desired channel was closed (all
            // senders dropped).
            Ok(()) => break,
            Err(join_err) if join_err.is_panic() => {
                log::error!("turn supervisor actor panicked; rebuilding actor");
                // Loop: re-subscribe and respawn. The new actor takes over the
                // existing runtime via the shared state and converges to the
                // latest desired (retained by send_replace).
            }
            Err(_) => break,
        }
    }
    // Only now is nothing left that could start a runtime, so a waiting
    // `shutdown()` may return.
    finished_tx.send_replace(true);
}

async fn run_actor(
    mut desired_rx: watch::Receiver<DesiredState>,
    state: Arc<Mutex<SupervisorState>>,
    driver: Arc<dyn TurnRuntimeDriver>,
    backoff: BackoffConfig,
    observer: Option<Arc<dyn RuntimeObserver>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut delay = backoff.min;
    loop {
        // A shutdown observed at the top of the loop overrides whatever was
        // desired: converge to "no runtime" and leave.
        if *shutdown_rx.borrow_and_update() {
            break;
        }
        let desired = desired_rx.borrow_and_update().clone();
        match converge_once(&driver, &state, &desired, observer.as_ref()).await {
            Ok(()) => {
                delay = backoff.min;
                // Park until the desired state changes or shutdown is requested.
                tokio::select! {
                    res = desired_rx.changed() => {
                        if res.is_err() { break; } // all senders dropped
                    }
                    _ = shutdown_rx.changed() => {}
                }
            }
            Err(e) => {
                {
                    let mut s = state.lock().await;
                    s.last_error = Some(e);
                }
                // Retry after backoff, but wake immediately if desired changes or
                // shutdown is requested.
                tokio::select! {
                    res = desired_rx.changed() => {
                        if res.is_err() { break; }
                    }
                    _ = shutdown_rx.changed() => {}
                    _ = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(backoff.max);
            }
        }
    }

    if *shutdown_rx.borrow() {
        shutdown_runtime(&state, observer.as_ref(), backoff).await;
    }
}

/// Close the running runtime on the way out, retrying a failing close with the
/// same backoff convergence uses: dropping the handle instead would leave the
/// UDP port held until the process exits.
async fn shutdown_runtime(
    state: &Arc<Mutex<SupervisorState>>,
    observer: Option<&Arc<dyn RuntimeObserver>>,
    backoff: BackoffConfig,
) {
    let stop = DesiredState {
        revision: u64::MAX,
        params: None,
    };
    let mut delay = backoff.min;
    loop {
        // A `None` desired state only ever closes, so no driver is needed; the
        // unreachable-driver guard makes that explicit rather than implicit.
        let driver: Arc<dyn TurnRuntimeDriver> = Arc::new(NoStartDriver);
        match converge_once(&driver, state, &stop, observer).await {
            Ok(()) => return,
            Err(e) => {
                log::warn!("turn supervisor shutdown close failed, retrying: {e}");
                let mut s = state.lock().await;
                s.last_error = Some(e);
                drop(s);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(backoff.max);
            }
        }
    }
}

/// Driver for the shutdown path, where the desired state is always "no runtime"
/// and `start` is therefore unreachable.
struct NoStartDriver;

#[async_trait]
impl TurnRuntimeDriver for NoStartDriver {
    async fn start(
        &self,
        _params: &TurnRuntimeParams,
    ) -> Result<Arc<dyn TurnRuntimeHandle>, String> {
        Err("the supervisor is shutting down and starts no runtime".to_string())
    }
}

/// Perform one convergence step: make the runtime match `desired`. Returns `Err`
/// if a close or a start fails (the actor retries after backoff).
async fn converge_once(
    driver: &Arc<dyn TurnRuntimeDriver>,
    state: &Arc<Mutex<SupervisorState>>,
    desired: &DesiredState,
    observer: Option<&Arc<dyn RuntimeObserver>>,
) -> Result<(), String> {
    let current = {
        let mut s = state.lock().await;
        s.desired_revision = desired.revision;
        s.running.as_ref().map(|r| r.params.clone())
    };

    if current.as_ref() == desired.params.as_ref() {
        // Already converged; keep applied_revision in step with the running params'
        // revision (it does not change the runtime, only the recorded revision).
        let mut s = state.lock().await;
        if let Some(running) = s.running.as_mut() {
            running.revision = desired.revision;
            s.applied_revision = Some(desired.revision);
        } else {
            s.applied_revision = None;
        }
        s.last_error = None;
        return Ok(());
    }

    // Close any existing runtime first. The handle is kept in durable state until
    // close *succeeds*: a failed close must not drop the only handle, or the UDP
    // socket leaks (`TurnApiState` has no Drop) and a same-port restart wedges
    // forever. On failure we return Err and the actor retries this close.
    let existing = {
        let s = state.lock().await;
        s.running.as_ref().map(|r| r.handle.clone())
    };
    if let Some(handle) = existing {
        // Withdraw the node from the registry *before* the socket closes, so peers
        // never advertise an endpoint whose runtime has gone away. Best-effort and
        // idempotent (a retried close re-runs it harmlessly).
        if let Some(observer) = observer {
            observer.before_close().await;
        }
        handle
            .close()
            .await
            .map_err(|e| format!("failed to close TURN runtime: {e}"))?;
        let mut s = state.lock().await;
        s.running = None;
        s.applied_revision = None;
    }

    // Bump generation for the new lifecycle state (monotonic across rebuilds).
    {
        let mut s = state.lock().await;
        s.generation += 1;
    }

    // Start the new runtime if desired.
    if let Some(params) = &desired.params {
        let handle = driver.start(params).await?;
        let mut s = state.lock().await;
        s.running = Some(RunningRuntime {
            handle,
            params: params.clone(),
            revision: desired.revision,
        });
        s.applied_revision = Some(desired.revision);
        s.last_error = None;
    } else {
        let mut s = state.lock().await;
        s.applied_revision = None;
        s.last_error = None;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TurnTransport;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fake handle recording closes; can fail the first `close_failures` close
    /// attempts (shared with its driver so a test can inject failures after the
    /// handle is already running).
    struct FakeHandle {
        closes: Arc<AtomicUsize>,
        close_failures: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl TurnRuntimeHandle for FakeHandle {
        async fn close(&self) -> Result<(), String> {
            if self.close_failures.load(Ordering::SeqCst) > 0 {
                self.close_failures.fetch_sub(1, Ordering::SeqCst);
                return Err("injected close failure".into());
            }
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Fake driver: records starts/closes; can fail the first `fail_starts`
    /// attempts, panic the first `panic_starts` attempts, or make a handle's close
    /// fail the first `close_failures` attempts.
    struct FakeDriver {
        starts: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
        close_failures: Arc<AtomicUsize>,
        fail_starts: AtomicUsize,
        panic_starts: AtomicUsize,
    }
    impl FakeDriver {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                starts: Arc::new(AtomicUsize::new(0)),
                closes: Arc::new(AtomicUsize::new(0)),
                close_failures: Arc::new(AtomicUsize::new(0)),
                fail_starts: AtomicUsize::new(0),
                panic_starts: AtomicUsize::new(0),
            })
        }
    }
    #[async_trait]
    impl TurnRuntimeDriver for FakeDriver {
        async fn start(
            &self,
            _params: &TurnRuntimeParams,
        ) -> Result<Arc<dyn TurnRuntimeHandle>, String> {
            if self.panic_starts.load(Ordering::SeqCst) > 0 {
                self.panic_starts.fetch_sub(1, Ordering::SeqCst);
                panic!("injected start panic");
            }
            if self.fail_starts.load(Ordering::SeqCst) > 0 {
                self.fail_starts.fetch_sub(1, Ordering::SeqCst);
                return Err("injected start failure".into());
            }
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(FakeHandle {
                closes: self.closes.clone(),
                close_failures: self.close_failures.clone(),
            }))
        }
    }

    fn iface(listen: &str) -> TurnInterface {
        TurnInterface {
            transport: TurnTransport::UDP,
            listen: listen.into(),
            external: "1.2.3.4:3478".into(),
        }
    }

    fn params(realm: &str) -> TurnRuntimeParams {
        TurnRuntimeParams {
            realm: realm.into(),
            secret: Some("s".into()),
            interfaces: vec![iface("0.0.0.0:3478")],
            relay_min_port: 1024,
            relay_max_port: 2048,
            identity: format!("fp-{realm}"),
        }
    }

    fn fast_backoff() -> BackoffConfig {
        BackoffConfig {
            min: Duration::from_millis(5),
            max: Duration::from_millis(20),
        }
    }

    /// Poll status until `pred` holds or a timeout elapses.
    async fn wait_until(
        h: &TurnSupervisorHandle,
        pred: impl Fn(&SupervisorStatus) -> bool,
    ) -> SupervisorStatus {
        for _ in 0..200 {
            let s = h.status().await;
            if pred(&s) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition not reached; last status: {:?}", h.status().await);
    }

    #[tokio::test]
    async fn enable_starts_runtime() {
        let driver = FakeDriver::new();
        let starts = driver.starts.clone();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("r")),
            },
            fast_backoff(),
        );
        let s = wait_until(&h, |s| s.running).await;
        assert_eq!(s.applied_revision, Some(1));
        // The running runtime's applied identity is exposed for observers.
        assert_eq!(s.applied_identity.as_deref(), Some("fp-r"));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn disable_stops_runtime() {
        let driver = FakeDriver::new();
        let closes = driver.closes.clone();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("r")),
            },
            fast_backoff(),
        );
        wait_until(&h, |s| s.running).await;
        // Kill-switch.
        h.apply(DesiredState {
            revision: 2,
            params: None,
        });
        let s = wait_until(&h, |s| !s.running).await;
        assert_eq!(s.applied_revision, None);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn param_change_restarts_runtime() {
        let driver = FakeDriver::new();
        let starts = driver.starts.clone();
        let closes = driver.closes.clone();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("r1")),
            },
            fast_backoff(),
        );
        wait_until(&h, |s| s.running && s.applied_revision == Some(1)).await;
        h.apply(DesiredState {
            revision: 2,
            params: Some(params("r2")),
        });
        wait_until(&h, |s| s.applied_revision == Some(2)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 2, "restarted");
        assert_eq!(closes.load(Ordering::SeqCst), 1, "old closed");
        h.shutdown().await;
    }

    /// Adding an interface is a configuration change like any other. It is
    /// called out because collapsing the list to a single bind/external — the
    /// shape this type used to have — would make these two params compare equal
    /// and silently skip the restart.
    #[tokio::test]
    async fn adding_an_interface_restarts_runtime() {
        let driver = FakeDriver::new();
        let starts = driver.starts.clone();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("r")),
            },
            fast_backoff(),
        );
        wait_until(&h, |s| s.running).await;

        let mut two = params("r");
        two.interfaces.push(iface("0.0.0.0:3479"));
        h.apply(DesiredState {
            revision: 2,
            params: Some(two),
        });
        wait_until(&h, |s| s.applied_revision == Some(2)).await;
        assert_eq!(
            starts.load(Ordering::SeqCst),
            2,
            "the second interface must reach a runtime"
        );
        h.shutdown().await;
    }

    #[tokio::test]
    async fn start_failure_retries_until_success() {
        let driver = FakeDriver::new();
        driver.fail_starts.store(2, Ordering::SeqCst);
        let starts = driver.starts.clone();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("r")),
            },
            fast_backoff(),
        );
        // After two failures, the third start succeeds.
        let s = wait_until(&h, |s| s.running).await;
        assert_eq!(s.applied_revision, Some(1));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        // While failing, applied_revision honestly stayed None and last_error set.
        // (Now cleared on success.)
        assert!(s.last_error.is_none());
        h.shutdown().await;
    }

    #[tokio::test]
    async fn new_desired_interrupts_failing_retry() {
        let driver = FakeDriver::new();
        // Fail a lot, so without interruption it would stay stuck.
        driver.fail_starts.store(1_000, Ordering::SeqCst);
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("r")),
            },
            fast_backoff(),
        );
        // Observe it is failing (not running) then switch to disabled.
        wait_until(&h, |s| s.last_error.is_some()).await;
        h.apply(DesiredState {
            revision: 2,
            params: None,
        });
        // Disabled converges promptly (no start needed).
        let s = wait_until(&h, |s| s.desired_revision == 2 && !s.running).await;
        assert_eq!(s.applied_revision, None);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn watchdog_rebuilds_actor_after_panic() {
        let driver = FakeDriver::new();
        // First start panics (killing the actor); the watchdog rebuilds it, and
        // the retry succeeds. The runtime handle/state lives in the facade, so the
        // rebuilt actor converges.
        driver.panic_starts.store(1, Ordering::SeqCst);
        let starts = driver.starts.clone();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("r")),
            },
            fast_backoff(),
        );
        let s = wait_until(&h, |s| s.running).await;
        assert_eq!(s.applied_revision, Some(1));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        // Generation advanced across the rebuild (monotonic).
        assert!(s.generation >= 1);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn apply_during_rebuild_window_is_not_lost() {
        // send_replace retains the latest desired even if applied with no live
        // receiver. Here we apply several updates quickly; the final one wins.
        let driver = FakeDriver::new();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("a")),
            },
            fast_backoff(),
        );
        wait_until(&h, |s| s.running).await;
        h.apply(DesiredState {
            revision: 2,
            params: Some(params("b")),
        });
        h.apply(DesiredState {
            revision: 3,
            params: Some(params("c")),
        });
        let s = wait_until(&h, |s| s.applied_revision == Some(3)).await;
        assert!(s.running);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn observer_before_close_runs_before_runtime_close() {
        // The registry-withdraw hook must fire before the socket closes, so a
        // closed runtime is never advertised. Record both events in one ordered log
        // and assert the order.
        use std::sync::Mutex as StdMutex;
        let order: Arc<StdMutex<Vec<&'static str>>> = Arc::new(StdMutex::new(Vec::new()));

        struct OrderHandle {
            order: Arc<StdMutex<Vec<&'static str>>>,
        }
        #[async_trait]
        impl TurnRuntimeHandle for OrderHandle {
            async fn close(&self) -> Result<(), String> {
                self.order.lock().unwrap().push("close");
                Ok(())
            }
        }
        struct OrderDriver {
            order: Arc<StdMutex<Vec<&'static str>>>,
        }
        #[async_trait]
        impl TurnRuntimeDriver for OrderDriver {
            async fn start(
                &self,
                _params: &TurnRuntimeParams,
            ) -> Result<Arc<dyn TurnRuntimeHandle>, String> {
                Ok(Arc::new(OrderHandle {
                    order: self.order.clone(),
                }))
            }
        }
        struct RecObserver {
            order: Arc<StdMutex<Vec<&'static str>>>,
        }
        #[async_trait]
        impl RuntimeObserver for RecObserver {
            async fn before_close(&self) {
                self.order.lock().unwrap().push("before_close");
            }
        }

        let driver = Arc::new(OrderDriver {
            order: order.clone(),
        });
        let observer: Arc<dyn RuntimeObserver> = Arc::new(RecObserver {
            order: order.clone(),
        });
        let h = spawn_with_observer(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("a")),
            },
            fast_backoff(),
            Some(observer),
        );
        wait_until(&h, |s| s.running).await;
        // Kill-switch: tears down the runtime, exercising the pre-close hook.
        h.apply(DesiredState {
            revision: 2,
            params: None,
        });
        wait_until(&h, |s| !s.running).await;
        assert_eq!(
            order.lock().unwrap().clone(),
            vec!["before_close", "close"],
            "registry withdrawal must precede the socket close"
        );
        h.shutdown().await;
    }

    #[tokio::test]
    async fn close_failure_retries_without_losing_handle_or_double_starting() {
        // Runtime A is up. A reconfigure to B requires closing A first, but A's
        // close fails twice before succeeding. The supervisor must keep retrying
        // the close (not drop A's handle, not start B early); once the close
        // finally succeeds it starts B exactly once.
        let driver = FakeDriver::new();
        let starts = driver.starts.clone();
        let closes = driver.closes.clone();
        let close_failures = driver.close_failures.clone();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("a")),
            },
            fast_backoff(),
        );
        wait_until(&h, |s| s.running && s.applied_revision == Some(1)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);

        // Make the next two close attempts on A fail, then reconfigure to B.
        close_failures.store(2, Ordering::SeqCst);
        h.apply(DesiredState {
            revision: 2,
            params: Some(params("b")),
        });

        // While the close keeps failing, B must NOT have started yet and an error
        // is surfaced.
        wait_until(&h, |s| s.last_error.is_some()).await;
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "B must not start until A's close succeeds"
        );

        // Eventually the close succeeds (after two failures), A is torn down, and B
        // starts exactly once.
        let s = wait_until(&h, |s| s.applied_revision == Some(2)).await;
        assert!(s.running);
        assert_eq!(s.applied_identity.as_deref(), Some("fp-b"));
        assert_eq!(starts.load(Ordering::SeqCst), 2, "B started exactly once");
        assert_eq!(closes.load(Ordering::SeqCst), 1, "A closed exactly once");
        assert_eq!(
            close_failures.load(Ordering::SeqCst),
            0,
            "both failures used"
        );
        h.shutdown().await;
    }

    /// Shutdown has to close the runtime, not merely stop watching it: an
    /// embedded server that outlived its own TURN socket would fail to rebind
    /// the port on the next start.
    #[tokio::test]
    async fn shutdown_closes_the_running_runtime_and_stops_the_actor() {
        let driver = FakeDriver::new();
        let closes = driver.closes.clone();
        let starts = driver.starts.clone();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("r")),
            },
            fast_backoff(),
        );
        wait_until(&h, |s| s.running).await;

        h.shutdown().await;
        assert_eq!(closes.load(Ordering::SeqCst), 1, "the runtime was closed");
        assert!(!h.status().await.running);

        // The actor is gone, so a later `apply` cannot resurrect a runtime.
        h.apply(DesiredState {
            revision: 2,
            params: Some(params("r2")),
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "no runtime may start after shutdown"
        );
        assert!(!h.status().await.running);
    }

    /// Idempotent, and callable when nothing is running — the embedded caller
    /// shuts down on every exit path and cannot know which one it is on.
    #[tokio::test]
    async fn shutdown_is_idempotent_and_safe_without_a_runtime() {
        let driver = FakeDriver::new();
        let closes = driver.closes.clone();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: None,
            },
            fast_backoff(),
        );
        wait_until(&h, |s| s.desired_revision == 1).await;

        h.shutdown().await;
        h.shutdown().await;
        assert_eq!(closes.load(Ordering::SeqCst), 0);
    }

    /// A close that keeps failing must not let shutdown return early and drop
    /// the handle: the caller waits, and the runtime is closed once it can be.
    #[tokio::test]
    async fn shutdown_retries_a_failing_close() {
        let driver = FakeDriver::new();
        let closes = driver.closes.clone();
        let close_failures = driver.close_failures.clone();
        let h = spawn(
            driver,
            DesiredState {
                revision: 1,
                params: Some(params("r")),
            },
            fast_backoff(),
        );
        wait_until(&h, |s| s.running).await;

        close_failures.store(2, Ordering::SeqCst);
        h.shutdown().await;
        assert_eq!(closes.load(Ordering::SeqCst), 1, "closed after the retries");
        assert_eq!(
            close_failures.load(Ordering::SeqCst),
            0,
            "both failures used"
        );
        assert!(!h.status().await.running);
    }

    /// The secret is a credential; a `{:?}` on the params reaches logs and panic
    /// messages, so it must not carry the value.
    #[test]
    fn debug_output_redacts_the_secret() {
        let rendered = format!("{:?}", params("r"));
        assert!(
            !rendered.contains("\"s\""),
            "secret value leaked: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("fp-r"), "non-secret fields still print");
    }
}
