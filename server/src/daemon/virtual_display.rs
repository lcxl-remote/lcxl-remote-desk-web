//! Daemon-side owner of the Windows virtual display.
//!
//! Holds the `SwDevice` handle returned by
//! [`desk_virtual_display::VirtualDisplayLifecycle::create`] for the
//! lifetime of the supervisor. The supervisor lives only in service-
//! daemon mode (`RouterContext::virtual_display = Some(...)`); other
//! startup paths leave the field as `None` and the router replies with
//! `FEATURE_UNAVAILABLE`.
//!
//! State machine:
//! - `Disabled` — toggle off or the latest `lifecycle.create()` failed.
//! - `Attaching` — handle created and `AttachVirtualDisplay` has either
//!   been queued at least once (after the first `Capabilities`) or is
//!   awaiting the next `Capabilities`. The supervisor stays in this
//!   state until the worker reports it has actually resolved the PnP
//!   instance id to a usable GDI display name. `is_active()` is `false`
//!   here so the router still rejects inbound `ChangeDisplaySettings`.
//! - `Attached` — both daemon (holding `SwDevice`) and worker (running
//!   capture against the virtual `\\.\DISPLAYn`) are in sync. Entered
//!   only via [`VirtualDisplaySupervisor::on_worker_attach_result`]
//!   with [`VirtualDisplayAttachOutcome::Attached`].
//! - `Detaching` — handle dropped, `DetachVirtualDisplay` sent; waiting
//!   for the worker's capture pipeline to swap back to the physical
//!   target.
//!
//! **Why send-success does not imply attached.** Earlier iterations of
//! this code promoted `Attaching → Attached` as soon as
//! `send_to_worker(AttachVirtualDisplay)` returned `Ok`. That was a
//! distributed-systems mistake: a successful IPC enqueue tells us only
//! that the message left the daemon's outgoing queue, not that the
//! worker has run `EnumDisplayDevicesW` in the user session and found
//! the virtual monitor. If GDI resolution fails inside the worker
//! (driver race, monitor never enumerated), the supervisor would
//! incorrectly report `is_active() == true` and the router would let a
//! `ChangeDisplaySettings(205)` through to a worker that has no
//! attached display. The fix is to gate the promotion on
//! `WorkerToService::VirtualDisplayAttachResult`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use desk_ipc_protocol::message::{
    AttachVirtualDisplayPayload, ExclusiveDirection, ExclusiveOutcome, ExclusiveResultPayload,
    ServiceToWorker, SetVirtualDisplayExclusivePayload, VirtualDisplayAttachOutcome,
    VirtualDisplayAttachResultPayload,
};
use desk_virtual_display::{VirtualDisplayError, VirtualDisplayHandle, VirtualDisplayLifecycle};
use tokio::sync::{Mutex, Notify, RwLock, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::daemon::worker_manager::WorkerManager;
use crate::error::DeskError;
use desk_utils::error::DeskErrorCode;

/// Lifecycle of the exclusive-mode layer that sits on top of the
/// `Attached` lifecycle state. Disjoint from `SupervisorState` —
/// exclusive can only meaningfully exist while the attach lifecycle
/// is in `Attached`, but the gate that decides whether to enter or
/// leave (control accepted? settings allow it?) lives entirely
/// outside the attach state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusiveState {
    Idle,
    Entering,
    Active,
    Leaving,
}

/// `state` + `current_op_id` co-mutate under a single write lock so
/// the codex round 6 #3 invariant ("op_id and state are always
/// observed together") is enforced by the type system. Reading
/// `current_op_id` without holding the lock is intentionally
/// unsupported — the field is private to this module.
#[derive(Debug)]
struct ExclusiveInner {
    state: ExclusiveState,
    /// Monotonically incremented at every `prepare_next_action`,
    /// `rollback_send_failure`, and `reset_exclusive_state` call.
    /// The worker echoes the op_id from
    /// [`SetVirtualDisplayExclusivePayload::op_id`] back via
    /// [`ExclusiveResultPayload::op_id`]; the daemon's
    /// `on_exclusive_result` drops anything whose `op_id` does not
    /// equal `current_op_id` (i.e. came from a superseded request).
    current_op_id: u64,
    /// Number of consecutive `LeftWithErrors` results received since
    /// the last successful `Left` / explicit reset (codex follow-up
    /// P1, 2026-05-26). When this reaches [`MAX_LEAVE_RETRIES`] the
    /// supervisor force-Idles and stops auto-retrying, leaving the
    /// worker's `ExclusiveGuard` Drop / OS logoff as the final
    /// recovery path. Reset on every successful `Left` and on
    /// `reset_exclusive_state`.
    leave_retry_count: u8,
    /// Earliest instant at which the reconciler is allowed to issue
    /// the next `(Active, desired=false) → Leaving` transition.
    /// `None` ⇒ no backoff in effect. Set after each `LeftWithErrors`
    /// based on `leave_retry_count` and the doubling schedule.
    /// `prepare_next_action` returns `ExclusiveAction::None` if
    /// `now < next_leave_at`, and `on_exclusive_result` spawns a
    /// delayed `reconcile_notify` so the driver loop wakes up at the
    /// right time.
    next_leave_at: Option<Instant>,
    /// Number of consecutive `EnterFailed` results received since the
    /// last successful `Entered` / explicit reset (e2e fix
    /// 2026-05-27, symmetric to `leave_retry_count`). Without this
    /// gate, `(Entering, EnterFailed) → Idle` is immediately
    /// re-triggered by `prepare_next_action` because `desired=true`
    /// is still set — producing the infinite "5 s prompt, fail,
    /// repeat" loop the user hit in e2e. When this reaches
    /// [`MAX_ENTER_RETRIES`] the supervisor force-clears
    /// `exclusive_desired` so the loop terminates; the user must
    /// re-acquire control (or toggle the setting) to retry.
    enter_retry_count: u8,
    /// Earliest instant at which the reconciler is allowed to issue
    /// the next `(Idle, desired=true) → Entering` transition.
    /// `None` ⇒ no backoff in effect. Same semantics as
    /// `next_leave_at` but for the enter path.
    next_enter_at: Option<Instant>,
}

/// Callback the router injects to let the supervisor recompute the
/// desired exclusive state at attach edges (where the supervisor is
/// the only party that knows the transition just happened).
///
/// Signature is `Fn(active: bool) -> (desired, prompt_ms)` — codex
/// round 7 #1 forced the `active` parameter out of the closure body
/// so the closure never has to reach back into the supervisor (which
/// would form a self-reference and a potential lock cycle). The
/// supervisor takes `active = self.is_active().await` itself before
/// calling the closure.
pub type DesiredComputerFn =
    Arc<dyn Fn(bool) -> Pin<Box<dyn Future<Output = (bool, u32)> + Send>> + Send + Sync>;

/// Action returned by [`VirtualDisplaySupervisor::prepare_next_action`].
/// `Send` carries the pre-built IPC plus the state pair needed for
/// guarded rollback on send failure.
#[derive(Debug)]
enum ExclusiveAction {
    None,
    Send {
        ipc: ServiceToWorker,
        next_state: ExclusiveState,
        op_id: u64,
        prev_state: ExclusiveState,
    },
}

/// Driver loop teardown timeout — how long the supervisor waits for
/// the worker to acknowledge a leave (or report enter failure) before
/// it gives up and forces the state back to Idle. 30 s is generous
/// for a CDS round-trip (~1 s per physical display); going higher
/// would let an unresponsive worker block daemon shutdown.
const EXCLUSIVE_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Initial driver-loop backoff between IPC send retries. Doubles up
/// to `MAX_BACKOFF` to keep a runaway loop from hammering the worker.
const MIN_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Cap on how many consecutive `LeftWithErrors` results the daemon
/// will auto-retry before forcing the state back to `Idle` and
/// logging an operator-visible error (codex follow-up P1,
/// 2026-05-26). Three retries with the schedule below gives the
/// worker ~14 s total (2 s + 4 s + 8 s); after that, the layout
/// retained on the worker side is left to `ExclusiveGuard::drop`
/// at session end + CDS transient logoff fallback. Picking a low
/// cap is intentional: `leave_exclusive` is mostly deterministic
/// (DEVMODE-driven), so repeated failures usually indicate a state
/// the daemon cannot resolve from here.
const MAX_LEAVE_RETRIES: u8 = 3;
/// Base delay for the exponential `LeftWithErrors` retry schedule:
/// retry N waits `LEAVE_RETRY_BASE_DELAY * 2^N` before firing, so
/// the three retries land at 2 s, 4 s, 8 s after their respective
/// failures. Total wall-clock window from the first failure to the
/// final give-up: ~14 s, comparable to one ICE failed-timeout.
const LEAVE_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);

/// E2E fix 2026-05-27: cap on consecutive `EnterFailed` results
/// before the supervisor stops auto-retrying. Symmetric to
/// [`MAX_LEAVE_RETRIES`]. When the budget is exhausted, the
/// supervisor clears `exclusive_desired` so the
/// `(Idle, desired=true) → Entering` row stops firing; the user
/// must re-acquire control or toggle the setting to retry.
const MAX_ENTER_RETRIES: u8 = 3;
/// Same doubling schedule as [`LEAVE_RETRY_BASE_DELAY`] — retry N
/// waits `ENTER_RETRY_BASE_DELAY * 2^N`. Keeps the two paths in
/// rough wall-clock parity (~14 s window before give-up).
const ENTER_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);

/// Internal lifecycle state. Only `Attached` makes the supervisor
/// `is_active()`; `Attaching` and `Detaching` are transition states
/// the router treats as inactive.
///
/// `instance_id` is the **PnP device instance id** assigned by
/// `SwDeviceCreate` (e.g. `SWD\LcxlVirtualDisplay\LcxlVirtualDisplay`).
/// It is the same value forwarded to the worker over IPC. The worker
/// (running inside the interactive user session) is responsible for
/// turning it into a GDI display name via
/// [`desk_virtual_display::resolve_display_name`] — the daemon cannot
/// do that itself because `EnumDisplayDevicesW` does not see the
/// virtual monitor from Session 0.
#[allow(
    clippy::large_enum_variant,
    reason = "The supervisor only ever holds one SupervisorState at a time, \
              so the per-instance overhead is bounded by max(variant size). \
              Boxing the Attaching/Attached variants would force an extra \
              allocation on every state transition without any concurrency \
              or copy-cost benefit."
)]
enum SupervisorState {
    Disabled,
    Attaching {
        instance_id: String,
        #[allow(dead_code)]
        handle: VirtualDisplayHandle,
    },
    Attached {
        instance_id: String,
        /// GDI display name (`\\.\DISPLAYn`) reported by the worker via
        /// `VirtualDisplayAttachOutcome::Attached`. Used by
        /// [`VirtualDisplaySupervisor::ensure_attached`] to verify the
        /// post-attach capabilities cache actually surfaces this
        /// monitor before letting `RequestRemote` proceed.
        display_name: String,
        // `handle` keeps the OS resource alive — dropped only on
        // `apply(false)` / `shutdown`. The struct is held for its
        // Drop, never read.
        #[allow(dead_code)]
        handle: VirtualDisplayHandle,
    },
    Detaching,
}

/// Outcome of [`VirtualDisplaySupervisor::ensure_attached`].
#[derive(Debug)]
pub enum EnsureAttachedOutcome {
    /// State is `Attached`, the post-promotion capabilities-version
    /// target has been reached, and the cached `MediaCapabilities`
    /// contains the attached display name. `RequestRemote` can safely
    /// assemble the Init reply expecting the IDD to appear in the
    /// dropdown.
    Attached,
    /// Bring-up did not complete within the timeout. The supervisor is
    /// left in whatever transitional state it reached (typically
    /// `Attaching`); a follow-up call will resume from there. Init
    /// reply this round should fall through without the IDD.
    TimedOut,
    /// Provider returned `NotSupported` or an OS error from
    /// `SwDeviceCreate`. Supervisor is `Disabled`.
    Unavailable(DeskError),
}

/// Service-daemon-only owner of the virtual display handle. The
/// supervisor is the **sole** caller of `provider.create()`; the
/// router asks it `is_active()` to decide whether to forward the
/// inbound `ChangeDisplaySettings` to the worker; `signaling_proxy`
/// pokes it via [`Self::on_worker_capabilities`] every time the
/// worker comes back with a `Capabilities` payload, so a freshly
/// re-spawned worker recovers `AttachVirtualDisplay` without
/// polling.
pub struct VirtualDisplaySupervisor {
    state: RwLock<SupervisorState>,
    provider: Box<dyn VirtualDisplayLifecycle>,
    worker_mgr: WorkerManager,
    /// Set when the `Attaching → Attached` promotion happens to the
    /// post-promotion `worker_mgr.capabilities_version()` target value
    /// (`snapshot + 1`). [`Self::ensure_attached`] uses this watch to
    /// wait until **both** the cap version has reached the target
    /// **and** the cached `MediaCapabilities` contains the attached
    /// `display_name` — the strict three-way confirmation that the
    /// daemon's view of the world has caught up with the worker's.
    ///
    /// Cleared (`send_replace(None)`) on detach so the next attach
    /// cycle starts from a clean slate.
    attached_capabilities_target: watch::Sender<Option<u64>>,
    /// Serialises the entire `apply(true)` / `apply(false)` /
    /// `shutdown()` flow including the IPC awaits that happen while
    /// the state lock is **not** held. Without this lock a concurrent
    /// `apply(true)` running between an in-flight `apply(false)`'s
    /// `*state = Detaching` and its trailing `DetachVirtualDisplay`
    /// IPC send could end up with the worker receiving Attach followed
    /// by the old Detach — tearing down the freshly attached monitor.
    /// `state` (the `RwLock`) still owns field-level synchronisation;
    /// `lifecycle_lock` provides whole-operation atomicity.
    lifecycle_lock: Mutex<()>,
    /// Most-recently observed IDD refresh rate (Hz) from the worker's
    /// `VirtualDisplayMode::Applied` echo. `0` ⇒ no observation yet.
    /// Used by:
    ///   * `InitSignalingData::virtual_display_current_refresh_hz`
    ///     (display only, not authoritative)
    ///   * The router's auto `ChangeDisplaySettings` path to fill in a
    ///     refresh value when the browser sends `refresh_hz=0`.
    /// Survives detach/re-attach intentionally — operator manual tuning
    /// during the previous attach cycle should not be lost just because
    /// the IDD bounced.
    last_known_refresh_hz: AtomicU32,
    /// Most-recently observed IDD width / height (pixels) from the
    /// worker's `VirtualDisplayMode::Applied` echo. `0` ⇒ no observation
    /// yet. Together with `last_known_refresh_hz` they form the
    /// idempotency key for the router's same-resolution short-circuit:
    /// a 205 whose `(width, height, refresh_hz)` exactly matches the
    /// cache skips the worker IPC entirely (no IDD Departure+Arrival
    /// driver round-trip, no WGC restart).
    ///
    /// **Unlike refresh, dimensions DO NOT survive attach generations.**
    /// On detach the driver release of the IDD lets Windows lose the
    /// negotiated mode; on the next attach the IDD comes up at whatever
    /// the driver advertises by default (usually `1920×1080@60`). A
    /// stale `2560×1440` cache would silently cause the router to fake
    /// a successful response to a request that really needs to set the
    /// mode. `reset_known_dimensions` is therefore called from every
    /// attach-lifecycle transition point — see the body comments at
    /// `apply` / `on_worker_attach_result`. Refresh stays put because
    /// it only feeds the `refresh_hz=0` fallback and is informational.
    last_known_width: AtomicU32,
    last_known_height: AtomicU32,
    /// Timestamp of the last auto request that consumed a throttle slot.
    /// `None` until the first call. Survives detach/re-attach (acts as a
    /// global rate limit regardless of supervisor cycles).
    last_auto_change_at: std::sync::Mutex<Option<Instant>>,

    // ───── Exclusive-mode layer ─────
    //
    // The exclusive state machine sits on top of the attach lifecycle:
    // the worker can only meaningfully enter exclusive while the
    // supervisor is in Attached, but the *decision* to enter is
    // driven by remote-control state, which arrives via signaling. A
    // dedicated driver loop owns the IPC sends + retries; the public
    // surface is `set_desired_exclusive` (write the desired flag) and
    // `on_exclusive_result` (apply the worker's reply).
    /// State + op_id co-mutated under one RwLock. Type-level
    /// enforcement of codex round 6 #3.
    exclusive_inner: Arc<RwLock<ExclusiveInner>>,
    /// Desired state set by the router (control change / settings
    /// change) and by `recompute_desired()` at attach edges.
    exclusive_desired: Arc<AtomicBool>,
    /// Prompt duration the worker should use the next time it
    /// receives `desired = true`. Stored separately so the router can
    /// update it without taking the exclusive_inner lock.
    exclusive_desired_prompt_ms: Arc<AtomicU32>,
    /// Watch channel populated on every state transition (including
    /// reset / rollback). `await_exclusive_idle` subscribes and
    /// returns once the state observed is `Idle`.
    exclusive_state_watch: watch::Sender<ExclusiveState>,
    /// Wake the driver loop when desired changes or a result lands.
    /// Also poked from `reset_exclusive_state` so a sleeping backoff
    /// re-evaluates immediately after an attach swap.
    reconcile_notify: Arc<Notify>,
    /// One-shot owned by `new_arc` so `shutdown_driver_loop` can stop
    /// the background task. `None` after shutdown.
    exclusive_shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// Router-injected callback (`Fn(active) -> (desired, prompt_ms)`).
    /// Installed via [`Self::set_desired_computer`] after the
    /// supervisor is constructed because the closure captures handles
    /// that themselves reference the supervisor's owning context.
    /// `None` until set — `recompute_desired()` is a no-op in that
    /// window, which is fine because nothing has driven the state
    /// machine into a non-Idle state yet either.
    desired_computer: Mutex<Option<DesiredComputerFn>>,
}

impl VirtualDisplaySupervisor {
    /// Construct the supervisor with a real `provider` (the platform
    /// factory returns the Windows IDD impl on Windows and a stub
    /// elsewhere) and a clone of the daemon's `WorkerManager` for IPC.
    /// Starts in `Disabled`.
    pub fn new(provider: Box<dyn VirtualDisplayLifecycle>, worker_mgr: WorkerManager) -> Self {
        let (target_tx, _target_rx) = watch::channel::<Option<u64>>(None);
        let (exclusive_watch_tx, _exclusive_watch_rx) =
            watch::channel::<ExclusiveState>(ExclusiveState::Idle);
        Self {
            state: RwLock::new(SupervisorState::Disabled),
            provider,
            worker_mgr,
            attached_capabilities_target: target_tx,
            lifecycle_lock: Mutex::new(()),
            last_known_refresh_hz: AtomicU32::new(0),
            last_known_width: AtomicU32::new(0),
            last_known_height: AtomicU32::new(0),
            last_auto_change_at: std::sync::Mutex::new(None),
            exclusive_inner: Arc::new(RwLock::new(ExclusiveInner {
                state: ExclusiveState::Idle,
                current_op_id: 0,
                leave_retry_count: 0,
                next_leave_at: None,
                enter_retry_count: 0,
                next_enter_at: None,
            })),
            exclusive_desired: Arc::new(AtomicBool::new(false)),
            exclusive_desired_prompt_ms: Arc::new(AtomicU32::new(0)),
            exclusive_state_watch: exclusive_watch_tx,
            reconcile_notify: Arc::new(Notify::new()),
            exclusive_shutdown_tx: Mutex::new(None),
            desired_computer: Mutex::new(None),
        }
    }

    /// Whether the supervisor currently holds a live monitor handle
    /// **and** the worker has confirmed it can drive that monitor.
    /// Used by the router to gate inbound `ChangeDisplaySettings`.
    pub async fn is_active(&self) -> bool {
        matches!(*self.state.read().await, SupervisorState::Attached { .. })
    }

    /// GDI device name (e.g. `\\.\DISPLAY8`) the IDD is currently pinned
    /// to when the supervisor is in `Attached`. `None` for every other
    /// state. Used by `pc_manager` to populate
    /// `InitSignalingData::virtual_display_device_name`, which the
    /// browser then uses both to label the matching entry in the display
    /// picker and to gate the adaptive-resolution hook.
    pub async fn attached_display_name(&self) -> Option<String> {
        match &*self.state.read().await {
            SupervisorState::Attached { display_name, .. } => Some(display_name.clone()),
            _ => None,
        }
    }

    /// Most-recently observed IDD refresh rate. `0` means the daemon has
    /// no `VirtualDisplayMode::Applied` observation yet (cold start) or
    /// the IDD has never reported a refresh.
    pub fn last_refresh_hz(&self) -> u32 {
        self.last_known_refresh_hz.load(Ordering::Relaxed)
    }

    /// Most-recently observed IDD mode `(width, height, refresh_hz)` as
    /// reported by the worker's `VirtualDisplayMode::Applied` echo.
    /// Returns `None` when **any** of the three is `0` — the router's
    /// idempotency check only short-circuits on fully-observed modes,
    /// because a half-zero cache would let a request with the same
    /// non-zero dimensions get faked-Applied against a never-observed
    /// refresh value.
    pub fn last_known_mode(&self) -> Option<(u32, u32, u32)> {
        let w = self.last_known_width.load(Ordering::Relaxed);
        let h = self.last_known_height.load(Ordering::Relaxed);
        let hz = self.last_known_refresh_hz.load(Ordering::Relaxed);
        if w == 0 || h == 0 || hz == 0 {
            None
        } else {
            Some((w, h, hz))
        }
    }

    /// Stash the full mode the driver actually applied, learned via the
    /// worker's `VirtualDisplayMode::Applied` echo path. Any zero
    /// component is treated as "no observation" — the whole update is
    /// skipped so a malformed echo never erases a valid prior value.
    pub fn record_applied_mode(&self, width: u32, height: u32, refresh_hz: u32) {
        if width == 0 || height == 0 || refresh_hz == 0 {
            return;
        }
        self.last_known_width.store(width, Ordering::Relaxed);
        self.last_known_height.store(height, Ordering::Relaxed);
        self.last_known_refresh_hz
            .store(refresh_hz, Ordering::Relaxed);
    }

    /// Clear the cached width/height (but **not** refresh). Called at
    /// every attach lifecycle transition — see the field-level doc on
    /// `last_known_width` for why dimensions cannot survive across
    /// attach generations while refresh can.
    fn reset_known_dimensions(&self) {
        self.last_known_width.store(0, Ordering::Relaxed);
        self.last_known_height.store(0, Ordering::Relaxed);
    }

    /// Throttle an auto `ChangeDisplaySettings` request: returns `true`
    /// if `min_interval` has elapsed since the last consumed slot (or
    /// this is the first ever call), and records `now` as the new slot.
    /// Returns `false` if the request must be rejected.
    ///
    /// `min_interval == 0` always permits (operator-configured "no
    /// defense"). The state is held in a sync `Mutex` because all calls
    /// are short critical sections and the surrounding router code is
    /// already async-aware; the lock is never held across `await`.
    pub fn try_consume_auto_slot(&self, now: Instant, min_interval: Duration) -> bool {
        let mut last = self
            .last_auto_change_at
            .lock()
            .expect("last_auto_change_at mutex poisoned");
        let allow = match *last {
            None => true,
            Some(prev) => now.saturating_duration_since(prev) >= min_interval,
        };
        if allow {
            *last = Some(now);
        }
        allow
    }

    /// Apply the desired enabled-state — `desired=true` ⇒ create the
    /// handle (if not already up) and move to `Attaching`. `false` ⇒
    /// drop the handle and notify the worker to swap capture back to
    /// the physical display.
    ///
    /// Failure to create the handle (e.g. stub provider returning
    /// `NotSupported`, real provider returning `SwDeviceCreate`
    /// errors) leaves the supervisor in `Disabled` and returns
    /// `DeskError::CustomError(FEATURE_UNAVAILABLE | WINDOWS_ERROR)`
    /// — the daemon startup path logs + continues so the rest of the
    /// service is still usable, just without virtual display
    /// support.
    pub async fn apply(&self, desired: bool) -> Result<(), DeskError> {
        // Hold the lifecycle lock for the whole operation so concurrent
        // apply / shutdown calls cannot interleave their IPC sends. See
        // the `lifecycle_lock` field doc.
        let _lifecycle = self.lifecycle_lock.lock().await;
        // Exclusive teardown must precede the SwDevice drop on
        // apply(false) — otherwise the worker would receive
        // SetVirtualDisplayExclusive(false) after the virtual display
        // has already disappeared from the OS (codex round 2 #1).
        // For apply(true), reset before the Attach IPC so the driver
        // loop sees a clean (Idle, desired=false) slate for the new
        // attach cycle (codex round 7 #5).
        if !desired {
            self.set_desired_exclusive(false, 0);
            if let Err(e) = self.await_exclusive_idle(EXCLUSIVE_TEARDOWN_TIMEOUT).await {
                warn!(
                    "[virtual-display] exclusive teardown timed out: {e}; \
                     dropping virtual display handle anyway, physical \
                     displays will recover via registry on next logon"
                );
            }
            self.reset_exclusive_state().await;
        } else {
            // codex round 7 #5: reset BEFORE the Attach IPC. The
            // driver loop must see a clean state machine for the new
            // attach cycle so a stale result from a previous cycle
            // cannot pollute the new op_id space.
            let needs_reset = matches!(
                &*self.state.read().await,
                SupervisorState::Disabled | SupervisorState::Detaching
            );
            if needs_reset {
                self.reset_exclusive_state().await;
            }
        }
        let mut state = self.state.write().await;
        match (&*state, desired) {
            // Already in the desired direction — no-op.
            (SupervisorState::Disabled | SupervisorState::Detaching, false) => Ok(()),
            (SupervisorState::Attaching { .. } | SupervisorState::Attached { .. }, true) => Ok(()),
            // Bring up.
            (SupervisorState::Disabled | SupervisorState::Detaching, true) => {
                match self.provider.create() {
                    Ok(handle) => {
                        let instance_id = handle.instance_id.clone();
                        info!(
                            virtual_display.instance_id = %instance_id,
                            "VirtualDisplaySupervisor created handle, moving to Attaching",
                        );
                        // Drop stale width/height from any previous attach
                        // generation BEFORE we transition to Attaching —
                        // the new IDD comes up at whatever the driver
                        // defaults to (typically 1920x1080@60), and a
                        // cached 2560x1440 would let the router fake an
                        // Applied response to the next 2560x1440 request
                        // even though the driver is still at the default
                        // mode. Refresh is preserved as an operator hint
                        // for the auto-fallback path.
                        self.reset_known_dimensions();
                        *state = SupervisorState::Attaching {
                            instance_id: instance_id.clone(),
                            handle,
                        };
                        drop(state); // Don't hold the write lock across the IPC await.
                        // Proactively kick the worker so attach does not
                        // depend on a future `WorkerToService::Capabilities`
                        // arriving. Under lazy bring-up the worker has
                        // typically already emitted its initial
                        // Capabilities; without this send the supervisor
                        // would sit in Attaching forever.
                        self.send_attach_to_worker(&instance_id).await;
                        Ok(())
                    }
                    Err(VirtualDisplayError::NotSupported) => {
                        warn!(
                            "VirtualDisplaySupervisor.create returned NotSupported \
                             (stub or unsupported platform); staying in Disabled",
                        );
                        *state = SupervisorState::Disabled;
                        DeskError::custom_error(
                            DeskErrorCode::FEATURE_UNAVAILABLE,
                            "virtual display provider returned NotSupported",
                        )
                    }
                    Err(e) => {
                        warn!("VirtualDisplaySupervisor.create failed: {e}");
                        *state = SupervisorState::Disabled;
                        DeskError::custom_error(
                            DeskErrorCode::SYSTEM_ERROR,
                            &format!("virtual display create failed: {e}"),
                        )
                    }
                }
            }
            // Tear down.
            (SupervisorState::Attaching { .. } | SupervisorState::Attached { .. }, false) => {
                // Drop the handle first (Drop closes SwDevice), then
                // tell the worker so it doesn't keep trying to capture
                // a monitor that is going away. `lifecycle_lock` is
                // held across all sends below, so any concurrent
                // `apply(true)` is forced to wait — guaranteeing the
                // worker observes Detach + RefreshCapabilities of this
                // round strictly before any next-round Attach.
                // Same dimension-cache invariant as the bring-up branch
                // (see comment there): tear-down ends an attach
                // generation, so the cached mode is no longer
                // authoritative even if we later re-attach with the
                // same IDD instance id.
                self.reset_known_dimensions();
                *state = SupervisorState::Detaching;
                drop(state); // Don't hold the write lock across the IPC await.
                if let Err(e) = self
                    .worker_mgr
                    .send_to_worker(ServiceToWorker::DetachVirtualDisplay)
                    .await
                {
                    warn!("failed to send DetachVirtualDisplay to worker: {e}");
                }
                // Mirror shutdown(): a detach must also flush the
                // capabilities cache so the next dialog drops the IDD
                // from the dropdown.
                if let Err(e) = self
                    .worker_mgr
                    .send_to_worker(ServiceToWorker::RefreshCapabilities)
                    .await
                {
                    warn!(
                        "[virtual-display] detach RefreshCapabilities send failed: {e}; \
                         dropdown may still list the IDD until the next worker restart",
                    );
                }
                let _ = self.attached_capabilities_target.send_replace(None);
                let mut state = self.state.write().await;
                *state = SupervisorState::Disabled;
                Ok(())
            }
        }
    }

    /// IPC helper shared by `apply(true)` (initial kick) and
    /// `on_worker_capabilities` (worker-restart recovery). Errors are
    /// logged but not propagated: a missing worker channel is a
    /// transient condition that the caller's outer retry loop will
    /// recover from.
    async fn send_attach_to_worker(&self, instance_id: &str) {
        let payload = AttachVirtualDisplayPayload {
            instance_id: instance_id.to_string(),
        };
        if let Err(e) = self
            .worker_mgr
            .send_to_worker(ServiceToWorker::AttachVirtualDisplay(payload))
            .await
        {
            warn!(
                "[virtual-display] AttachVirtualDisplay send to worker failed for \
                 {instance_id}: {e}; will retry on next opportunity",
            );
        }
    }

    /// Called by `signaling_proxy` every time the worker sends
    /// `WorkerToService::Capabilities`. If the supervisor is in
    /// `Attaching` (first-time bring-up) or `Attached` (worker
    /// re-spawned after a crash / desktop swap), re-emit
    /// `AttachVirtualDisplay` so the new worker rebuilds its capture
    /// pipeline against the virtual `\\.\DISPLAYn`.
    ///
    /// **State machine note:** this method does NOT promote
    /// `Attaching → Attached`. A successful IPC enqueue only tells
    /// us the message left our outbound queue; the worker might still
    /// fail to resolve the PnP instance id (Session 0 race, driver
    /// removal, etc.). The promotion happens in
    /// [`Self::on_worker_attach_result`] when the worker reports
    /// [`VirtualDisplayAttachOutcome::Attached`].
    pub async fn on_worker_capabilities(&self) {
        let instance_id = {
            let state = self.state.read().await;
            match &*state {
                SupervisorState::Attaching { instance_id, .. }
                | SupervisorState::Attached { instance_id, .. } => instance_id.clone(),
                _ => return,
            }
        };
        self.send_attach_to_worker(&instance_id).await;
        // Intentionally no state promotion here — see method doc.
    }

    /// Called by `signaling_proxy` when the worker reports the outcome
    /// of resolving the PnP instance id we forwarded via
    /// [`ServiceToWorker::AttachVirtualDisplay`]. This is the **only**
    /// place that promotes `Attaching → Attached`.
    ///
    /// Routing rules:
    /// - `payload.instance_id` must match the currently-tracked
    ///   `Attaching`/`Attached` instance id. A mismatch means a stale
    ///   reply (e.g. the daemon dropped and re-created the handle
    ///   between the worker's send and our receive); drop it.
    /// - `Attached(name)` while we are in `Attaching` → promote to
    ///   `Attached`. While we are already in `Attached` → idempotent
    ///   no-op (this happens when the worker is re-attaching after a
    ///   restart).
    /// - `Failed(msg)` → stay in `Attaching` and log. The next
    ///   `WorkerToService::Capabilities` triggers another
    ///   `on_worker_capabilities` send, giving the worker another
    ///   chance to resolve.
    pub async fn on_worker_attach_result(&self, payload: VirtualDisplayAttachResultPayload) {
        let mut state = self.state.write().await;
        // Capture the current tracked id before we move out of state.
        let current_id = match &*state {
            SupervisorState::Attaching { instance_id, .. }
            | SupervisorState::Attached { instance_id, .. } => Some(instance_id.clone()),
            _ => None,
        };
        let Some(current_id) = current_id else {
            debug!(
                virtual_display.instance_id = %payload.instance_id,
                "VirtualDisplayAttachResult arrived while supervisor not bringing up; dropping",
            );
            return;
        };
        if current_id != payload.instance_id {
            debug!(
                virtual_display.current_id = %current_id,
                virtual_display.received_id = %payload.instance_id,
                "VirtualDisplayAttachResult instance id mismatch; dropping stale reply",
            );
            return;
        }
        match payload.outcome {
            VirtualDisplayAttachOutcome::Attached(display_name) => {
                // Reset cached dimensions unconditionally on every
                // Attached outcome — covering BOTH the Attaching→Attached
                // promotion edge AND the already-Attached re-attach path
                // worker takes after a restart. The latter is the case
                // codex round 2 #2 caught: a worker restart re-sends
                // AttachVirtualDisplay and lands here while the
                // supervisor is still Attached, and the IDD has been
                // reborn at the driver's default mode rather than the
                // dimensions we cached pre-restart. Refresh is preserved
                // — see `last_known_width` field doc.
                self.reset_known_dimensions();
                let prev = std::mem::replace(&mut *state, SupervisorState::Disabled);
                // Edge-trigger: only the Attaching → Attached promotion
                // fires RefreshCapabilities. A second attach-result on
                // an already-Attached supervisor is an idempotent no-op
                // (worker restart path), so it must not re-publish.
                let promoted_now = matches!(prev, SupervisorState::Attaching { .. });
                // Snapshot the current capabilities version BEFORE we
                // emit RefreshCapabilities. The worker's response will
                // bump the version to at least `snapshot + 1`;
                // `ensure_attached` waits for the version to reach
                // that target (and for the cache to actually contain
                // `display_name`) before signalling completion.
                let cap_snapshot = self.worker_mgr.capabilities_version();
                *state = match prev {
                    SupervisorState::Attaching {
                        instance_id,
                        handle,
                    } => {
                        info!(
                            virtual_display.instance_id = %instance_id,
                            virtual_display.display_name = %display_name,
                            "VirtualDisplaySupervisor promoted Attaching -> Attached \
                             (via attach-result)",
                        );
                        SupervisorState::Attached {
                            instance_id,
                            display_name: display_name.clone(),
                            handle,
                        }
                    }
                    SupervisorState::Attached {
                        instance_id,
                        display_name: existing_name,
                        handle,
                    } => {
                        debug!(
                            virtual_display.instance_id = %instance_id,
                            virtual_display.display_name = %display_name,
                            "VirtualDisplayAttachResult Attached received while already \
                             Attached; idempotent no-op",
                        );
                        SupervisorState::Attached {
                            instance_id,
                            display_name: existing_name,
                            handle,
                        }
                    }
                    // We pulled current_id above only on Attaching/Attached, so
                    // we cannot be in Disabled/Detaching here.
                    other => other,
                };
                drop(state);
                if promoted_now {
                    // Publish the target BEFORE sending RefreshCapabilities.
                    // ensure_attached subscribes to this watch and must see
                    // a `Some(target)` value before the cap-version bump
                    // that satisfies it can complete; reversing the order
                    // would let an awaiter observe the cap bump without
                    // ever seeing the target and miss the completion edge.
                    let target = cap_snapshot + 1;
                    let _ = self.attached_capabilities_target.send_replace(Some(target));
                    // The IDD HMONITOR is now visible to
                    // `monitors::enum_display_infos`; ask the worker to
                    // re-publish Capabilities so the daemon's cache (and
                    // the next browser's `InitSignalingData`) reflects
                    // it. Re-emitting Capabilities will also trigger
                    // `on_worker_capabilities`, which re-sends a fresh
                    // AttachVirtualDisplay; the resulting second attach
                    // result lands on an already-Attached supervisor
                    // and is no-op, so the loop terminates after one
                    // extra attach.
                    if let Err(e) = self
                        .worker_mgr
                        .send_to_worker(ServiceToWorker::RefreshCapabilities)
                        .await
                    {
                        warn!(
                            "[virtual-display] failed to send RefreshCapabilities on attach \
                             promotion: {e}; daemon's capabilities cache may stay stale \
                             until the next worker restart",
                        );
                    }
                    // codex round 6 #1 + round 7 #1: attach just
                    // promoted to Attached, so `is_active()` is now
                    // true. The router-injected desired_computer may
                    // have returned `false` while we were still
                    // Attaching; recompute now so the driver loop can
                    // pick up the new desired state. State write
                    // lock has already been dropped above; the
                    // recompute_desired helper does its own brief
                    // `is_active().await` read.
                    self.recompute_desired().await;
                }
            }
            VirtualDisplayAttachOutcome::Failed(message) => {
                warn!(
                    virtual_display.instance_id = %payload.instance_id,
                    "Worker failed to attach virtual display: {message}. \
                     Staying in Attaching; next Capabilities will retry.",
                );
                // No state change. `Attaching` keeps `is_active() == false`,
                // so the router will still answer FEATURE_UNAVAILABLE.
            }
        }
    }

    /// Shutdown path — drop the handle if any. Best-effort
    /// `DetachVirtualDisplay` to the worker; failures are logged.
    /// After the detach is acknowledged we ask the worker to
    /// re-publish [`MediaCapabilities`] so the daemon's cache and any
    /// subsequent browser session no longer offers the IDD as a
    /// selectable display.
    pub async fn shutdown(&self) {
        // Serialise with apply() so a concurrent apply(true) cannot
        // wedge an Attach send between our Detach and the final state
        // transition to Disabled.
        let _lifecycle = self.lifecycle_lock.lock().await;
        // codex round 6 #2: shutdown must also tear down the
        // exclusive layer before the SwDevice handle is dropped.
        self.set_desired_exclusive(false, 0);
        if let Err(e) = self.await_exclusive_idle(EXCLUSIVE_TEARDOWN_TIMEOUT).await {
            warn!("[virtual-display] shutdown exclusive teardown timed out: {e}");
        }
        self.reset_exclusive_state().await;
        self.shutdown_driver_loop().await;
        let send_detach = {
            let state = self.state.read().await;
            matches!(
                *state,
                SupervisorState::Attaching { .. } | SupervisorState::Attached { .. }
            )
        };
        if send_detach {
            if let Err(e) = self
                .worker_mgr
                .send_to_worker(ServiceToWorker::DetachVirtualDisplay)
                .await
            {
                warn!("[virtual-display] shutdown DetachVirtualDisplay send failed: {e}");
            }
            if let Err(e) = self
                .worker_mgr
                .send_to_worker(ServiceToWorker::RefreshCapabilities)
                .await
            {
                warn!(
                    "[virtual-display] shutdown RefreshCapabilities send failed: {e}; \
                     dropdown may still list the IDD until the next worker restart",
                );
            }
        }
        let _ = self.attached_capabilities_target.send_replace(None);
        let mut state = self.state.write().await;
        *state = SupervisorState::Disabled;
    }

    /// Lazy bring-up entry point used by the router's `RequestRemote`
    /// branch. Returns when the supervisor is `Attached` **and** the
    /// daemon's `worker_capabilities` cache has been refreshed
    /// post-attach (cap version reached the promotion target **and**
    /// the cache lists the attached `display_name`) — so the caller
    /// can safely assemble an `InitSignalingData` whose
    /// `video_device_list` is known to include the IDD.
    ///
    /// On timeout the supervisor is left in its current state (typically
    /// `Attaching`); the next call resumes from there. The caller is
    /// expected to fall through to a capabilities-without-IDD Init reply
    /// rather than fail the `RequestRemote`. A background
    /// `RefreshCapabilities` may still complete after we return
    /// `TimedOut`, and the next dialog open will see the IDD.
    pub async fn ensure_attached(&self, timeout: Duration) -> EnsureAttachedOutcome {
        let deadline = Instant::now() + timeout;
        // Subscribe BEFORE the state inspection so a concurrent
        // promotion / cap bump that lands between the fast-path read
        // and the wait below cannot escape both signals.
        let mut target_rx = self.attached_capabilities_target.subscribe();
        let mut cap_rx = self.worker_mgr.subscribe_capabilities_version();

        if self.is_attach_cache_synced().await {
            return EnsureAttachedOutcome::Attached;
        }

        // Decide whether to kick. Holding only a read lock here keeps
        // the critical section short; the actual bring-up paths each
        // acquire the write lock + lifecycle lock independently.
        enum Kick {
            ResendAttach(String),
            BringUp,
            Wait,
        }
        let kick = {
            let state = self.state.read().await;
            match &*state {
                SupervisorState::Attaching { instance_id, .. } => {
                    Kick::ResendAttach(instance_id.clone())
                }
                SupervisorState::Attached { .. } => Kick::Wait,
                SupervisorState::Disabled | SupervisorState::Detaching => Kick::BringUp,
            }
        };
        match kick {
            Kick::ResendAttach(id) => {
                // Re-send the AttachVirtualDisplay IPC. The first send
                // (from apply(true)) may have raced a not-yet-installed
                // worker channel; retransmitting now is safe — the
                // worker's attach-result handling is idempotent.
                self.send_attach_to_worker(&id).await;
            }
            Kick::BringUp => {
                if let Err(e) = self.apply(true).await {
                    return EnsureAttachedOutcome::Unavailable(e);
                }
            }
            Kick::Wait => { /* already Attached; just wait for cache sync */ }
        }

        loop {
            if self.is_attach_cache_synced().await {
                // codex round 6 #1: defensive recompute on the
                // ensure_attached return path so a race where on_worker_
                // attach_result's recompute fires before is_active() is
                // observably true still surfaces the new desired state.
                self.recompute_desired().await;
                return EnsureAttachedOutcome::Attached;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return EnsureAttachedOutcome::TimedOut;
            }
            tokio::select! {
                res = tokio::time::timeout(remaining, target_rx.changed()) => {
                    if res.is_err() {
                        return EnsureAttachedOutcome::TimedOut;
                    }
                }
                res = tokio::time::timeout(remaining, cap_rx.changed()) => {
                    if res.is_err() {
                        return EnsureAttachedOutcome::TimedOut;
                    }
                }
            }
        }
    }

    /// Three-way completion check used by `ensure_attached`:
    /// 1. The `Attaching → Attached` promotion has happened (target is
    ///    `Some(_)`).
    /// 2. The daemon's `capabilities_version` has reached the target
    ///    set at promotion time (i.e. the worker has emitted at least
    ///    one `Capabilities` after we sent RefreshCapabilities).
    /// 3. The cached capabilities actually list the attached
    ///    `display_name` — guarding against unrelated capability bumps
    ///    that happened to satisfy `cap_version >= target` without
    ///    surfacing the IDD.
    async fn is_attach_cache_synced(&self) -> bool {
        let target = *self.attached_capabilities_target.borrow();
        let Some(t) = target else {
            return false;
        };
        if self.worker_mgr.capabilities_version() < t {
            return false;
        }
        let display_name = match &*self.state.read().await {
            SupervisorState::Attached { display_name, .. } => Some(display_name.clone()),
            _ => None,
        };
        let Some(name) = display_name else {
            return false;
        };
        self.worker_mgr.capabilities_contains_display(&name)
    }
}

/// Helper used by callers that want the supervisor wrapped in
/// `Arc<...>` so it can be cloned into `RouterContext.virtual_display`
/// and the `signaling_proxy` Capabilities hook simultaneously.
///
/// Spawns the exclusive-mode driver loop and stores the shutdown
/// one-shot back on the supervisor. The driver loop is alive even
/// before any `desired_computer` is installed; it simply produces
/// `ExclusiveAction::None` until exclusive becomes desired.
pub fn new_arc(
    provider: Box<dyn VirtualDisplayLifecycle>,
    worker_mgr: WorkerManager,
) -> Arc<VirtualDisplaySupervisor> {
    let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr));
    spawn_driver_loop(supervisor.clone());
    supervisor
}

fn spawn_driver_loop(supervisor: Arc<VirtualDisplaySupervisor>) {
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    {
        let mut guard = supervisor
            .exclusive_shutdown_tx
            .try_lock()
            .expect("spawn_driver_loop runs once on a fresh supervisor; lock cannot be contended");
        *guard = Some(shutdown_tx);
    }
    let supervisor_for_loop = Arc::downgrade(&supervisor);
    tokio::spawn(async move {
        // The loop holds a Weak so a dropped supervisor (e.g. tests
        // that forget to call shutdown) lets the task exit on the
        // next iteration.
        let mut shutdown_rx = shutdown_rx;
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => return,
                _ = async {
                    if let Some(s) = supervisor_for_loop.upgrade() {
                        s.reconcile_notify.notified().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
            }
            let Some(supervisor) = supervisor_for_loop.upgrade() else {
                return;
            };
            supervisor.reconcile_once_with_retry(&mut shutdown_rx).await;
        }
    });
}

impl VirtualDisplaySupervisor {
    /// Install the router-injected desired-state callback. Called once
    /// at daemon startup after the supervisor and the router context
    /// are both wired up. Idempotent re-installations are tolerated
    /// (the latest closure wins).
    pub async fn set_desired_computer(&self, computer: DesiredComputerFn) {
        let mut guard = self.desired_computer.lock().await;
        *guard = Some(computer);
    }

    /// Router-facing: change the desired exclusive state. Does not
    /// emit an IPC by itself — the driver loop reads the flag at the
    /// next reconcile and produces the right action.
    pub fn set_desired_exclusive(&self, desired: bool, prompt_ms: u32) {
        self.exclusive_desired.store(desired, Ordering::SeqCst);
        self.exclusive_desired_prompt_ms
            .store(prompt_ms, Ordering::SeqCst);
        self.reconcile_notify.notify_one();
    }

    /// codex round 6 #1 + round 7 #1: re-derive the desired flag at
    /// attach edges. The supervisor takes the `is_active()` snapshot
    /// itself (in this method, *not* inside the closure) so the
    /// closure body never reaches back into the supervisor's locks.
    /// Callers must `drop(state_guard)` before awaiting this — the
    /// helper itself only takes `is_active()`'s read lock briefly.
    pub async fn recompute_desired(&self) {
        let computer = {
            let guard = self.desired_computer.lock().await;
            guard.clone()
        };
        let Some(computer) = computer else {
            return; // no router wired up yet (e.g. tests / in-process mode)
        };
        let active = self.is_active().await;
        let (desired, prompt_ms) = computer(active).await;
        self.set_desired_exclusive(desired, prompt_ms);
    }

    /// Subscribe to a watch reader of the exclusive state. Used by
    /// `await_exclusive_idle` and by tests that need to observe the
    /// transition sequence.
    pub fn subscribe_exclusive_state(&self) -> watch::Receiver<ExclusiveState> {
        self.exclusive_state_watch.subscribe()
    }

    /// Wait until the exclusive state becomes `Idle` or the timeout
    /// fires. `Ok(())` on idle; `Err(_)` on timeout.
    pub async fn await_exclusive_idle(&self, timeout: Duration) -> Result<(), DeskError> {
        let mut rx = self.exclusive_state_watch.subscribe();
        if *rx.borrow() == ExclusiveState::Idle {
            return Ok(());
        }
        match tokio::time::timeout(timeout, async {
            while rx.changed().await.is_ok() {
                if *rx.borrow() == ExclusiveState::Idle {
                    return Ok::<(), ()>(());
                }
            }
            Err(())
        })
        .await
        {
            Ok(Ok(())) => Ok(()),
            _ => {
                DeskError::custom_error(DeskErrorCode::SYSTEM_ERROR, "exclusive teardown timed out")
            }
        }
    }

    /// Forcibly drop the exclusive layer's state to `(Idle, desired=false)`
    /// and bump `op_id`. Called from `apply(true)` before sending the
    /// next Attach (so a stale result from a previous attach cycle
    /// can never poison the fresh state machine) and from
    /// `apply(false)` / `shutdown` after the await idle settles
    /// (covers both success and timeout paths).
    pub async fn reset_exclusive_state(&self) {
        let mut inner = self.exclusive_inner.write().await;
        inner.state = ExclusiveState::Idle;
        inner.current_op_id = inner.current_op_id.wrapping_add(1);
        // Codex follow-up P1: reset the leave-retry bookkeeping too —
        // a fresh apply(true) / apply(false) / shutdown must not
        // inherit a stale retry-budget counter from the previous
        // generation. E2E fix 2026-05-27: same applies to the enter
        // retry bookkeeping.
        inner.leave_retry_count = 0;
        inner.next_leave_at = None;
        inner.enter_retry_count = 0;
        inner.next_enter_at = None;
        self.exclusive_desired.store(false, Ordering::SeqCst);
        let _ = self
            .exclusive_state_watch
            .send_replace(ExclusiveState::Idle);
    }

    /// Apply a worker result. Drops stale results whose `op_id` does
    /// not match `current_op_id`. State transition runs in the same
    /// write lock that loaded `current_op_id` so codex round 5 #1's
    /// "load + take write" race is closed by construction.
    pub async fn on_exclusive_result(&self, payload: ExclusiveResultPayload) {
        let mut inner = self.exclusive_inner.write().await;
        if payload.op_id != inner.current_op_id {
            debug!(
                "drop stale ExclusiveResult: op_id={} current={}",
                payload.op_id, inner.current_op_id
            );
            return;
        }
        let mut new_state = apply_result_transition(inner.state, &payload);
        // Codex follow-up P1 (2026-05-26): bounded backoff retry on
        // `LeftWithErrors`. The pure `apply_result_transition` puts
        // us into `Active` so the reconciler can drive another leave;
        // the retry budget + delayed `reconcile_notify` live here
        // because they depend on `leave_retry_count` (the inner field
        // the pure transition function does not see).
        let mut delayed_notify: Option<Duration> = None;
        match &payload.outcome {
            ExclusiveOutcome::LeftWithErrors(msg) => {
                let retries_so_far = inner.leave_retry_count;
                if retries_so_far + 1 >= MAX_LEAVE_RETRIES {
                    error!(
                        "[virtual-display] worker LeftWithErrors after {} attempts \
                         (op_id={}): {}; giving up auto-retry, forcing state to Idle; \
                         layout retained worker-side for ExclusiveGuard::drop / \
                         logoff CDS fallback",
                        retries_so_far + 1,
                        payload.op_id,
                        msg,
                    );
                    new_state = ExclusiveState::Idle;
                    inner.leave_retry_count = 0;
                    inner.next_leave_at = None;
                } else {
                    let next_count = retries_so_far + 1;
                    let delay = LEAVE_RETRY_BASE_DELAY * (1u32 << next_count);
                    inner.leave_retry_count = next_count;
                    inner.next_leave_at = Some(Instant::now() + delay);
                    delayed_notify = Some(delay);
                    warn!(
                        "[virtual-display] worker LeftWithErrors (op_id={}, attempt {}/{}): \
                         {}; will retry leave in {:?}",
                        payload.op_id, next_count, MAX_LEAVE_RETRIES, msg, delay,
                    );
                }
            }
            ExclusiveOutcome::Left => {
                // Successful leave: clear any retry bookkeeping.
                if inner.leave_retry_count > 0 || inner.next_leave_at.is_some() {
                    info!(
                        "[virtual-display] leave succeeded after {} retry attempt(s); \
                         resetting backoff state",
                        inner.leave_retry_count,
                    );
                }
                inner.leave_retry_count = 0;
                inner.next_leave_at = None;
            }
            // E2E fix 2026-05-27: enter side gets its own bounded
            // backoff symmetric to the leave path. Without this gate
            // `(Entering, EnterFailed) → Idle` is immediately
            // re-triggered by `prepare_next_action` because
            // `desired=true` is still set, producing an infinite
            // "5 s prompt, fail, repeat" loop.
            ExclusiveOutcome::EnterFailed(msg) => {
                let retries_so_far = inner.enter_retry_count;
                if retries_so_far + 1 >= MAX_ENTER_RETRIES {
                    error!(
                        "[virtual-display] worker EnterFailed after {} attempts \
                         (op_id={}): {}; giving up auto-retry, clearing \
                         exclusive_desired so the loop stops; user must \
                         re-acquire control or toggle the setting to retry",
                        retries_so_far + 1,
                        payload.op_id,
                        msg,
                    );
                    inner.enter_retry_count = 0;
                    inner.next_enter_at = None;
                    // Drop the user wish so `(Idle, true) → Entering`
                    // stops firing. AtomicBool is safe to mutate while
                    // holding the inner write lock — no lock cycle.
                    self.exclusive_desired.store(false, Ordering::SeqCst);
                } else {
                    let next_count = retries_so_far + 1;
                    let delay = ENTER_RETRY_BASE_DELAY * (1u32 << next_count);
                    inner.enter_retry_count = next_count;
                    inner.next_enter_at = Some(Instant::now() + delay);
                    delayed_notify = Some(delay);
                    warn!(
                        "[virtual-display] worker EnterFailed (op_id={}, attempt {}/{}): \
                         {}; will retry enter in {:?}",
                        payload.op_id, next_count, MAX_ENTER_RETRIES, msg, delay,
                    );
                }
            }
            ExclusiveOutcome::Entered => {
                // Successful enter: clear any retry bookkeeping.
                if inner.enter_retry_count > 0 || inner.next_enter_at.is_some() {
                    info!(
                        "[virtual-display] enter succeeded after {} retry attempt(s); \
                         resetting backoff state",
                        inner.enter_retry_count,
                    );
                }
                inner.enter_retry_count = 0;
                inner.next_enter_at = None;
            }
        }
        inner.state = new_state;
        let _ = self.exclusive_state_watch.send_replace(new_state);
        drop(inner);
        // Spawn the delayed retry notification before the regular
        // notify_one so the driver loop doesn't immediately see a
        // ready `notified()` and skip past the backoff gate.
        if let Some(delay) = delayed_notify {
            let notify = self.reconcile_notify.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                notify.notify_one();
            });
        }
        self.reconcile_notify.notify_one();
    }

    /// Compute the next IPC to send given the current `(state, desired)`
    /// pair. Mutates `exclusive_inner` in the same write lock: state
    /// advances to the transitional value (`Entering` / `Leaving`)
    /// and `current_op_id` bumps once. If the round-trip fails
    /// (worker IPC error), `rollback_send_failure` reverses both.
    async fn prepare_next_action(&self) -> ExclusiveAction {
        let mut inner = self.exclusive_inner.write().await;
        let current = inner.state;
        let desired = self.exclusive_desired.load(Ordering::SeqCst);
        let prompt_ms = self.exclusive_desired_prompt_ms.load(Ordering::SeqCst);
        let (kind, next_state) = match (current, desired) {
            (ExclusiveState::Idle, true) => (true, ExclusiveState::Entering),
            (ExclusiveState::Entering, false) => (false, ExclusiveState::Leaving),
            (ExclusiveState::Active, false) => (false, ExclusiveState::Leaving),
            _ => return ExclusiveAction::None,
        };
        // Codex follow-up P1 (2026-05-26): gate the
        // (Active, desired=false) retry path on `next_leave_at`. Any
        // earlier wake-up just re-arms the timer and returns None;
        // `on_exclusive_result` already scheduled the matching delayed
        // notify so the loop will revisit at the right time. Other
        // transitions ignore the gate — only the retry path uses it.
        if matches!(current, ExclusiveState::Active) && !desired {
            if let Some(retry_at) = inner.next_leave_at {
                let now = Instant::now();
                if now < retry_at {
                    return ExclusiveAction::None;
                }
                // Backoff elapsed — clear the marker so the retry
                // fires this round (count stays until a successful
                // Left clears it via on_exclusive_result).
                inner.next_leave_at = None;
            }
        }
        // E2E fix 2026-05-27: same gate for the symmetric
        // `(Idle, desired=true) → Entering` retry path. Without it,
        // `apply_result_transition` puts EnterFailed back into Idle
        // and the reconciler immediately resends — producing the
        // infinite re-prompt loop observed in e2e.
        if matches!(current, ExclusiveState::Idle) && desired {
            if let Some(retry_at) = inner.next_enter_at {
                let now = Instant::now();
                if now < retry_at {
                    return ExclusiveAction::None;
                }
                inner.next_enter_at = None;
            }
        }
        inner.current_op_id = inner.current_op_id.wrapping_add(1);
        let op_id = inner.current_op_id;
        let prev_state = inner.state;
        inner.state = next_state;
        let _ = self.exclusive_state_watch.send_replace(next_state);
        ExclusiveAction::Send {
            ipc: ServiceToWorker::SetVirtualDisplayExclusive(SetVirtualDisplayExclusivePayload {
                op_id,
                desired: kind,
                prompt_duration_ms: prompt_ms,
            }),
            next_state,
            op_id,
            prev_state,
        }
    }

    /// Guarded rollback. Only reverses if `current_op_id` and
    /// observed `state` still match the values that `prepare_next_action`
    /// recorded — codex round 5 #2 prevents a concurrent reset from
    /// being clobbered.
    async fn rollback_send_failure(
        &self,
        failed_op_id: u64,
        expected_state: ExclusiveState,
        prev_state: ExclusiveState,
    ) {
        let mut inner = self.exclusive_inner.write().await;
        if inner.current_op_id != failed_op_id {
            return;
        }
        if inner.state != expected_state {
            return;
        }
        inner.state = prev_state;
        inner.current_op_id = inner.current_op_id.wrapping_add(1);
        let _ = self.exclusive_state_watch.send_replace(prev_state);
    }

    /// Stop the driver loop. Idempotent — sending on a dropped
    /// receiver is a no-op.
    pub async fn shutdown_driver_loop(&self) {
        let tx = {
            let mut guard = self.exclusive_shutdown_tx.lock().await;
            guard.take()
        };
        if let Some(tx) = tx {
            let _ = tx.send(());
        }
    }

    /// One iteration of the driver loop. Repeats `prepare_next_action`
    /// + send + backoff until either there is no work to do or the
    /// shutdown signal fires.
    async fn reconcile_once_with_retry(&self, shutdown_rx: &mut oneshot::Receiver<()>) {
        let mut backoff = MIN_BACKOFF;
        loop {
            let action = self.prepare_next_action().await;
            let ExclusiveAction::Send {
                ipc,
                next_state,
                op_id,
                prev_state,
            } = action
            else {
                return;
            };
            match self.worker_mgr.send_to_worker(ipc).await {
                Ok(()) => return,
                Err(e) => {
                    warn!("[virtual-display] reconcile IPC send failed: {e}; retry in {backoff:?}");
                    self.rollback_send_failure(op_id, next_state, prev_state)
                        .await;
                    tokio::select! {
                        _ = &mut *shutdown_rx => return,
                        _ = self.reconcile_notify.notified() => {
                            backoff = MIN_BACKOFF;
                        }
                        _ = tokio::time::sleep(backoff) => {
                            backoff = (backoff * 2).min(MAX_BACKOFF);
                        }
                    }
                }
            }
        }
    }
}

/// State transition table driven by an `ExclusiveResult`. Implements
/// the codex round 6 #5 acceptance (4 outcomes only — EnterCancelled
/// deleted). The `Leaving + Entered` row is defensive (a stale
/// `Entered` should never reach here thanks to the op_id gate, but
/// keeping the row guards against a wire regression).
///
/// Codex follow-up P1 (2026-05-26): `(Leaving, LeftWithErrors)` now
/// transitions to `Active` instead of `Idle` so the worker-side
/// retained layout has a daemon-side counterpart that can drive a
/// bounded backoff retry. The actual "give up after N retries →
/// force Idle" decision lives in `on_exclusive_result` (it depends
/// on `leave_retry_count`, not visible to this pure function).
fn apply_result_transition(
    state: ExclusiveState,
    payload: &ExclusiveResultPayload,
) -> ExclusiveState {
    match (state, &payload.outcome, payload.direction) {
        (ExclusiveState::Entering, ExclusiveOutcome::Entered, ExclusiveDirection::Entering) => {
            ExclusiveState::Active
        }
        (
            ExclusiveState::Entering,
            ExclusiveOutcome::EnterFailed(_),
            ExclusiveDirection::Entering,
        ) => ExclusiveState::Idle,
        (ExclusiveState::Leaving, ExclusiveOutcome::Left, ExclusiveDirection::Leaving) => {
            ExclusiveState::Idle
        }
        (
            ExclusiveState::Leaving,
            ExclusiveOutcome::LeftWithErrors(_),
            ExclusiveDirection::Leaving,
        ) => ExclusiveState::Active,
        // Defensive: state already absorbed the transition, or stale
        // ack made it past op_id gating (shouldn't happen). Stay put.
        _ => state,
    }
}

#[cfg(test)]
impl VirtualDisplaySupervisor {
    /// Test-only helper: produce a supervisor stuck in `Disabled`,
    /// so `is_active()` returns `false` and the router's
    /// FEATURE_UNAVAILABLE / "unavailable" arm fires. The provider
    /// is a `NotSupported` stub — `apply(true)` would surface a
    /// `FEATURE_UNAVAILABLE` error but leaves state in `Disabled`.
    pub fn new_disabled_for_test(worker_mgr: WorkerManager) -> Self {
        struct NotSupportedProvider;
        impl VirtualDisplayLifecycle for NotSupportedProvider {
            fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
                Err(VirtualDisplayError::NotSupported)
            }
        }
        Self::new(Box::new(NotSupportedProvider), worker_mgr)
    }

    /// Test-only helper: seed only the refresh portion of the cache,
    /// leaving width/height at zero. Models the real-world state right
    /// after `apply(true)` (dimension reset preserved refresh from the
    /// previous attach generation as an operator hint) and lets tests
    /// exercise the router's `refresh_hz=0` fallback path without
    /// triggering the idempotent short-circuit (which requires a fully
    /// observed mode).
    pub fn seed_refresh_hz_for_test(&self, hz: u32) {
        self.last_known_refresh_hz.store(hz, Ordering::Relaxed);
    }

    /// Test-only state inspector. `is_active()` only distinguishes
    /// `Attached` from everything else; tests for the
    /// `Attaching → Attached` promotion need finer granularity to tell
    /// the `Disabled` and `Attaching` arms apart.
    pub async fn state_label(&self) -> &'static str {
        match *self.state.read().await {
            SupervisorState::Disabled => "Disabled",
            SupervisorState::Attaching { .. } => "Attaching",
            SupervisorState::Attached { .. } => "Attached",
            SupervisorState::Detaching => "Detaching",
        }
    }

    /// Test-only helper: produce a supervisor pre-promoted to
    /// `Attached`, so `is_active()` returns `true` and the router
    /// proceeds past the FEATURE_UNAVAILABLE gates into validation /
    /// dispatch. Useful for testing the INVALID_PARAMS /
    /// REMOTE_DESK_OFFLINE / success-dispatch routes.
    ///
    /// Installs a one-bucket `MediaCapabilities` containing
    /// `display_name` so [`Self::ensure_attached`] fast-path passes
    /// without any external setup, and primes
    /// `attached_capabilities_target` to the current cap version. Both
    /// pieces are required for `ensure_attached` to recognise the
    /// supervisor as fully attached.
    pub fn new_attached_for_test(worker_mgr: WorkerManager, instance_id: &str) -> Self {
        use desk_ipc_protocol::message::MediaCapabilities;
        use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};
        use desk_virtual_display::VirtualDisplayHandleInner;
        use std::collections::BTreeMap;
        struct MockHandleInner;
        impl VirtualDisplayHandleInner for MockHandleInner {}
        struct UnreachableProvider;
        impl VirtualDisplayLifecycle for UnreachableProvider {
            fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
                panic!("provider must not be invoked on a pre-attached test supervisor")
            }
        }
        let display_name = r"\\.\TESTDISPLAY".to_string();
        let handle = VirtualDisplayHandle::new(instance_id.to_string(), Box::new(MockHandleInner));
        // Install capabilities that include the display so
        // ensure_attached's `capabilities_contains_display` check
        // passes. This also bumps capabilities_version to >= 1.
        let mut video_device_list: BTreeMap<String, Vec<DisplayInfo>> = BTreeMap::new();
        video_device_list.insert(
            "test".to_string(),
            vec![DisplayInfo {
                device_name: display_name.clone(),
                display_device_name: None,
                desktop_coordinates: DisplayRect {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },
                resolutions: vec![],
                attached_to_desktop: true,
                rotation: 0,
            }],
        );
        worker_mgr.set_worker_capabilities(MediaCapabilities {
            video_codecs: vec![],
            audio_codecs: vec![],
            video_encoders: vec![],
            audio_encoders: vec![],
            video_device_list,
            audio_device_list: BTreeMap::new(),
            has_tauri: false,
            is_admin: false,
            desktop_name: "Default".to_string(),
        });
        let cap_version = worker_mgr.capabilities_version();
        let (target_tx, _target_rx) = watch::channel(Some(cap_version));
        let (exclusive_watch_tx, _exclusive_watch_rx) =
            watch::channel::<ExclusiveState>(ExclusiveState::Idle);
        Self {
            state: RwLock::new(SupervisorState::Attached {
                instance_id: instance_id.to_string(),
                display_name,
                handle,
            }),
            provider: Box::new(UnreachableProvider),
            worker_mgr,
            attached_capabilities_target: target_tx,
            lifecycle_lock: Mutex::new(()),
            last_known_refresh_hz: AtomicU32::new(0),
            last_known_width: AtomicU32::new(0),
            last_known_height: AtomicU32::new(0),
            last_auto_change_at: std::sync::Mutex::new(None),
            exclusive_inner: Arc::new(RwLock::new(ExclusiveInner {
                state: ExclusiveState::Idle,
                current_op_id: 0,
                leave_retry_count: 0,
                next_leave_at: None,
                enter_retry_count: 0,
                next_enter_at: None,
            })),
            exclusive_desired: Arc::new(AtomicBool::new(false)),
            exclusive_desired_prompt_ms: Arc::new(AtomicU32::new(0)),
            exclusive_state_watch: exclusive_watch_tx,
            reconcile_notify: Arc::new(Notify::new()),
            exclusive_shutdown_tx: Mutex::new(None),
            desired_computer: Mutex::new(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::pc_manager::PcRegistry;
    use crate::model::settings::{Settings, SharedSettings};
    use actix_web::web;
    use desk_ipc_protocol::message::WorkerToService;
    use desk_virtual_display::VirtualDisplayHandleInner;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockHandleInner;
    impl VirtualDisplayHandleInner for MockHandleInner {}

    struct MockLifecycle {
        create_calls: AtomicU32,
        result: fn() -> Result<VirtualDisplayHandle, VirtualDisplayError>,
    }

    /// The PnP instance id our `MockLifecycle::returns_handle()` mock
    /// claims to have just created. Mirrors the
    /// `SWD\<HW id>\<instance id>` shape produced by the real
    /// `SwDeviceCreate` so the tests double as documentation of the
    /// post-fix payload contract.
    const MOCK_INSTANCE_ID: &str = "SWD\\MOCK\\MOCK";

    impl MockLifecycle {
        fn returns_handle() -> Self {
            Self {
                create_calls: AtomicU32::new(0),
                result: || {
                    Ok(VirtualDisplayHandle::new(
                        MOCK_INSTANCE_ID.to_string(),
                        Box::new(MockHandleInner),
                    ))
                },
            }
        }
        fn returns_not_supported() -> Self {
            Self {
                create_calls: AtomicU32::new(0),
                result: || Err(VirtualDisplayError::NotSupported),
            }
        }
    }

    impl VirtualDisplayLifecycle for MockLifecycle {
        fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
            self.create_calls.fetch_add(1, Ordering::SeqCst);
            (self.result)()
        }
    }

    fn make_worker_mgr() -> (
        WorkerManager,
        tokio::sync::mpsc::UnboundedReceiver<WorkerToService>,
    ) {
        let shared = SharedSettings::from(Settings::default());
        let settings = web::Data::new(shared);
        let pc_registry = PcRegistry::new();
        WorkerManager::new(settings, pc_registry)
    }

    #[tokio::test]
    async fn supervisor_apply_false_then_true_creates_handle() {
        let lifecycle = Arc::new(MockLifecycle::returns_handle());
        let lifecycle_for_provider = Arc::clone(&lifecycle);
        struct ArcProvider(Arc<MockLifecycle>);
        impl VirtualDisplayLifecycle for ArcProvider {
            fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
                self.0.create()
            }
        }
        let provider: Box<dyn VirtualDisplayLifecycle> =
            Box::new(ArcProvider(lifecycle_for_provider));
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

        // apply(false) on initially-Disabled is a no-op.
        supervisor
            .apply(false)
            .await
            .expect("apply(false) on Disabled");
        assert_eq!(lifecycle.create_calls.load(Ordering::SeqCst), 0);
        assert!(!supervisor.is_active().await);

        // apply(true) creates the handle, moves to Attaching.
        supervisor.apply(true).await.expect("apply(true)");
        assert_eq!(lifecycle.create_calls.load(Ordering::SeqCst), 1);
        // Attaching is NOT active — must wait for Capabilities first.
        assert!(!supervisor.is_active().await);

        // Second apply(true) is idempotent — does not re-create.
        supervisor
            .apply(true)
            .await
            .expect("apply(true) idempotent");
        assert_eq!(lifecycle.create_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn supervisor_apply_true_returns_not_supported_when_stub() {
        let provider: Box<dyn VirtualDisplayLifecycle> =
            Box::new(MockLifecycle::returns_not_supported());
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        let err = supervisor
            .apply(true)
            .await
            .expect_err("apply(true) on NotSupported provider must surface error");
        match err {
            DeskError::CustomError(custom) => {
                assert_eq!(
                    custom.error_code.code(),
                    DeskErrorCode::FEATURE_UNAVAILABLE.code()
                );
            }
            other => panic!("expected CustomError(FEATURE_UNAVAILABLE), got {other:?}"),
        }
        assert!(!supervisor.is_active().await);
    }

    #[tokio::test]
    async fn supervisor_apply_true_then_false_drops_handle_and_emits_detach() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _worker_rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

        supervisor.apply(true).await.expect("apply(true)");
        // on_worker_capabilities no longer promotes — even if the
        // send succeeded, the supervisor stays in Attaching until an
        // explicit attach-result lands. So whatever happens to the
        // IPC send below, state remains Attaching.
        supervisor.on_worker_capabilities().await;
        assert!(
            !supervisor.is_active().await,
            "Capabilities alone must NOT promote Attaching -> Attached"
        );

        supervisor.apply(false).await.expect("apply(false)");
        // Detach drops the handle (Drop closes the OS resource).
        // We can't observe the drop directly here, but we can
        // confirm the supervisor is back to Disabled and another
        // apply(true) creates a fresh handle.
        assert!(!supervisor.is_active().await);
    }

    /// `apply(true)` must persist the PnP instance id surfaced by the
    /// lifecycle into `SupervisorState::Attaching.instance_id`. This is
    /// the value the supervisor later forwards over IPC, and a
    /// regression here would make the worker resolve the wrong device.
    #[tokio::test]
    async fn supervisor_apply_caches_instance_id() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        supervisor.apply(true).await.expect("apply(true)");
        assert_eq!(supervisor.state_label().await, "Attaching");
        assert!(!supervisor.is_active().await);

        // The instance id stored in Attaching must match the one we
        // would later forward over IPC.
        let payload = VirtualDisplayAttachResultPayload {
            instance_id: MOCK_INSTANCE_ID.to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
        };
        supervisor.on_worker_attach_result(payload).await;
        // The fact that the matching attach-result promoted to Attached
        // proves the stored id was `MOCK_INSTANCE_ID`. A mismatch would
        // have been silently dropped (see the mismatch test below).
        assert_eq!(supervisor.state_label().await, "Attached");
        assert!(supervisor.is_active().await);
    }

    /// This is the v1 regression test: a successful
    /// `send_to_worker(AttachVirtualDisplay)` must NOT by itself
    /// promote the state machine. Promotion is gated on an explicit
    /// worker reply via [`on_worker_attach_result`].
    #[tokio::test]
    async fn supervisor_on_capabilities_sends_attach_but_does_not_promote_state() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        supervisor.apply(true).await.expect("apply(true)");

        // The WorkerManager mock has no worker registered, so the
        // attach send below will return Err. Test both paths anyway:
        // even when the send succeeds (e.g. enqueue accepted), the
        // supervisor stays Attaching. With the unbound channel mock
        // currently shipped, the send fails — exercising the warn
        // path of on_worker_capabilities — but the assertion still
        // holds: NO promotion.
        supervisor.on_worker_capabilities().await;
        assert_eq!(supervisor.state_label().await, "Attaching");
        assert!(
            !supervisor.is_active().await,
            "router gate must still reject ChangeDisplaySettings until \
             the worker has confirmed via attach-result",
        );
    }

    /// Happy path: worker replies `Attached(name)` for the currently
    /// tracked instance id → supervisor flips to `Attached` and
    /// `is_active()` returns `true`. Receiving a second `Attached`
    /// reply for the same id is idempotent.
    #[tokio::test]
    async fn supervisor_on_attach_result_attached_promotes_to_attached() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        supervisor.apply(true).await.expect("apply(true)");
        assert_eq!(supervisor.state_label().await, "Attaching");

        let payload = VirtualDisplayAttachResultPayload {
            instance_id: MOCK_INSTANCE_ID.to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
        };
        supervisor.on_worker_attach_result(payload.clone()).await;
        assert_eq!(supervisor.state_label().await, "Attached");
        assert!(supervisor.is_active().await);

        // Idempotent: a second Attached reply must not regress state.
        supervisor.on_worker_attach_result(payload).await;
        assert_eq!(supervisor.state_label().await, "Attached");
        assert!(supervisor.is_active().await);
    }

    /// Worker replied `Failed(_)` for the currently tracked instance
    /// id → supervisor stays in `Attaching`, `is_active()` remains
    /// `false`. The next `Capabilities` (worker restart / desktop
    /// switch) will trigger another `on_worker_capabilities` send and
    /// give the worker another chance to resolve.
    #[tokio::test]
    async fn supervisor_on_attach_result_failed_stays_attaching() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        supervisor.apply(true).await.expect("apply(true)");

        let payload = VirtualDisplayAttachResultPayload {
            instance_id: MOCK_INSTANCE_ID.to_string(),
            outcome: VirtualDisplayAttachOutcome::Failed(
                "EnumDisplayDevicesW returned seen=[] after 6 retries".to_string(),
            ),
        };
        supervisor.on_worker_attach_result(payload).await;
        assert_eq!(supervisor.state_label().await, "Attaching");
        assert!(
            !supervisor.is_active().await,
            "router must still reject ChangeDisplaySettings after worker Failed",
        );
    }

    /// Worker reply carrying a different `instance_id` (e.g. stale
    /// reply from a previous daemon incarnation) must be dropped with
    /// no state change. Mismatch is detected even for an otherwise
    /// well-formed `Attached(_)` outcome.
    #[tokio::test]
    async fn supervisor_on_attach_result_ignores_mismatched_instance_id() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        supervisor.apply(true).await.expect("apply(true)");

        let mismatched = VirtualDisplayAttachResultPayload {
            instance_id: "SWD\\OTHER\\OTHER".to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY9".to_string()),
        };
        supervisor.on_worker_attach_result(mismatched).await;
        assert_eq!(supervisor.state_label().await, "Attaching");
        assert!(!supervisor.is_active().await);
    }

    /// Worker reply that lands when the supervisor is `Disabled`
    /// (e.g. operator toggled the feature off in the same window) must
    /// be silently dropped without panicking.
    #[tokio::test]
    async fn supervisor_on_attach_result_ignored_when_disabled() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        // Never call apply(true) — start from Disabled.
        let payload = VirtualDisplayAttachResultPayload {
            instance_id: MOCK_INSTANCE_ID.to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
        };
        supervisor.on_worker_attach_result(payload).await;
        assert_eq!(supervisor.state_label().await, "Disabled");
        assert!(!supervisor.is_active().await);
    }

    #[tokio::test]
    async fn supervisor_on_capabilities_no_op_when_disabled() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        // Disabled state: on_worker_capabilities should be a no-op
        // (no panic, no state change).
        supervisor.on_worker_capabilities().await;
        assert!(!supervisor.is_active().await);
    }

    #[tokio::test]
    async fn supervisor_shutdown_drops_handle() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        supervisor.apply(true).await.expect("apply(true)");
        supervisor.shutdown().await;
        assert!(!supervisor.is_active().await);
    }

    /// Drain all currently buffered messages out of the in-process IPC
    /// `ipc_rx` without ever awaiting — the supervisor sends to an
    /// unbounded channel so all enqueued messages are observable
    /// immediately after the call that produced them.
    fn drain_ipc(
        ipc_rx: &mut tokio::sync::mpsc::UnboundedReceiver<
            desk_ipc_protocol::message::ServiceToWorker,
        >,
    ) -> Vec<desk_ipc_protocol::message::ServiceToWorker> {
        let mut out = Vec::new();
        while let Ok(msg) = ipc_rx.try_recv() {
            out.push(msg);
        }
        out
    }

    /// v4 RefreshCapabilities path: the `Attaching → Attached`
    /// promotion must enqueue exactly one `RefreshCapabilities` on the
    /// daemon's worker channel so the worker re-publishes its display
    /// enumeration (which now includes the freshly attached IDD).
    #[tokio::test]
    async fn supervisor_on_attach_result_attached_emits_refresh_capabilities_to_worker() {
        use desk_ipc_protocol::message::ServiceToWorker;
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

        supervisor.apply(true).await.expect("apply(true)");
        let _ = drain_ipc(&mut ipc_rx);

        let payload = VirtualDisplayAttachResultPayload {
            instance_id: MOCK_INSTANCE_ID.to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
        };
        supervisor.on_worker_attach_result(payload).await;
        let sent = drain_ipc(&mut ipc_rx);
        let refresh_count = sent
            .iter()
            .filter(|m| matches!(m, ServiceToWorker::RefreshCapabilities))
            .count();
        assert_eq!(
            refresh_count, 1,
            "Attaching -> Attached promotion must emit exactly one \
             RefreshCapabilities, observed: {sent:?}"
        );
    }

    /// Edge-trigger discipline: a second `Attached(_)` reply for an
    /// already-Attached supervisor must not re-emit
    /// `RefreshCapabilities`. Without this guard the worker would be
    /// asked to re-publish capabilities every time the daemon
    /// re-issued an AttachVirtualDisplay (which happens on each
    /// `WorkerToService::Capabilities`), turning a one-shot refresh
    /// into a per-Capabilities ping-pong.
    #[tokio::test]
    async fn supervisor_on_attach_result_attached_does_not_emit_refresh_when_already_attached() {
        use desk_ipc_protocol::message::ServiceToWorker;
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        supervisor.apply(true).await.expect("apply(true)");

        let payload = VirtualDisplayAttachResultPayload {
            instance_id: MOCK_INSTANCE_ID.to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
        };
        // First Attached: edge fires.
        supervisor.on_worker_attach_result(payload.clone()).await;
        let _ = drain_ipc(&mut ipc_rx);

        // Second Attached on already-Attached supervisor: no edge.
        supervisor.on_worker_attach_result(payload).await;
        let sent = drain_ipc(&mut ipc_rx);
        let refresh_count = sent
            .iter()
            .filter(|m| matches!(m, ServiceToWorker::RefreshCapabilities))
            .count();
        assert_eq!(
            refresh_count, 0,
            "second Attached on an already-Attached supervisor must not \
             re-emit RefreshCapabilities; observed: {sent:?}"
        );
    }

    /// Detach path is symmetric: when the supervisor's `shutdown`
    /// transitions away from `Attaching` / `Attached`, the worker
    /// must be told to re-publish capabilities so any browser that
    /// reconnects no longer sees the IDD in the dropdown.
    #[tokio::test]
    async fn supervisor_detach_emits_refresh_capabilities_to_worker() {
        use desk_ipc_protocol::message::ServiceToWorker;
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        supervisor.apply(true).await.expect("apply(true)");
        let _ = drain_ipc(&mut ipc_rx);

        supervisor.shutdown().await;
        let sent = drain_ipc(&mut ipc_rx);
        let refresh_count = sent
            .iter()
            .filter(|m| matches!(m, ServiceToWorker::RefreshCapabilities))
            .count();
        let detach_count = sent
            .iter()
            .filter(|m| matches!(m, ServiceToWorker::DetachVirtualDisplay))
            .count();
        assert_eq!(
            detach_count, 1,
            "shutdown must emit one DetachVirtualDisplay; observed: {sent:?}"
        );
        assert_eq!(
            refresh_count, 1,
            "shutdown must emit one RefreshCapabilities after the detach; \
             observed: {sent:?}"
        );
    }

    /// Backwards-compat: the commit-4 test for the
    /// `newly_constructed_supervisor_is_inactive` invariant still
    /// holds with the commit-5 constructor signature.
    #[tokio::test]
    async fn newly_constructed_supervisor_is_inactive() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        assert!(!supervisor.is_active().await);
    }

    // ===== v5 lazy lifecycle: ensure_attached + lifecycle_lock =====

    /// Build a `MediaCapabilities` whose `video_device_list` contains
    /// exactly one display under the `"wgc"` bucket. Used by tests that
    /// want to simulate the worker's post-attach `Capabilities` refresh.
    fn caps_with_display(display_name: &str) -> desk_ipc_protocol::message::MediaCapabilities {
        use desk_ipc_protocol::message::MediaCapabilities;
        use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};
        let mut video_device_list: std::collections::BTreeMap<String, Vec<DisplayInfo>> =
            std::collections::BTreeMap::new();
        video_device_list.insert(
            "wgc".to_string(),
            vec![DisplayInfo {
                device_name: display_name.to_string(),
                display_device_name: None,
                desktop_coordinates: DisplayRect {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },
                resolutions: vec![],
                attached_to_desktop: true,
                rotation: 0,
            }],
        );
        MediaCapabilities {
            video_codecs: vec![],
            audio_codecs: vec![],
            video_encoders: vec![],
            audio_encoders: vec![],
            video_device_list,
            audio_device_list: std::collections::BTreeMap::new(),
            has_tauri: false,
            is_admin: false,
            desktop_name: "Default".to_string(),
        }
    }

    /// `apply(true)` is the lazy bring-up entry point — it must
    /// proactively enqueue an `AttachVirtualDisplay` IPC instead of
    /// waiting for a future `Capabilities` re-emission, otherwise
    /// `ensure_attached` would sit in Attaching forever in the
    /// post-initial-Capabilities steady state.
    #[tokio::test]
    async fn apply_true_sends_attach_virtual_display_to_worker() {
        use desk_ipc_protocol::message::ServiceToWorker;
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

        supervisor.apply(true).await.expect("apply(true)");
        let sent = drain_ipc(&mut ipc_rx);
        let attach_count = sent
            .iter()
            .filter(|m| matches!(m, ServiceToWorker::AttachVirtualDisplay(_)))
            .count();
        assert_eq!(
            attach_count, 1,
            "apply(true) must emit exactly one AttachVirtualDisplay; observed: {sent:?}",
        );
    }

    /// `apply(false)` from `Attached` must clear the
    /// `attached_capabilities_target` watch so a stale post-detach
    /// `ensure_attached` call cannot fast-path through on the previous
    /// target.
    #[tokio::test]
    async fn apply_false_sends_detach_refresh_and_clears_target() {
        use desk_ipc_protocol::message::ServiceToWorker;
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

        supervisor.apply(true).await.expect("apply(true)");
        supervisor
            .on_worker_attach_result(VirtualDisplayAttachResultPayload {
                instance_id: MOCK_INSTANCE_ID.to_string(),
                outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
            })
            .await;
        assert!(
            supervisor.attached_capabilities_target.borrow().is_some(),
            "promotion sets target",
        );
        let _ = drain_ipc(&mut ipc_rx);

        supervisor.apply(false).await.expect("apply(false)");
        let sent = drain_ipc(&mut ipc_rx);
        assert_eq!(
            sent.iter()
                .filter(|m| matches!(m, ServiceToWorker::DetachVirtualDisplay))
                .count(),
            1,
            "apply(false) emits DetachVirtualDisplay; observed: {sent:?}",
        );
        assert_eq!(
            sent.iter()
                .filter(|m| matches!(m, ServiceToWorker::RefreshCapabilities))
                .count(),
            1,
            "apply(false) emits RefreshCapabilities; observed: {sent:?}",
        );
        assert!(
            supervisor.attached_capabilities_target.borrow().is_none(),
            "apply(false) clears target",
        );
        assert_eq!(supervisor.state_label().await, "Disabled");
    }

    /// The `Attaching → Attached` promotion records both the
    /// `display_name` (for the cache-contains check) and the
    /// post-promotion `capabilities_version` target (snapshot + 1).
    #[tokio::test]
    async fn promotion_stores_display_name_and_sets_target_snapshot_plus_one() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        // Seed cap_version to 1 via a no-op Capabilities install so the
        // snapshot is non-zero (matches the typical bring-up flow where
        // the worker has already emitted at least one Capabilities).
        worker_mgr.set_worker_capabilities(caps_with_display(r"\\.\OTHER"));
        assert_eq!(worker_mgr.capabilities_version(), 1);
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
        supervisor.apply(true).await.expect("apply(true)");

        supervisor
            .on_worker_attach_result(VirtualDisplayAttachResultPayload {
                instance_id: MOCK_INSTANCE_ID.to_string(),
                outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
            })
            .await;

        // display_name lands in state.
        let stored = match &*supervisor.state.read().await {
            SupervisorState::Attached { display_name, .. } => Some(display_name.clone()),
            _ => None,
        };
        assert_eq!(stored.as_deref(), Some(r"\\.\DISPLAY4"));
        // target == snapshot + 1 == 2.
        assert_eq!(*supervisor.attached_capabilities_target.borrow(), Some(2));
    }

    /// Fast-path: `ensure_attached` returns `Attached` without doing
    /// anything when the supervisor is fully attached AND the cache
    /// already includes the attached display.
    #[tokio::test]
    async fn ensure_attached_fast_path_when_target_and_cache_match() {
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        // `new_attached_for_test` seeds capabilities + target itself.
        let supervisor =
            VirtualDisplaySupervisor::new_attached_for_test(worker_mgr, "SWD\\TEST\\TEST");

        let outcome = supervisor
            .ensure_attached(std::time::Duration::from_millis(100))
            .await;
        assert!(matches!(outcome, EnsureAttachedOutcome::Attached));
    }

    /// Codex round 3 #13: even if target is satisfied by cap_version,
    /// the cache must actually contain the attached display name for
    /// the ensure_attached completion to fire.
    #[tokio::test]
    async fn ensure_attached_waits_when_target_satisfied_but_cache_missing_display() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, _ipc_rx) =
            tokio::sync::mpsc::unbounded_channel::<desk_ipc_protocol::message::ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr.clone()));
        supervisor.apply(true).await.expect("apply(true)");

        // Promote with display "\\.\DISPLAY4".
        supervisor
            .on_worker_attach_result(VirtualDisplayAttachResultPayload {
                instance_id: MOCK_INSTANCE_ID.to_string(),
                outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
            })
            .await;
        // Background: bump cap_version with capabilities that DO NOT
        // include the attached display. ensure_attached must NOT
        // complete until the cache actually lists the IDD.
        let worker_mgr_bg = worker_mgr.clone();
        let bumper = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            // Wrong display name — should not satisfy the check.
            worker_mgr_bg.set_worker_capabilities(caps_with_display(r"\\.\OTHER"));
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            // Now publish capabilities that include the attached IDD.
            worker_mgr_bg.set_worker_capabilities(caps_with_display(r"\\.\DISPLAY4"));
        });

        let outcome = supervisor
            .ensure_attached(std::time::Duration::from_secs(2))
            .await;
        bumper.await.unwrap();
        assert!(
            matches!(outcome, EnsureAttachedOutcome::Attached),
            "ensure_attached completed only after cache surfaced the IDD: {outcome:?}",
        );
    }

    /// An unrelated `Capabilities` bump (e.g. worker restart) must not
    /// satisfy `ensure_attached` — without the strict cache-contains
    /// check, the daemon could report Attached while the dropdown
    /// still lacks the IDD.
    #[tokio::test]
    async fn ensure_attached_ignores_unrelated_capabilities_bump_before_attached() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, _ipc_rx) =
            tokio::sync::mpsc::unbounded_channel::<desk_ipc_protocol::message::ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr.clone()));

        // Background: bump cap_version but never deliver attach_result.
        let worker_mgr_bg = worker_mgr.clone();
        let bumper = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            worker_mgr_bg.set_worker_capabilities(caps_with_display(r"\\.\OTHER"));
        });
        let outcome = supervisor
            .ensure_attached(std::time::Duration::from_millis(200))
            .await;
        bumper.await.unwrap();
        assert!(
            matches!(outcome, EnsureAttachedOutcome::TimedOut),
            "cap bump without attach_result must not complete; observed: {outcome:?}",
        );
    }

    /// Lazy bring-up from `Disabled`: ensure_attached kicks `apply(true)`
    /// internally; a background task simulates the worker's
    /// `attach_result` + `Capabilities` round-trip; ensure_attached
    /// returns `Attached`.
    #[tokio::test]
    async fn ensure_attached_brings_up_from_disabled() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, _ipc_rx) =
            tokio::sync::mpsc::unbounded_channel::<desk_ipc_protocol::message::ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr.clone()));

        let supervisor_bg = supervisor.clone();
        let worker_mgr_bg = worker_mgr.clone();
        let bumper = tokio::spawn(async move {
            // Wait for ensure_attached to issue apply(true).
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            supervisor_bg
                .on_worker_attach_result(VirtualDisplayAttachResultPayload {
                    instance_id: MOCK_INSTANCE_ID.to_string(),
                    outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY7".to_string()),
                })
                .await;
            // Worker's RefreshCapabilities response includes the IDD.
            worker_mgr_bg.set_worker_capabilities(caps_with_display(r"\\.\DISPLAY7"));
        });

        let outcome = supervisor
            .ensure_attached(std::time::Duration::from_secs(2))
            .await;
        bumper.await.unwrap();
        assert!(matches!(outcome, EnsureAttachedOutcome::Attached));
        assert_eq!(supervisor.state_label().await, "Attached");
    }

    /// Timeout path: no `attach_result` ever lands. State stays in
    /// `Attaching` so the next ensure call resumes from there.
    #[tokio::test]
    async fn ensure_attached_times_out_when_attach_never_completes() {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, _ipc_rx) =
            tokio::sync::mpsc::unbounded_channel::<desk_ipc_protocol::message::ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

        let outcome = supervisor
            .ensure_attached(std::time::Duration::from_millis(100))
            .await;
        assert!(matches!(outcome, EnsureAttachedOutcome::TimedOut));
        assert_eq!(
            supervisor.state_label().await,
            "Attaching",
            "state must remain Attaching after timeout so the next ensure resumes",
        );
    }

    /// Codex round 2 #9 / round 3 #9: when a previous ensure_attached
    /// timed out with the supervisor stuck in Attaching (e.g. the first
    /// Attach IPC was lost before the worker channel was installed), a
    /// subsequent ensure_attached must re-send the AttachVirtualDisplay
    /// IPC so the worker eventually gets the request.
    #[tokio::test]
    async fn ensure_attached_resends_attach_when_still_attaching() {
        use desk_ipc_protocol::message::ServiceToWorker;
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

        supervisor.apply(true).await.expect("apply(true)");
        let initial = drain_ipc(&mut ipc_rx);
        assert_eq!(
            initial
                .iter()
                .filter(|m| matches!(m, ServiceToWorker::AttachVirtualDisplay(_)))
                .count(),
            1,
            "first attach send observed",
        );

        // Second ensure call: state is still Attaching (no attach_result
        // arrived). The ensure_attached fast-path miss should trigger a
        // re-send before the wait loop.
        let _ = supervisor
            .ensure_attached(std::time::Duration::from_millis(50))
            .await;
        let resent = drain_ipc(&mut ipc_rx);
        assert!(
            resent
                .iter()
                .any(|m| matches!(m, ServiceToWorker::AttachVirtualDisplay(_))),
            "subsequent ensure must re-send AttachVirtualDisplay when state is Attaching; \
             observed: {resent:?}",
        );
    }

    /// `Unavailable`: provider returns `NotSupported` (stub platforms).
    /// ensure_attached must surface the error promptly.
    #[tokio::test]
    async fn ensure_attached_returns_unavailable_when_provider_not_supported() {
        let provider: Box<dyn VirtualDisplayLifecycle> =
            Box::new(MockLifecycle::returns_not_supported());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

        let outcome = supervisor
            .ensure_attached(std::time::Duration::from_millis(100))
            .await;
        match outcome {
            EnsureAttachedOutcome::Unavailable(_) => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    /// Codex round 4 #15: lifecycle_lock must serialise the entire
    /// apply flow including IPC sends so concurrent calls cannot
    /// interleave. Specifically: an apply(false) running between an
    /// in-flight apply(true)'s state set and IPC send would let the
    /// worker observe Detach BEFORE the previous Attach completes its
    /// own IPC. The test launches one apply(true), waits for it to
    /// finish, then races apply(false) + apply(true) concurrently and
    /// asserts the IPC sequence is consistent with serialised
    /// execution (no Attach interleaved before a Detach of the same
    /// generation).
    #[tokio::test]
    async fn apply_serializes_concurrent_calls_via_lifecycle_lock() {
        use desk_ipc_protocol::message::ServiceToWorker;
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr));

        supervisor.apply(true).await.expect("first apply(true)");
        let _ = drain_ipc(&mut ipc_rx);

        // Concurrent apply(false) followed by apply(true). lifecycle_lock
        // must force them to serialise; the IPC stream observed afterwards
        // must contain Detach before any second Attach.
        let s1 = supervisor.clone();
        let s2 = supervisor.clone();
        let t1 = tokio::spawn(async move { s1.apply(false).await });
        // Small skew so apply(false) wins the lock first deterministically.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let t2 = tokio::spawn(async move { s2.apply(true).await });
        let _ = t1.await.unwrap();
        let _ = t2.await.unwrap();

        let sent = drain_ipc(&mut ipc_rx);
        // Locate the Detach. It must precede any AttachVirtualDisplay that
        // appears after it (the second apply(true)).
        let detach_idx = sent
            .iter()
            .position(|m| matches!(m, ServiceToWorker::DetachVirtualDisplay))
            .expect("detach must be present in the IPC stream");
        let second_attach_idx = sent
            .iter()
            .enumerate()
            .skip(detach_idx + 1)
            .find_map(|(i, m)| {
                if matches!(m, ServiceToWorker::AttachVirtualDisplay(_)) {
                    Some(i)
                } else {
                    None
                }
            });
        assert!(
            second_attach_idx.is_some(),
            "second apply(true) must enqueue Attach after Detach; observed: {sent:?}",
        );
    }

    /// Two concurrent `ensure_attached` calls must share a single
    /// underlying bring-up: provider.create() must run exactly once,
    /// and both calls must observe `Attached`.
    #[tokio::test]
    async fn ensure_attached_concurrent_calls_share_single_apply() {
        let lifecycle = Arc::new(MockLifecycle::returns_handle());
        let lifecycle_for_provider = Arc::clone(&lifecycle);
        struct ArcProvider(Arc<MockLifecycle>);
        impl VirtualDisplayLifecycle for ArcProvider {
            fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
                self.0.create()
            }
        }
        let provider: Box<dyn VirtualDisplayLifecycle> =
            Box::new(ArcProvider(lifecycle_for_provider));
        let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
        let (ipc_tx, _ipc_rx) =
            tokio::sync::mpsc::unbounded_channel::<desk_ipc_protocol::message::ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr.clone()));

        let supervisor_bg = supervisor.clone();
        let worker_mgr_bg = worker_mgr.clone();
        let driver = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            supervisor_bg
                .on_worker_attach_result(VirtualDisplayAttachResultPayload {
                    instance_id: MOCK_INSTANCE_ID.to_string(),
                    outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY8".to_string()),
                })
                .await;
            worker_mgr_bg.set_worker_capabilities(caps_with_display(r"\\.\DISPLAY8"));
        });

        let s1 = supervisor.clone();
        let s2 = supervisor.clone();
        let h1 =
            tokio::spawn(
                async move { s1.ensure_attached(std::time::Duration::from_secs(2)).await },
            );
        let h2 =
            tokio::spawn(
                async move { s2.ensure_attached(std::time::Duration::from_secs(2)).await },
            );
        let r1 = h1.await.unwrap();
        let r2 = h2.await.unwrap();
        driver.await.unwrap();

        assert!(
            matches!(r1, EnsureAttachedOutcome::Attached),
            "first: {r1:?}"
        );
        assert!(
            matches!(r2, EnsureAttachedOutcome::Attached),
            "second: {r2:?}"
        );
        assert_eq!(
            lifecycle.create_calls.load(Ordering::SeqCst),
            1,
            "provider.create must be called at most once",
        );
    }

    /// `record_applied_mode` stores the full mode the driver applied
    /// (width × height × refresh) from the worker's echo. Any zero
    /// component skips the update so a malformed echo cannot wipe a
    /// prior valid observation. `last_known_mode()` only reports a
    /// fully-observed mode.
    #[test]
    fn supervisor_records_full_mode_on_applied() {
        let (worker_mgr, _rx) = make_worker_mgr();
        let s = VirtualDisplaySupervisor::new(
            Box::new(MockLifecycle::returns_not_supported()),
            worker_mgr,
        );

        // Initial state: nothing observed yet.
        assert_eq!(s.last_refresh_hz(), 0);
        assert!(s.last_known_mode().is_none());

        // First fully-formed Applied caches all three.
        s.record_applied_mode(2560, 1440, 120);
        assert_eq!(s.last_refresh_hz(), 120);
        assert_eq!(s.last_known_mode(), Some((2560, 1440, 120)));

        // Any zero component is treated as "no observation" and the
        // whole update is skipped — refresh + dimensions all stay put.
        s.record_applied_mode(0, 1440, 60);
        assert_eq!(s.last_known_mode(), Some((2560, 1440, 120)));
        s.record_applied_mode(1920, 0, 60);
        assert_eq!(s.last_known_mode(), Some((2560, 1440, 120)));
        s.record_applied_mode(1920, 1080, 0);
        assert_eq!(s.last_known_mode(), Some((2560, 1440, 120)));

        // A subsequent fully-valid Applied does overwrite.
        s.record_applied_mode(1920, 1080, 60);
        assert_eq!(s.last_known_mode(), Some((1920, 1080, 60)));
        assert_eq!(s.last_refresh_hz(), 60);
    }

    /// `attached_display_name()` returns the GDI device name only while
    /// the supervisor is `Attached`. Every other state (`Disabled`,
    /// `Attaching`, `Detaching`) returns `None`. `pc_manager` reads this
    /// to populate `InitSignalingData::virtual_display_device_name`.
    #[tokio::test]
    async fn supervisor_attached_display_name_only_when_attached() {
        let (worker_mgr, _rx) = make_worker_mgr();
        // Disabled: brand-new supervisor.
        let disabled = VirtualDisplaySupervisor::new(
            Box::new(MockLifecycle::returns_not_supported()),
            worker_mgr.clone(),
        );
        assert_eq!(disabled.attached_display_name().await, None);

        // Attached via the pre-promoted test helper.
        let attached =
            VirtualDisplaySupervisor::new_attached_for_test(worker_mgr.clone(), "SWD\\TEST\\TEST");
        assert_eq!(
            attached.attached_display_name().await.as_deref(),
            Some("\\\\.\\TESTDISPLAY"),
        );

        // Attaching: bring up via apply(true) with a handle-returning
        // provider, but never deliver the attach result.
        let (worker_mgr2, _rx2) = make_worker_mgr();
        let attaching =
            VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_handle()), worker_mgr2);
        attaching.apply(true).await.expect("apply(true) succeeds");
        assert_eq!(attaching.state_label().await, "Attaching");
        assert_eq!(attaching.attached_display_name().await, None);
    }

    /// Codex round 1 #1: `apply(false)` ending an attach generation
    /// must clear cached width/height (so a stale 2560x1440 cannot
    /// fake-short-circuit the next request) while preserving the
    /// refresh hint.
    #[tokio::test]
    async fn supervisor_apply_false_clears_dimensions_keeps_refresh() {
        let (worker_mgr, _rx) = make_worker_mgr();
        let s =
            VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_handle()), worker_mgr);
        // Bring the supervisor up to Attaching so the (Attaching, false)
        // arm exercises the dimension reset.
        s.apply(true).await.expect("apply(true) succeeds");
        // Seed a full cached mode as if the worker had echoed Applied.
        s.record_applied_mode(2560, 1440, 60);
        assert_eq!(s.last_known_mode(), Some((2560, 1440, 60)));

        s.apply(false).await.expect("apply(false) succeeds");

        assert!(
            s.last_known_mode().is_none(),
            "dimensions must be cleared on tear-down so a future re-attach \
             does not inherit a stale fake-short-circuit cache",
        );
        assert_eq!(
            s.last_refresh_hz(),
            60,
            "refresh is preserved as an operator hint across attach generations",
        );
    }

    /// Codex round 1 #1: `apply(true)` starting an attach generation
    /// also clears stale dimensions, regardless of what the previous
    /// detach left behind.
    #[tokio::test]
    async fn supervisor_apply_true_clears_dimensions_keeps_refresh() {
        let (worker_mgr, _rx) = make_worker_mgr();
        let s =
            VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_handle()), worker_mgr);
        // Pretend we previously had an attach cycle that left dimensions
        // cached (skipping the `apply(true)` reset). This mirrors the
        // shape `apply(false)` could not reach in the absence of a fresh
        // bring-up.
        s.record_applied_mode(2560, 1440, 144);

        s.apply(true).await.expect("apply(true) succeeds");

        assert!(s.last_known_mode().is_none());
        assert_eq!(s.last_refresh_hz(), 144);
    }

    /// Codex round 2 #2: every `Attached` outcome — including the
    /// already-Attached re-entry path that worker restart takes —
    /// must clear cached dimensions. The Attaching→Attached promotion
    /// edge is exercised implicitly by the apply(true) chain in other
    /// tests; this one pins the *already-Attached* branch.
    #[tokio::test]
    async fn supervisor_on_worker_attach_result_already_attached_clears_dimensions() {
        let (worker_mgr, _rx) = make_worker_mgr();
        let s = VirtualDisplaySupervisor::new_attached_for_test(worker_mgr, "SWD\\TEST\\TEST");
        // Seed cached dimensions inside an existing Attached state.
        s.record_applied_mode(2560, 1440, 120);
        assert_eq!(s.last_known_mode(), Some((2560, 1440, 120)));
        assert_eq!(s.state_label().await, "Attached");

        // Re-send the attach result with the same instance id; this
        // lands on the already-Attached branch in on_worker_attach_result.
        s.on_worker_attach_result(VirtualDisplayAttachResultPayload {
            instance_id: "SWD\\TEST\\TEST".to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached("\\\\.\\TESTDISPLAY".to_string()),
        })
        .await;

        assert!(
            s.last_known_mode().is_none(),
            "already-Attached re-entry (worker restart path) must clear \
             stale dimensions even though no state transition fires",
        );
        assert_eq!(s.last_refresh_hz(), 120, "refresh must survive");
        assert_eq!(s.state_label().await, "Attached");
    }

    /// The first call ever to `try_consume_auto_slot` must always
    /// succeed — there is no prior timestamp to compare against and no
    /// reason to make the operator wait `min_interval` after boot.
    #[test]
    fn supervisor_auto_slot_first_call_succeeds() {
        let (worker_mgr, _rx) = make_worker_mgr();
        let s = VirtualDisplaySupervisor::new(
            Box::new(MockLifecycle::returns_not_supported()),
            worker_mgr,
        );

        let allowed = s.try_consume_auto_slot(Instant::now(), Duration::from_secs(60));
        assert!(allowed, "first try_consume_auto_slot must always succeed");
    }

    /// Two calls within `min_interval` ⇒ the second is rejected. After
    /// the interval has elapsed the slot becomes available again. We
    /// pass synthetic `Instant`s (relative to a baseline) so the test
    /// is wall-clock independent.
    #[test]
    fn supervisor_auto_slot_throttles_within_interval() {
        let (worker_mgr, _rx) = make_worker_mgr();
        let s = VirtualDisplaySupervisor::new(
            Box::new(MockLifecycle::returns_not_supported()),
            worker_mgr,
        );
        let base = Instant::now();
        // First call at t=0 succeeds.
        assert!(s.try_consume_auto_slot(base, Duration::from_millis(1000)));
        // 500 ms later — interval not elapsed ⇒ false, last_at unchanged.
        assert!(!s.try_consume_auto_slot(
            base + Duration::from_millis(500),
            Duration::from_millis(1000)
        ));
        // 1500 ms after the first slot ⇒ interval elapsed ⇒ true.
        assert!(s.try_consume_auto_slot(
            base + Duration::from_millis(1500),
            Duration::from_millis(1000)
        ));
    }

    /// `min_interval` is taken from the caller (router reads it from
    /// `settings.virtual_display.adaptive_throttle_ms`). Different
    /// intervals on subsequent calls must drive different behaviour —
    /// pins that the supervisor never caches the interval.
    #[test]
    fn supervisor_auto_slot_respects_dynamic_interval() {
        let (worker_mgr, _rx) = make_worker_mgr();
        let s = VirtualDisplaySupervisor::new(
            Box::new(MockLifecycle::returns_not_supported()),
            worker_mgr,
        );
        let base = Instant::now();
        // Consume slot at t=0 with a long interval.
        assert!(s.try_consume_auto_slot(base, Duration::from_millis(2000)));
        // 1500 ms later, still within the 2000 ms window ⇒ false.
        assert!(!s.try_consume_auto_slot(
            base + Duration::from_millis(1500),
            Duration::from_millis(2000)
        ));
        // Same elapsed (1500 ms), but caller now passes 500 ms ⇒ true
        // (interval is per-call, not state).
        assert!(s.try_consume_auto_slot(
            base + Duration::from_millis(1500),
            Duration::from_millis(500)
        ));
    }

    /// `min_interval=0` is the operator-configured "no throttle" mode.
    /// Two back-to-back calls (Δ ≈ 0) must both succeed.
    #[test]
    fn supervisor_auto_slot_zero_interval_never_throttles() {
        let (worker_mgr, _rx) = make_worker_mgr();
        let s = VirtualDisplaySupervisor::new(
            Box::new(MockLifecycle::returns_not_supported()),
            worker_mgr,
        );
        let now = Instant::now();
        assert!(s.try_consume_auto_slot(now, Duration::from_millis(0)));
        // Δ = 0 between two calls with min_interval = 0 ⇒ second
        // still succeeds (0 >= 0).
        assert!(s.try_consume_auto_slot(now, Duration::from_millis(0)));
    }

    // ───── Exclusive-mode tests ─────

    fn fresh_supervisor() -> Arc<VirtualDisplaySupervisor> {
        let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
        let (worker_mgr, _rx) = make_worker_mgr();
        new_arc(provider, worker_mgr)
    }

    async fn read_inner(s: &VirtualDisplaySupervisor) -> (ExclusiveState, u64) {
        let inner = s.exclusive_inner.read().await;
        (inner.state, inner.current_op_id)
    }

    /// `set_desired_exclusive(true)` updates the flag + prompt and
    /// notifies the driver loop. With no `desired_computer` installed
    /// and no active state behind the supervisor, the driver loop
    /// produces `Send { Enter }` once because (Idle, true) matches
    /// the transition table. We do not assert the IPC went out here;
    /// the worker_mgr has no installed channel so the send errors,
    /// the rollback brings state back to Idle, and the driver loop
    /// goes to sleep on the next notification.
    #[tokio::test]
    async fn set_desired_exclusive_idle_true_advances_to_entering_and_bumps_op_id() {
        let s = fresh_supervisor();
        // Manually drive prepare_next_action to inspect state advancement
        // without depending on the driver loop's send result.
        s.exclusive_desired.store(true, Ordering::SeqCst);
        match s.prepare_next_action().await {
            ExclusiveAction::Send {
                next_state,
                op_id,
                prev_state,
                ..
            } => {
                assert_eq!(next_state, ExclusiveState::Entering);
                assert_eq!(prev_state, ExclusiveState::Idle);
                assert_eq!(op_id, 1);
            }
            other => panic!("expected Send, got {other:?}"),
        }
        let (state, op_id) = read_inner(&s).await;
        assert_eq!(state, ExclusiveState::Entering);
        assert_eq!(op_id, 1);
        s.shutdown_driver_loop().await;
    }

    /// `prepare_next_action` returns None when (Idle, false): no
    /// state advancement, no op_id bump.
    #[tokio::test]
    async fn prepare_next_action_idle_false_is_none() {
        let s = fresh_supervisor();
        match s.prepare_next_action().await {
            ExclusiveAction::None => {}
            other => panic!("expected None, got {other:?}"),
        }
        let (state, op_id) = read_inner(&s).await;
        assert_eq!(state, ExclusiveState::Idle);
        assert_eq!(op_id, 0);
        s.shutdown_driver_loop().await;
    }

    /// `on_exclusive_result` with matching `op_id` advances Entering
    /// to Active; with a mismatched `op_id` it is a no-op.
    #[tokio::test]
    async fn on_exclusive_result_op_id_gate() {
        let s = fresh_supervisor();
        s.exclusive_desired.store(true, Ordering::SeqCst);
        s.prepare_next_action().await; // state -> Entering, op_id -> 1

        // Stale op_id: dropped silently.
        s.on_exclusive_result(ExclusiveResultPayload {
            op_id: 999,
            direction: ExclusiveDirection::Entering,
            outcome: ExclusiveOutcome::Entered,
        })
        .await;
        let (state, op_id) = read_inner(&s).await;
        assert_eq!(state, ExclusiveState::Entering, "stale op must not advance");
        assert_eq!(op_id, 1);

        // Matching op_id: transitions to Active.
        s.on_exclusive_result(ExclusiveResultPayload {
            op_id: 1,
            direction: ExclusiveDirection::Entering,
            outcome: ExclusiveOutcome::Entered,
        })
        .await;
        let (state, op_id) = read_inner(&s).await;
        assert_eq!(state, ExclusiveState::Active);
        assert_eq!(op_id, 1, "successful result does not bump op_id");
        s.shutdown_driver_loop().await;
    }

    /// `apply_result_transition` table (codex round 6 #5: only four
    /// outcomes; EnterCancelled was removed).
    #[test]
    fn apply_result_transition_table() {
        // Entering + Entered -> Active
        assert_eq!(
            apply_result_transition(
                ExclusiveState::Entering,
                &ExclusiveResultPayload {
                    op_id: 1,
                    direction: ExclusiveDirection::Entering,
                    outcome: ExclusiveOutcome::Entered,
                }
            ),
            ExclusiveState::Active
        );
        // Entering + EnterFailed -> Idle
        assert_eq!(
            apply_result_transition(
                ExclusiveState::Entering,
                &ExclusiveResultPayload {
                    op_id: 1,
                    direction: ExclusiveDirection::Entering,
                    outcome: ExclusiveOutcome::EnterFailed("bad".into()),
                }
            ),
            ExclusiveState::Idle
        );
        // Leaving + Left -> Idle
        assert_eq!(
            apply_result_transition(
                ExclusiveState::Leaving,
                &ExclusiveResultPayload {
                    op_id: 1,
                    direction: ExclusiveDirection::Leaving,
                    outcome: ExclusiveOutcome::Left,
                }
            ),
            ExclusiveState::Idle
        );
        // Codex follow-up P1 (2026-05-26):
        // Leaving + LeftWithErrors -> Active (was Idle). The bounded
        // retry budget + force-Idle on exhaustion lives in
        // `on_exclusive_result`, not in this pure transition function.
        assert_eq!(
            apply_result_transition(
                ExclusiveState::Leaving,
                &ExclusiveResultPayload {
                    op_id: 1,
                    direction: ExclusiveDirection::Leaving,
                    outcome: ExclusiveOutcome::LeftWithErrors("partial".into()),
                }
            ),
            ExclusiveState::Active
        );
        // Defensive: Leaving + Entered stays Leaving (stale ack would
        // already be dropped by op_id gate before reaching here; but
        // if it does, do not regress to Active).
        assert_eq!(
            apply_result_transition(
                ExclusiveState::Leaving,
                &ExclusiveResultPayload {
                    op_id: 1,
                    direction: ExclusiveDirection::Entering,
                    outcome: ExclusiveOutcome::Entered,
                }
            ),
            ExclusiveState::Leaving
        );
    }

    /// Codex follow-up P1 (2026-05-26): a first `LeftWithErrors`
    /// must transition the supervisor to `Active` (not `Idle`) so
    /// the reconciler can drive a retry, bump `leave_retry_count`,
    /// and set `next_leave_at` to the doubling schedule entry.
    /// `prepare_next_action` must then return `None` while the
    /// backoff is still in effect.
    #[tokio::test]
    async fn on_exclusive_result_left_with_errors_arms_retry() {
        let s = fresh_supervisor();
        // Move directly to Leaving with a known op_id so the gate fires.
        let op_id = {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Leaving;
            inner.current_op_id = 42;
            let _ = s
                .exclusive_state_watch
                .send_replace(ExclusiveState::Leaving);
            inner.current_op_id
        };
        // Mark exclusive as still desired-off so the reconciler would
        // want to drive Leaving again.
        s.exclusive_desired.store(false, Ordering::SeqCst);

        let before = Instant::now();
        s.on_exclusive_result(ExclusiveResultPayload {
            op_id,
            direction: ExclusiveDirection::Leaving,
            outcome: ExclusiveOutcome::LeftWithErrors("partial".into()),
        })
        .await;

        let inner = s.exclusive_inner.read().await;
        assert_eq!(
            inner.state,
            ExclusiveState::Active,
            "must go to Active for retry"
        );
        assert_eq!(inner.leave_retry_count, 1);
        let next_at = inner.next_leave_at.expect("backoff timer must be set");
        // Schedule entry for the first retry is LEAVE_RETRY_BASE_DELAY * 2^1 = 4 s.
        let scheduled_delay = next_at.saturating_duration_since(before);
        assert!(
            scheduled_delay >= Duration::from_secs(3),
            "expected ~4s delay, got {scheduled_delay:?}",
        );
        drop(inner);

        // While backoff is in effect, prepare_next_action must NOT
        // produce a leave action — even though state=Active &&
        // desired=false would otherwise transition to Leaving.
        let action = s.prepare_next_action().await;
        assert!(
            matches!(action, ExclusiveAction::None),
            "backoff gate must short-circuit prepare_next_action",
        );

        s.shutdown_driver_loop().await;
    }

    /// Codex follow-up P1: after [`MAX_LEAVE_RETRIES`] consecutive
    /// `LeftWithErrors`, the supervisor must force-Idle and reset
    /// `leave_retry_count` so a fresh enter cycle can proceed without
    /// inheriting stale budget.
    #[tokio::test]
    async fn on_exclusive_result_left_with_errors_exhausts_after_max_retries() {
        let s = fresh_supervisor();
        let mut op_id = {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Leaving;
            inner.current_op_id = 100;
            inner.current_op_id
        };

        for attempt in 1..=MAX_LEAVE_RETRIES {
            // Each result must match the current op_id.
            {
                let inner = s.exclusive_inner.read().await;
                op_id = inner.current_op_id;
                assert_eq!(inner.state, ExclusiveState::Leaving);
            }
            s.on_exclusive_result(ExclusiveResultPayload {
                op_id,
                direction: ExclusiveDirection::Leaving,
                outcome: ExclusiveOutcome::LeftWithErrors(format!("attempt {attempt}")),
            })
            .await;
            // Between retries, drive the state back to Leaving as if
            // the reconciler had picked it up (this isolates the unit
            // we are testing — on_exclusive_result's retry budget).
            if attempt < MAX_LEAVE_RETRIES {
                let mut inner = s.exclusive_inner.write().await;
                assert_eq!(inner.state, ExclusiveState::Active, "intermediate state");
                inner.state = ExclusiveState::Leaving;
            }
        }

        // After the final LeftWithErrors, state must be Idle and the
        // retry budget reset.
        let inner = s.exclusive_inner.read().await;
        assert_eq!(
            inner.state,
            ExclusiveState::Idle,
            "exhausted budget must force-Idle"
        );
        assert_eq!(inner.leave_retry_count, 0, "count must reset on give-up");
        assert!(inner.next_leave_at.is_none(), "no further retry scheduled");
        drop(inner);
        s.shutdown_driver_loop().await;
    }

    /// Codex follow-up P1: a successful `Left` (after one or more
    /// failed retries) must reset both `leave_retry_count` and
    /// `next_leave_at` — otherwise the *next* leave cycle inherits
    /// the stale backoff timer.
    #[tokio::test]
    async fn on_exclusive_result_left_resets_retry_state() {
        let s = fresh_supervisor();
        // Seed state as if we had just had a LeftWithErrors and are
        // now retrying Leaving.
        let op_id = {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Leaving;
            inner.current_op_id = 50;
            inner.leave_retry_count = 2;
            inner.next_leave_at = Some(Instant::now() + Duration::from_secs(60));
            inner.current_op_id
        };

        s.on_exclusive_result(ExclusiveResultPayload {
            op_id,
            direction: ExclusiveDirection::Leaving,
            outcome: ExclusiveOutcome::Left,
        })
        .await;

        let inner = s.exclusive_inner.read().await;
        assert_eq!(inner.state, ExclusiveState::Idle);
        assert_eq!(inner.leave_retry_count, 0);
        assert!(inner.next_leave_at.is_none());
        drop(inner);
        s.shutdown_driver_loop().await;
    }

    /// E2E fix 2026-05-27: a first `EnterFailed` must arm the
    /// enter-side backoff (count → 1, `next_enter_at` → now + 4 s)
    /// and `prepare_next_action` must short-circuit while the gate
    /// is still in effect. Symmetric to the LeftWithErrors test.
    #[tokio::test]
    async fn on_exclusive_result_enter_failed_arms_retry() {
        let s = fresh_supervisor();
        let op_id = {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Entering;
            inner.current_op_id = 11;
            let _ = s
                .exclusive_state_watch
                .send_replace(ExclusiveState::Entering);
            inner.current_op_id
        };
        // Desired stays true so the reconciler would want to retry.
        s.exclusive_desired.store(true, Ordering::SeqCst);

        let before = Instant::now();
        s.on_exclusive_result(ExclusiveResultPayload {
            op_id,
            direction: ExclusiveDirection::Entering,
            outcome: ExclusiveOutcome::EnterFailed("CDS BADMODE".into()),
        })
        .await;

        let inner = s.exclusive_inner.read().await;
        assert_eq!(
            inner.state,
            ExclusiveState::Idle,
            "EnterFailed transitions back to Idle (pure transition unchanged)"
        );
        assert_eq!(inner.enter_retry_count, 1);
        let next_at = inner.next_enter_at.expect("backoff timer must be set");
        let scheduled_delay = next_at.saturating_duration_since(before);
        // Schedule entry for first retry: ENTER_RETRY_BASE_DELAY * 2^1 = 4 s.
        assert!(
            scheduled_delay >= Duration::from_secs(3),
            "expected ~4s delay, got {scheduled_delay:?}",
        );
        // Desired is preserved while retries are still available — only
        // exhaustion drops it.
        assert!(
            s.exclusive_desired.load(Ordering::SeqCst),
            "desired must stay true while retries remain",
        );
        drop(inner);

        // Backoff gate must block (Idle, true) → Entering until the
        // timer elapses.
        let action = s.prepare_next_action().await;
        assert!(
            matches!(action, ExclusiveAction::None),
            "enter backoff gate must short-circuit prepare_next_action",
        );

        s.shutdown_driver_loop().await;
    }

    /// E2E fix 2026-05-27: after `MAX_ENTER_RETRIES` consecutive
    /// `EnterFailed`, the supervisor must clear `exclusive_desired`
    /// so the `(Idle, desired=true) → Entering` row stops firing.
    /// Counts must reset too so a fresh acquire later starts at zero.
    #[tokio::test]
    async fn on_exclusive_result_enter_failed_exhausts_after_max_retries() {
        let s = fresh_supervisor();
        s.exclusive_desired.store(true, Ordering::SeqCst);
        let mut op_id;
        {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Entering;
            inner.current_op_id = 200;
        }

        for attempt in 1..=MAX_ENTER_RETRIES {
            {
                let inner = s.exclusive_inner.read().await;
                op_id = inner.current_op_id;
                assert_eq!(inner.state, ExclusiveState::Entering);
            }
            s.on_exclusive_result(ExclusiveResultPayload {
                op_id,
                direction: ExclusiveDirection::Entering,
                outcome: ExclusiveOutcome::EnterFailed(format!("attempt {attempt}")),
            })
            .await;
            // Drive the state back to Entering between retries as if
            // the reconciler had picked it up (isolates the unit
            // under test — on_exclusive_result's retry budget).
            if attempt < MAX_ENTER_RETRIES {
                let mut inner = s.exclusive_inner.write().await;
                assert_eq!(inner.state, ExclusiveState::Idle, "intermediate state");
                inner.state = ExclusiveState::Entering;
                // Bump op_id like prepare_next_action would so each
                // retry round simulates the real reconciler.
                inner.current_op_id = inner.current_op_id.wrapping_add(1);
            }
        }

        // After exhaustion: state is Idle (always, on EnterFailed),
        // the retry budget is reset, AND desired has been cleared so
        // the reconciler will not pick this up again.
        let inner = s.exclusive_inner.read().await;
        assert_eq!(inner.state, ExclusiveState::Idle);
        assert_eq!(inner.enter_retry_count, 0, "count must reset on give-up");
        assert!(inner.next_enter_at.is_none(), "no further retry scheduled");
        drop(inner);
        assert!(
            !s.exclusive_desired.load(Ordering::SeqCst),
            "exhaustion must clear exclusive_desired to break the loop",
        );
        s.shutdown_driver_loop().await;
    }

    /// E2E fix 2026-05-27: a successful `Entered` (after one or more
    /// failed retries) must clear `enter_retry_count` and
    /// `next_enter_at` — otherwise the next attach inherits stale
    /// backoff bookkeeping.
    #[tokio::test]
    async fn on_exclusive_result_entered_resets_retry_state() {
        let s = fresh_supervisor();
        let op_id = {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Entering;
            inner.current_op_id = 77;
            inner.enter_retry_count = 2;
            inner.next_enter_at = Some(Instant::now() + Duration::from_secs(60));
            inner.current_op_id
        };

        s.on_exclusive_result(ExclusiveResultPayload {
            op_id,
            direction: ExclusiveDirection::Entering,
            outcome: ExclusiveOutcome::Entered,
        })
        .await;

        let inner = s.exclusive_inner.read().await;
        assert_eq!(inner.state, ExclusiveState::Active);
        assert_eq!(inner.enter_retry_count, 0);
        assert!(inner.next_enter_at.is_none());
        drop(inner);
        s.shutdown_driver_loop().await;
    }

    /// E2E fix 2026-05-27: `prepare_next_action` gates the enter
    /// path symmetrically to the leave path. With a pending
    /// `next_enter_at` in the future and `(Idle, desired=true)`, the
    /// call must return `None` instead of advancing to Entering.
    #[tokio::test]
    async fn prepare_next_action_gates_idle_true_on_next_enter_at() {
        let s = fresh_supervisor();
        {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Idle;
            inner.enter_retry_count = 1;
            inner.next_enter_at = Some(Instant::now() + Duration::from_secs(60));
        }
        s.exclusive_desired.store(true, Ordering::SeqCst);

        let action = s.prepare_next_action().await;
        assert!(
            matches!(action, ExclusiveAction::None),
            "enter backoff timer must short-circuit (Idle, true)",
        );
        // State must remain Idle (no spurious advance).
        let inner = s.exclusive_inner.read().await;
        assert_eq!(inner.state, ExclusiveState::Idle);
        s.shutdown_driver_loop().await;
    }

    /// Codex follow-up P1: `prepare_next_action` only honours the
    /// `next_leave_at` gate for the `(Active, desired=false)` retry
    /// row. Other rows ignore the gate entirely.
    #[tokio::test]
    async fn prepare_next_action_ignores_backoff_for_unrelated_transitions() {
        let s = fresh_supervisor();
        // Pre-seed a backoff timer + count as if a prior retry was in flight,
        // but switch state to Idle so the active row does NOT trigger.
        {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Idle;
            inner.leave_retry_count = 1;
            inner.next_leave_at = Some(Instant::now() + Duration::from_secs(60));
        }
        s.exclusive_desired.store(true, Ordering::SeqCst);

        let action = s.prepare_next_action().await;
        // (Idle, true) -> Entering: must NOT be gated by next_leave_at.
        assert!(
            matches!(
                action,
                ExclusiveAction::Send {
                    next_state: ExclusiveState::Entering,
                    ..
                }
            ),
            "non-leave transitions must ignore the backoff gate",
        );
        s.shutdown_driver_loop().await;
    }

    /// `rollback_send_failure` only reverses when (op_id, state) both
    /// match the recorded values. A concurrent reset that bumped
    /// op_id between the send attempt and the rollback must NOT
    /// regress the state.
    #[tokio::test]
    async fn rollback_send_failure_is_guarded() {
        let s = fresh_supervisor();
        s.exclusive_desired.store(true, Ordering::SeqCst);
        let (op_before, prev_state) = {
            let inner = s.exclusive_inner.read().await;
            (inner.current_op_id, inner.state)
        };
        s.prepare_next_action().await; // -> Entering, op_id +1
        let after_op = {
            let inner = s.exclusive_inner.read().await;
            inner.current_op_id
        };
        assert_eq!(after_op, op_before + 1);

        // Simulate a concurrent reset that bumps op_id again and
        // restores Idle. The pending rollback (which thinks it
        // recorded after_op + Entering) must NOT clobber it.
        s.reset_exclusive_state().await;
        let after_reset = {
            let inner = s.exclusive_inner.read().await;
            inner.current_op_id
        };
        assert!(after_reset > after_op);

        // Rollback referencing the stale (op_id, state) is a no-op.
        s.rollback_send_failure(after_op, ExclusiveState::Entering, prev_state)
            .await;
        let (state, op_id) = read_inner(&s).await;
        assert_eq!(state, ExclusiveState::Idle, "reset must survive rollback");
        assert_eq!(op_id, after_reset);
        s.shutdown_driver_loop().await;
    }

    /// `reset_exclusive_state` always returns state to Idle, bumps
    /// op_id, and flips desired off.
    #[tokio::test]
    async fn reset_exclusive_state_clears_and_bumps() {
        let s = fresh_supervisor();
        // Move into Active manually.
        {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Active;
            inner.current_op_id = 5;
        }
        s.exclusive_desired.store(true, Ordering::SeqCst);
        s.reset_exclusive_state().await;
        let (state, op_id) = read_inner(&s).await;
        assert_eq!(state, ExclusiveState::Idle);
        assert_eq!(op_id, 6);
        assert!(!s.exclusive_desired.load(Ordering::SeqCst));
        s.shutdown_driver_loop().await;
    }

    /// `await_exclusive_idle` returns immediately when already Idle.
    #[tokio::test]
    async fn await_exclusive_idle_returns_immediately_on_idle() {
        let s = fresh_supervisor();
        s.await_exclusive_idle(Duration::from_millis(100))
            .await
            .expect("immediate Ok");
        s.shutdown_driver_loop().await;
    }

    /// `await_exclusive_idle` times out when state is non-Idle and no
    /// transition arrives.
    #[tokio::test]
    async fn await_exclusive_idle_times_out_when_stuck() {
        let s = fresh_supervisor();
        {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Entering;
            let _ = s
                .exclusive_state_watch
                .send_replace(ExclusiveState::Entering);
        }
        // No state transition to Idle will arrive in the next 50ms.
        let res = s.await_exclusive_idle(Duration::from_millis(50)).await;
        assert!(res.is_err(), "expected timeout");
        s.shutdown_driver_loop().await;
    }

    /// `await_exclusive_idle` resolves when a state transition lands
    /// it on Idle.
    #[tokio::test]
    async fn await_exclusive_idle_resolves_on_transition() {
        let s = fresh_supervisor();
        {
            let mut inner = s.exclusive_inner.write().await;
            inner.state = ExclusiveState::Leaving;
            let _ = s
                .exclusive_state_watch
                .send_replace(ExclusiveState::Leaving);
        }
        let s_clone = Arc::clone(&s);
        let waiter =
            tokio::spawn(async move { s_clone.await_exclusive_idle(Duration::from_secs(2)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.reset_exclusive_state().await;
        let res = waiter.await.expect("join");
        assert!(res.is_ok());
        s.shutdown_driver_loop().await;
    }
}
