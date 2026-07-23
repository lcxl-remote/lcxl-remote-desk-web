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
        _handle: VirtualDisplayHandle,
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
        _handle: VirtualDisplayHandle,
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
                            _handle: handle,
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
                        _handle,
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
                            _handle,
                        }
                    }
                    SupervisorState::Attached {
                        instance_id,
                        display_name: existing_name,
                        _handle,
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
                            _handle,
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
        // Reset the leave-retry bookkeeping too —
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
        // Use bounded backoff retry on
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
        // Gate the
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
/// `(Leaving, LeftWithErrors)` now
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
                _handle: handle,
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
mod tests;
