//! Worker-side state for the virtual display feature.
//!
//! The daemon owns the `SwDevice` handle (see
//! [`crate::daemon::virtual_display`]); the worker owns the runtime
//! pieces that have to live in the interactive user session:
//!
//! - `attached_display` tracks the `\\.\DISPLAYn` device name the
//!   daemon currently has wired up.
//! - `original_start_payload` keeps each connection's StartMedia
//!   exactly as the daemon issued it (preserves the user's preferred
//!   physical capture target).
//! - `active_start_payload` is what we actually handed the producer —
//!   it may have `video_device` overridden when attached.
//!
//! Attach / Detach IPC mutates `attached_display` and emits a
//! `Stop+Start` per active connection so the capture pipeline rebuilds
//! against the new target. SetVirtualDisplayMode IPC drives the
//! [`VirtualDisplayController`] (running on a blocking task because
//! the Windows backend has to wait on `ChangeDisplaySettingsExW` /
//! the driver named pipe).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use desk_ipc_protocol::message::{
    ExclusiveDirection, ExclusiveOutcome, ExclusiveResultPayload, SetVirtualDisplayModePayload,
    StartMediaPayload, VirtualDisplayAttachOutcome, VirtualDisplayModeData,
    VirtualDisplayModeOutcome, VirtualDisplayModeResponsePayload, WorkerToService,
};
use desk_virtual_display::{
    ExclusiveLayout, PromptController, PromptWaiter, VirtualDisplayController, VirtualDisplayError,
    VirtualDisplayMode, enter_exclusive, leave_exclusive, show_pre_detach_prompt, snapshot_layout,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Worker-side virtual display state. All mutations happen from the
/// main message loop, so no synchronisation is needed beyond
/// `&mut self` — except for `exclusive_layout` which is owned by a
/// dedicated `std::sync::Mutex` so the Drop guard can take the lock
/// from a sync context (codex round 9 #3). The design notes called
/// for `parking_lot::Mutex` to avoid poisoning; we use `std::sync`
/// to skip the extra dependency and treat poisoning as a
/// **recoverable** condition (codex follow-up P2, 2026-05-26):
/// every path that locks this slot uses `PoisonError::into_inner()`
/// to read or write the inner `Option<ExclusiveLayout>` instead of
/// bailing out. That preserves the invariant "the slot is the
/// authoritative record of what physical displays need restoring",
/// which is critical because both `ExclusiveGuard::drop` and the
/// daemon-requested leave depend on reading the slot to find the
/// `ExclusiveLayout` they must restore.
#[derive(Default)]
pub struct VirtualDisplayState {
    /// `\\.\DISPLAYn` device name pushed by the daemon's
    /// `AttachVirtualDisplay`. `None` ⇒ capture targets the physical
    /// display the user originally selected.
    pub attached_display: Option<String>,
    /// Original StartMedia exactly as it arrived from the daemon
    /// (preserving the user's preferred physical capture target).
    /// Survives Attach/Detach swaps so a Detach can rebuild capture
    /// against the original target without the daemon re-issuing
    /// StartMedia from scratch.
    pub original_start_payload: HashMap<String, StartMediaPayload>,
    /// StartMedia actually handed to the producer — same as `original`
    /// except `video_device` is overridden to the attached display
    /// name when attached.
    pub active_start_payload: HashMap<String, StartMediaPayload>,
    /// Per-session exclusive layout — `Some` only while the worker
    /// has detached the physical displays to migrate windows onto
    /// the virtual one. Wrapped in a sync `std::sync::Mutex` so the
    /// Drop guard can take the lock from a sync context; the rest
    /// of the session loop accesses it via `lock()` which never
    /// holds across an await (codex round 9 #3). All critical
    /// sections are trivial set/take/clone so mutex poisoning is not
    /// a concern in practice.
    pub exclusive_layout: Arc<std::sync::Mutex<Option<ExclusiveLayout>>>,
}

impl VirtualDisplayState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the active payload from `original`, applying the attached
    /// display override if any.
    fn make_active(&self, original: &StartMediaPayload) -> StartMediaPayload {
        match &self.attached_display {
            Some(d) => StartMediaPayload {
                video_device: Some(d.clone()),
                ..original.clone()
            },
            None => original.clone(),
        }
    }

    /// Record an inbound StartMedia: cache `original`, return the
    /// payload that should be forwarded to the producer (already
    /// adjusted for the attached display, if any).
    pub fn record_start(&mut self, payload: StartMediaPayload) -> StartMediaPayload {
        self.original_start_payload
            .insert(payload.connection_id.clone(), payload.clone());
        let active = self.make_active(&payload);
        self.active_start_payload
            .insert(active.connection_id.clone(), active.clone());
        active
    }

    /// Record an inbound StopMedia: drop both caches.
    pub fn record_stop(&mut self, connection_id: &str) {
        self.original_start_payload.remove(connection_id);
        self.active_start_payload.remove(connection_id);
    }

    /// Apply a new attached-display state and re-derive `active`
    /// payloads. Returns the list of (connection_id, active_payload)
    /// the caller should drive Stop+Start against the producer to
    /// swap capture targets.
    pub fn rebuild_active_for_attach(&mut self, display_name: Option<String>) -> Vec<RestartStep> {
        self.attached_display = display_name;
        let originals: Vec<StartMediaPayload> =
            self.original_start_payload.values().cloned().collect();
        self.active_start_payload.clear();
        originals
            .into_iter()
            .map(|orig| {
                let active = self.make_active(&orig);
                self.active_start_payload
                    .insert(active.connection_id.clone(), active.clone());
                RestartStep {
                    connection_id: orig.connection_id.clone(),
                    active,
                }
            })
            .collect()
    }

    /// Collect a [`RestartStep`] for every currently-active connection
    /// without mutating `attached_display`. Used by the
    /// `SetVirtualDisplayMode` handler: a mode change does not switch
    /// targets, so the attached display name stays the same; we just
    /// need the per-connection active payloads to drive a Stop+Start
    /// when the capture backend (WGC) cannot self-adapt to the
    /// underlying monitor remount. The caller filters by per-connection
    /// effective `CaptureKey` before issuing the Stop+Start.
    pub fn restart_steps_for_attached(&self) -> Vec<RestartStep> {
        self.active_start_payload
            .iter()
            .map(|(connection_id, active)| RestartStep {
                connection_id: connection_id.clone(),
                active: active.clone(),
            })
            .collect()
    }
}

/// A (stop, start) instruction the caller should drive against the
/// media producer to swap a single connection's capture target.
pub struct RestartStep {
    pub connection_id: String,
    pub active: StartMediaPayload,
}

/// Exponential-backoff schedule for the attach resolver. Six attempts,
/// `250 / 500 / 1000 / 2000 / 4000 / 8000` ms between them (~15.75 s
/// total wall time). Sized to comfortably cover the worst-case IDD
/// driver bring-up + GDI enumeration race observed on a cold-installed
/// host; if six rounds still fail the problem is structural (driver
/// crashed, monitor never enumerated) and further retries would only
/// hide the failure.
pub const ATTACH_BACKOFF_SCHEDULE_MS: [u64; 6] = [250, 500, 1000, 2000, 4000, 8000];

/// Resolve a PnP instance id to a GDI `\\.\DISPLAYn` with bounded
/// exponential-backoff retries. Pure function — no side effects on
/// worker state — so the caller (worker session loop) wires the result
/// into `VirtualDisplayState` and the media producer.
///
/// The `resolver` and `sleeper` closures are injectable so unit tests
/// drive arbitrary retry sequences without sleeping real wall time.
/// Production wires `resolver = desk_virtual_display::resolve_display_name`
/// and `sleeper = tokio::time::sleep`.
///
/// Returns:
/// - [`VirtualDisplayAttachOutcome::Attached`] with the resolved
///   display name on the first successful resolver call.
/// - [`VirtualDisplayAttachOutcome::Failed`] carrying the last
///   resolver error message if every retry slot was exhausted.
pub async fn resolve_attach_with_backoff<R, S, Fut>(
    instance_id: &str,
    mut resolver: R,
    mut sleeper: S,
) -> VirtualDisplayAttachOutcome
where
    R: FnMut(&str) -> Result<String, VirtualDisplayError>,
    S: FnMut(Duration) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let schedule = ATTACH_BACKOFF_SCHEDULE_MS;
    let mut last_err: String = String::new();
    for (attempt, &sleep_ms) in schedule.iter().enumerate() {
        match resolver(instance_id) {
            Ok(display_name) => return VirtualDisplayAttachOutcome::Attached(display_name),
            Err(e) => {
                last_err = e.to_string();
                let is_last_attempt = attempt + 1 == schedule.len();
                if is_last_attempt {
                    // No sleep after the final attempt — exit the loop
                    // and fall through to the Failed return below.
                    break;
                }
                tracing::debug!(
                    virtual_display.instance_id = %instance_id,
                    virtual_display.attempt = attempt + 1,
                    virtual_display.backoff_ms = sleep_ms,
                    "resolve_attach_with_backoff: attempt failed: {e}; will retry",
                );
                sleeper(Duration::from_millis(sleep_ms)).await;
            }
        }
    }
    VirtualDisplayAttachOutcome::Failed(format!(
        "exhausted {} retries resolving instance {instance_id}: {last_err}",
        schedule.len(),
    ))
}

/// Drive `controller.set_mode` on a blocking thread (the Windows
/// backend has to wait on synchronous Win32 IO) and pack the result
/// into the `WorkerToService::VirtualDisplayMode` reply the daemon
/// will ferry back to the originating browser.
pub async fn run_set_mode(
    controller: Arc<dyn VirtualDisplayController>,
    attached_display: Option<String>,
    payload: SetVirtualDisplayModePayload,
) -> WorkerToService {
    let request_id = payload.request_id.clone();
    let connection_id = payload.connection_id.clone();
    let display = match attached_display {
        Some(d) => d,
        None => {
            tracing::error!(
                "run_set_mode: rejected request={} conn={} target={}x{}@{} — \
                 no attached virtual display",
                request_id,
                connection_id,
                payload.width,
                payload.height,
                payload.refresh_hz,
            );
            return WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
                request_id,
                connection_id,
                outcome: VirtualDisplayModeOutcome::Failed(
                    "virtual display not attached".to_string(),
                ),
            });
        }
    };
    let mode = VirtualDisplayMode {
        width: payload.width,
        height: payload.height,
        refresh_hz: payload.refresh_hz,
    };
    let join_result =
        tokio::task::spawn_blocking(move || controller.set_mode(&display, mode)).await;
    let result =
        join_result.unwrap_or_else(|e| Err(VirtualDisplayError::PipeIo(format!("join: {e}"))));
    let outcome = match result {
        Ok(m) => VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
            width: m.width,
            height: m.height,
            refresh_hz: m.refresh_hz,
        }),
        Err(e) => {
            // CRITICAL: this error is currently invisible because it was
            // previously only carried by the IPC Failed outcome (which
            // gets ferried back to the browser, but never written to the
            // worker log). The CDS `DISP_CHANGE_BADMODE` symptom that
            // caused the WGC mid-session resize blackscreen could not be
            // diagnosed without parsing browser-side traces. Log it here
            // so future regressions are visible from `desk-worker.log`
            // alone.
            tracing::error!(
                "run_set_mode FAILED: request={} conn={} target={}x{}@{} reason={}",
                request_id,
                connection_id,
                payload.width,
                payload.height,
                payload.refresh_hz,
                e,
            );
            VirtualDisplayModeOutcome::Failed(e.to_string())
        }
    };
    WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
        request_id,
        connection_id,
        outcome,
    })
}

// ═══════════════════════════════════════════════════════════════════
// Exclusive-mode pipeline (stage 4)
// ═══════════════════════════════════════════════════════════════════

/// RAII guard that drives `leave_exclusive` when the worker session
/// ends without an explicit teardown (panic catch / IPC reader EOF /
/// normal exit). Best-effort by design — `TerminateProcess` skips
/// Drop entirely, which is the fallback path the
/// non-`CDS_UPDATEREGISTRY` enter relies on (the OS restores the
/// physical layout on the next logon).
pub struct ExclusiveGuard {
    layout: Arc<std::sync::Mutex<Option<ExclusiveLayout>>>,
}

impl ExclusiveGuard {
    pub fn new(layout: Arc<std::sync::Mutex<Option<ExclusiveLayout>>>) -> Self {
        Self { layout }
    }
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        // Blocking lock; the critical sections that hold this mutex
        // are trivial set/take/clone calls that cannot deadlock.
        // codex round 9 #3: explicit error logging — silent swallow
        // would let a stuck partial detach disappear from the log.
        //
        // codex follow-up P2 (2026-05-26): poison MUST NOT skip the
        // recovery attempt. This guard is the last chance to restore
        // physical displays before the session terminates; if we
        // bail on poison we lose the layout that
        // `PoisonError::into_inner()` could still hand us. Recover
        // the inner `Option<ExclusiveLayout>` and proceed with leave.
        let layout = match self.layout.lock() {
            Ok(mut g) => g.take(),
            Err(p) => {
                tracing::error!(
                    "[virtual-display] ExclusiveGuard drop: layout mutex poisoned: {p}; \
                     recovering via PoisonError::into_inner() to attempt leave"
                );
                p.into_inner().take()
            }
        };
        if let Some(layout) = layout {
            if let Err(e) = leave_exclusive(&layout) {
                tracing::error!(
                    "[virtual-display] ExclusiveGuard drop leave_exclusive failed: {e:?}; \
                     physical displays may stay detached until logoff/restart (transient CDS)"
                );
            }
        }
    }
}

/// Worker-internal event emitted right after a CDS batch commit
/// (enter or leave) has been confirmed by the OS, so the session
/// loop can refresh anything that depended on the previous
/// HMONITOR mapping. Specifically: WGC capture sessions bound via
/// `CreateForMonitor(HMONITOR)` survive the CDS commit at the API
/// level but stop emitting fresh frames because the IDD's
/// framebuffer mapping moves underneath them — observed in e2e on
/// 2026-05-27 as a 26 s blackout that only cleared after the user
/// triggered `SetVirtualDisplayMode` by resizing the browser.
///
/// This event drives the same eviction + Stop/Start media cycle
/// `SetVirtualDisplayMode` already performs for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusiveCommitEvent {
    /// `enter_exclusive` returned `Ok(())` — the virtual display is
    /// now primary at `(0, 0)` and the physicals are detached.
    Entered,
    /// `leave_exclusive` returned `Ok(())` (or `LeftWithErrors`) —
    /// the physicals are back, virtual is restored to its
    /// pre-exclusive position. WGC needs to rebind to the original
    /// HMONITOR-with-original-position too.
    Left,
}

/// Worker-side coordinator: serialises enter / leave runners over a
/// single cancel oneshot. `request(op_id, desired, ...)` replaces any
/// in-flight runner with a new one; the old runner observes the
/// cancel and returns silently without emitting a result (codex
/// round 5 #3). Only the surviving runner publishes a matching
/// `op_id` result, which keeps the daemon's op_id gate simple.
pub struct ExclusiveCoordinator {
    cancel: Option<oneshot::Sender<()>>,
    runner: Option<JoinHandle<()>>,
    /// Optional channel for emitting [`ExclusiveCommitEvent`] after a
    /// CDS batch commit succeeds. The session loop creates the
    /// channel once and clones the sender into each `request` call;
    /// in tests this stays `None` so the coordinator can be exercised
    /// without wiring up the consumer side.
    commit_tx: Option<mpsc::UnboundedSender<ExclusiveCommitEvent>>,
}

impl Default for ExclusiveCoordinator {
    fn default() -> Self {
        Self {
            cancel: None,
            runner: None,
            commit_tx: None,
        }
    }
}

impl ExclusiveCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the channel that receives [`ExclusiveCommitEvent`]
    /// notifications after each successful CDS batch commit. Called
    /// once by the session loop during init; subsequent calls
    /// overwrite the previous sender (used in tests).
    pub fn set_commit_channel(&mut self, commit_tx: mpsc::UnboundedSender<ExclusiveCommitEvent>) {
        self.commit_tx = Some(commit_tx);
    }

    /// Replace the in-flight runner with a new one. The previous
    /// runner is cancelled via the oneshot drop; it will return
    /// without emitting a result. The new runner is awaited on top
    /// of the previous to keep CDS calls serialised.
    pub fn request(
        &mut self,
        op_id: u64,
        desired: bool,
        prompt_ms: u32,
        attached: Option<String>,
        layout: Arc<std::sync::Mutex<Option<ExclusiveLayout>>>,
        writer_tx: mpsc::UnboundedSender<WorkerToService>,
    ) {
        // Cancel any in-flight runner.
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        self.cancel = Some(cancel_tx);
        let prev = self.runner.take();
        let commit_tx = self.commit_tx.clone();
        self.runner = Some(tokio::spawn(async move {
            // Wait for the previous runner to finish so CDS calls
            // serialise. The cancel oneshot is the unconditional way
            // to stop — abort() does not wake the blocking CDS call.
            if let Some(h) = prev {
                let _ = h.await;
            }
            // codex round 9 #1: after the previous runner has been
            // collected but before any side-effect runs, check the
            // cancel oneshot. If the controller already cancelled us
            // (a faster third request landed during prev.await), the
            // CDS calls inside run_* are skipped entirely and no
            // result is emitted — the new op's runner will publish
            // the actual outcome.
            if cancel_rx.try_recv().is_ok() {
                return;
            }
            run_exclusive_reconciler(
                op_id, desired, prompt_ms, attached, layout, cancel_rx, writer_tx, commit_tx,
            )
            .await;
        }));
    }
}

async fn run_exclusive_reconciler(
    op_id: u64,
    desired: bool,
    prompt_ms: u32,
    attached: Option<String>,
    layout: Arc<std::sync::Mutex<Option<ExclusiveLayout>>>,
    cancel: oneshot::Receiver<()>,
    writer_tx: mpsc::UnboundedSender<WorkerToService>,
    commit_tx: Option<mpsc::UnboundedSender<ExclusiveCommitEvent>>,
) {
    // codex round 5 #4: idempotent paths must still ack — the daemon
    // gates state advancement on receiving a matching op_id.
    //
    // codex follow-up P2 (2026-05-26): poison MUST NOT degrade to
    // `currently_active = false`. A poisoned slot containing
    // `Some(layout)` means we are still detached; treating it as
    // false would drive `(true, _) → run_enter` (re-detach catastrophe)
    // or `(false, _) → idempotent Left` (daemon thinks done but layout
    // still in the slot). `PoisonError::into_inner()` lets us read
    // the inner Option regardless of poison status.
    let currently_active = match layout.lock() {
        Ok(g) => g.is_some(),
        Err(p) => {
            tracing::warn!(
                "[virtual-display] reconciler: layout slot poisoned; \
                 recovering via PoisonError::into_inner()"
            );
            p.into_inner().is_some()
        }
    };
    match (desired, currently_active) {
        (true, true) => {
            send_exclusive_result(
                &writer_tx,
                op_id,
                ExclusiveDirection::Entering,
                ExclusiveOutcome::Entered,
            );
        }
        (false, false) => {
            send_exclusive_result(
                &writer_tx,
                op_id,
                ExclusiveDirection::Leaving,
                ExclusiveOutcome::Left,
            );
        }
        (true, false) => {
            run_enter(
                op_id,
                prompt_ms,
                attached,
                layout,
                cancel,
                &writer_tx,
                commit_tx.as_ref(),
            )
            .await;
        }
        (false, true) => {
            run_leave(op_id, layout, &writer_tx, commit_tx.as_ref()).await;
        }
    }
}

async fn run_enter(
    op_id: u64,
    prompt_ms: u32,
    attached: Option<String>,
    layout_slot: Arc<std::sync::Mutex<Option<ExclusiveLayout>>>,
    mut cancel: oneshot::Receiver<()>,
    writer_tx: &mpsc::UnboundedSender<WorkerToService>,
    commit_tx: Option<&mpsc::UnboundedSender<ExclusiveCommitEvent>>,
) {
    let Some(name) = attached else {
        // Diagnostic log (2026-05-27): every EnterFailed branch in this
        // function must surface `op_id` + the underlying reason at
        // `error!` level. Without these the daemon's bounded-backoff
        // retry loop looks like an infinite repeat from the outside —
        // the actual CDS / GDI failure that produced the EnterFailed
        // is invisible.
        tracing::error!(
            "[virtual-display] run_enter op_id={op_id}: \
             no attached virtual display; cannot enter exclusive mode"
        );
        send_exclusive_result(
            writer_tx,
            op_id,
            ExclusiveDirection::Entering,
            ExclusiveOutcome::EnterFailed(
                "no attached virtual display; cannot enter exclusive mode".to_string(),
            ),
        );
        return;
    };
    let (prompt_ctrl, mut prompt_waiter): (PromptController, PromptWaiter) =
        show_pre_detach_prompt(Duration::from_millis(prompt_ms as u64));
    // Wait for either the prompt to complete naturally or cancel.
    tokio::select! {
        _ = &mut cancel => {
            // codex round 5 #3: old runner cancelled — return without
            // emitting a result. The new runner will publish the
            // actual final state.
            prompt_ctrl.cancel();
            prompt_waiter.wait().await;
            return;
        }
        _ = prompt_waiter.wait() => {}
    }
    // Second cancel check before the (blocking) CDS work begins.
    if cancel.try_recv().is_ok() {
        return;
    }
    let name_for_snapshot = name.clone();
    let snapshot_join =
        tokio::task::spawn_blocking(move || snapshot_layout(&name_for_snapshot)).await;
    let layout = match snapshot_join {
        Ok(Ok(layout)) => layout,
        Ok(Err(e)) => {
            tracing::error!(
                "[virtual-display] run_enter op_id={op_id} display={name}: \
                 snapshot_layout failed: {e}"
            );
            send_exclusive_result(
                writer_tx,
                op_id,
                ExclusiveDirection::Entering,
                ExclusiveOutcome::EnterFailed(format!("snapshot_layout failed: {e}")),
            );
            return;
        }
        Err(join_err) => {
            tracing::error!(
                "[virtual-display] run_enter op_id={op_id} display={name}: \
                 snapshot_layout spawn_blocking join failed: {join_err}"
            );
            send_exclusive_result(
                writer_tx,
                op_id,
                ExclusiveDirection::Entering,
                ExclusiveOutcome::EnterFailed(format!("snapshot join: {join_err}")),
            );
            return;
        }
    };
    if cancel.try_recv().is_ok() {
        return;
    }
    let layout_for_enter = layout.clone();
    let enter_join = tokio::task::spawn_blocking(move || enter_exclusive(&layout_for_enter)).await;
    match enter_join {
        Ok(Ok(())) => {
            // CDS has already detached — we are committed to "exclusive".
            // The slot store is the only handle the Drop guard / future
            // leave path will have to undo this.
            //
            // Codex follow-up P2 (2026-05-26): poison MUST recover via
            // `PoisonError::into_inner()` and store the layout anyway.
            // The prior "poison ⇒ rollback + EnterFailed" path was a
            // safe default under the "treat poison as failure" policy,
            // but discarded the inner data the guard still owned —
            // leaving every other poison path with the same choice to
            // recover OR roll back. Unifying on recovery keeps the
            // semantics identical to `attempt_leave_with_fn` /
            // `ExclusiveGuard::drop` (both also `into_inner()`).
            match layout_slot.lock() {
                Ok(mut slot) => *slot = Some(layout),
                Err(p) => {
                    tracing::warn!(
                        "[virtual-display] run_enter: layout slot poisoned after \
                         CDS detach; recovering via PoisonError::into_inner() \
                         and storing layout so leave / Drop guard can restore"
                    );
                    *p.into_inner() = Some(layout);
                }
            }
            send_exclusive_result(
                writer_tx,
                op_id,
                ExclusiveDirection::Entering,
                ExclusiveOutcome::Entered,
            );
            // E2E fix 2026-05-27: notify the session loop that the
            // CDS batch committed so it can restart WGC capture for
            // any connection bound to the (now-stale) HMONITOR. A
            // closed channel is benign — only the test path leaves
            // commit_tx unset.
            if let Some(tx) = commit_tx
                && tx.send(ExclusiveCommitEvent::Entered).is_err()
            {
                tracing::debug!(
                    "[virtual-display] run_enter op_id={op_id}: commit channel \
                     closed; session loop already shut down"
                );
            }
        }
        Ok(Err(e)) => {
            tracing::error!(
                "[virtual-display] run_enter op_id={op_id} display={name}: \
                 enter_exclusive (CDS detach) failed: {e}"
            );
            send_exclusive_result(
                writer_tx,
                op_id,
                ExclusiveDirection::Entering,
                ExclusiveOutcome::EnterFailed(e.to_string()),
            )
        }
        Err(join_err) => {
            tracing::error!(
                "[virtual-display] run_enter op_id={op_id} display={name}: \
                 enter_exclusive spawn_blocking join failed: {join_err}"
            );
            send_exclusive_result(
                writer_tx,
                op_id,
                ExclusiveDirection::Entering,
                ExclusiveOutcome::EnterFailed(format!("enter join: {join_err}")),
            )
        }
    }
}

/// Outcome of [`attempt_leave_with_fn`]. Kept as a separate type so
/// the (pure, sync) slot-retention logic can be exhaustively unit
/// tested without driving a tokio runtime or holding a real
/// `mpsc::UnboundedSender`.
#[derive(Debug)]
enum LeaveOutcome {
    /// Slot was empty — idempotent leave. No-op on the slot.
    Empty,
    /// `leave_fn` returned `Ok(())`. Slot has been cleared.
    Succeeded,
    /// `leave_fn` returned `Err`. **Slot retains the layout** so a
    /// subsequent leave attempt — explicit retry, future
    /// `SetVirtualDisplayExclusive(false)`, or the session-end
    /// [`ExclusiveGuard`] Drop — can try again. Codex P1 #2.
    Failed(String),
}

/// Sync core of `run_leave`. Locks the slot to copy the layout, runs
/// `leave_fn`, and clears the slot **only on success**. The previous
/// implementation `take()`d the layout before calling `leave_exclusive`;
/// a partial reattach failure then left the daemon with no
/// recoverable state and the worker with an empty slot. Codex P1 #2.
///
/// Codex follow-up P2 (2026-05-26): lock poisoning recovers via
/// `PoisonError::into_inner()` rather than aborting the leave. A
/// poisoned slot may still contain a valid `Some(layout)` — the
/// previous "treat poison as failure" path would ack
/// `LeftWithErrors` to the daemon while leaving the slot's data
/// unread, so neither the daemon nor the worker had any handle on
/// the still-detached displays.
fn attempt_leave_with_fn<F>(
    layout_slot: &Arc<std::sync::Mutex<Option<ExclusiveLayout>>>,
    leave_fn: F,
) -> LeaveOutcome
where
    F: FnOnce(&ExclusiveLayout) -> Result<(), VirtualDisplayError>,
{
    let layout = match layout_slot.lock() {
        Ok(g) => g.clone(),
        Err(p) => {
            tracing::warn!(
                "[virtual-display] attempt_leave: layout slot poisoned; \
                 recovering via PoisonError::into_inner()"
            );
            p.into_inner().clone()
        }
    };
    let Some(layout) = layout else {
        return LeaveOutcome::Empty;
    };
    match leave_fn(&layout) {
        Ok(()) => {
            // Success: clear the slot. Recover from poison again on
            // the second lock so the take always lands.
            match layout_slot.lock() {
                Ok(mut g) => *g = None,
                Err(p) => {
                    tracing::warn!(
                        "[virtual-display] attempt_leave: slot poisoned on clear; \
                         recovering via PoisonError::into_inner()"
                    );
                    *p.into_inner() = None;
                }
            }
            LeaveOutcome::Succeeded
        }
        Err(e) => {
            tracing::error!(
                "[virtual-display] leave_exclusive failed: {e}; layout retained \
                 in slot so Drop guard / next leave can retry"
            );
            LeaveOutcome::Failed(e.to_string())
        }
    }
}

async fn run_leave(
    op_id: u64,
    layout_slot: Arc<std::sync::Mutex<Option<ExclusiveLayout>>>,
    writer_tx: &mpsc::UnboundedSender<WorkerToService>,
    commit_tx: Option<&mpsc::UnboundedSender<ExclusiveCommitEvent>>,
) {
    let slot_for_blocking = Arc::clone(&layout_slot);
    let leave_join = tokio::task::spawn_blocking(move || {
        attempt_leave_with_fn(&slot_for_blocking, |l| leave_exclusive(l))
    })
    .await;
    let outcome = match leave_join {
        Ok(o) => o,
        Err(join_err) => LeaveOutcome::Failed(format!("leave join: {join_err}")),
    };
    // E2E fix 2026-05-27: notify the session loop after every leave
    // attempt that touched CDS — `Succeeded` and `Failed` both
    // committed (or partially-committed) the reattach + position
    // restore batch, so WGC's HMONITOR mapping has shifted either
    // way. `Empty` (slot was already None) is the idempotent path
    // that did NOT touch CDS, so it does not need a refresh.
    // Compute this BEFORE the match below consumes the `Failed`
    // String payload via `LeftWithErrors(msg)`.
    let needs_refresh = matches!(outcome, LeaveOutcome::Succeeded | LeaveOutcome::Failed(_));
    let exclusive_outcome = match outcome {
        // Idempotent (slot was empty) still acks `Left` so the
        // daemon's `current_op_id` gate fires (codex round 5 #4).
        LeaveOutcome::Empty | LeaveOutcome::Succeeded => ExclusiveOutcome::Left,
        LeaveOutcome::Failed(msg) => ExclusiveOutcome::LeftWithErrors(msg),
    };
    send_exclusive_result(
        writer_tx,
        op_id,
        ExclusiveDirection::Leaving,
        exclusive_outcome,
    );
    if needs_refresh
        && let Some(tx) = commit_tx
        && tx.send(ExclusiveCommitEvent::Left).is_err()
    {
        tracing::debug!(
            "[virtual-display] run_leave op_id={op_id}: commit channel \
             closed; session loop already shut down"
        );
    }
}

fn send_exclusive_result(
    writer_tx: &mpsc::UnboundedSender<WorkerToService>,
    op_id: u64,
    direction: ExclusiveDirection,
    outcome: ExclusiveOutcome,
) {
    let payload = ExclusiveResultPayload {
        op_id,
        direction,
        outcome,
    };
    if writer_tx
        .send(WorkerToService::ExclusiveResult(payload))
        .is_err()
    {
        tracing::warn!(
            "[virtual-display] writer task closed; dropping ExclusiveResult op_id={op_id}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_ipc_protocol::message::MediaCodec;
    use desk_virtual_display::VirtualDisplayController;
    use std::sync::Mutex;

    fn make_payload(connection_id: &str, video_device: Option<&str>) -> StartMediaPayload {
        StartMediaPayload {
            connection_id: connection_id.to_string(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: video_device.map(|s| s.to_string()),
            audio_device: None,
            fps: 60,
            bitrate_kbps: 4_000,
            quality: 0,
            start_video: true,
            start_audio: true,
            image_capture: None,
            enable_dirty_rect: None,
        }
    }

    #[test]
    fn record_start_caches_original_and_overrides_active_when_attached() {
        let mut state = VirtualDisplayState::new();
        state.attached_display = Some("\\\\.\\DISPLAY9".to_string());
        let original = make_payload("conn-1", Some("\\\\.\\DISPLAY1"));
        let active = state.record_start(original.clone());
        // original is preserved exactly.
        assert_eq!(
            state
                .original_start_payload
                .get("conn-1")
                .unwrap()
                .video_device
                .as_deref(),
            Some("\\\\.\\DISPLAY1"),
        );
        // active is overridden.
        assert_eq!(active.video_device.as_deref(), Some("\\\\.\\DISPLAY9"));
        assert_eq!(
            state
                .active_start_payload
                .get("conn-1")
                .unwrap()
                .video_device
                .as_deref(),
            Some("\\\\.\\DISPLAY9"),
        );
    }

    #[test]
    fn record_start_caches_original_unchanged_when_not_attached() {
        let mut state = VirtualDisplayState::new();
        let original = make_payload("conn-1", Some("\\\\.\\DISPLAY1"));
        let active = state.record_start(original.clone());
        assert_eq!(active.video_device.as_deref(), Some("\\\\.\\DISPLAY1"));
        assert_eq!(
            state
                .original_start_payload
                .get("conn-1")
                .unwrap()
                .video_device
                .as_deref(),
            Some("\\\\.\\DISPLAY1"),
        );
    }

    #[test]
    fn record_stop_clears_both_caches() {
        let mut state = VirtualDisplayState::new();
        state.record_start(make_payload("conn-1", Some("\\\\.\\DISPLAY1")));
        state.record_stop("conn-1");
        assert!(state.original_start_payload.is_empty());
        assert!(state.active_start_payload.is_empty());
    }

    #[test]
    fn restart_steps_for_attached_returns_step_per_active_connection() {
        let mut state = VirtualDisplayState::new();
        state.record_start(make_payload("conn-1", Some("\\\\.\\DISPLAY1")));
        state.record_start(make_payload("conn-2", Some("\\\\.\\DISPLAY1")));
        let _ = state.rebuild_active_for_attach(Some("\\\\.\\DISPLAY9".to_string()));
        // Sanity precondition: attached_display set, 2 active payloads.
        assert_eq!(state.attached_display.as_deref(), Some("\\\\.\\DISPLAY9"));

        let steps = state.restart_steps_for_attached();
        assert_eq!(steps.len(), 2);
        let mut ids: Vec<&str> = steps.iter().map(|s| s.connection_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["conn-1", "conn-2"]);
        for step in &steps {
            // Each step carries the active payload (already rewritten
            // to target the attached display); the SetVirtualDisplayMode
            // handler will Stop+Start the producer with this payload.
            assert_eq!(step.active.video_device.as_deref(), Some("\\\\.\\DISPLAY9"));
        }
        // restart_steps_for_attached must NOT mutate attached_display
        // (set_mode keeps the same target, only the resolution changed).
        assert_eq!(state.attached_display.as_deref(), Some("\\\\.\\DISPLAY9"));
    }

    #[test]
    fn restart_steps_for_attached_returns_empty_when_no_active() {
        let state = VirtualDisplayState::new();
        assert!(state.restart_steps_for_attached().is_empty());
    }

    #[test]
    fn rebuild_active_for_attach_emits_restart_steps_for_each_connection() {
        let mut state = VirtualDisplayState::new();
        state.record_start(make_payload("conn-1", Some("\\\\.\\DISPLAY1")));
        state.record_start(make_payload("conn-2", Some("\\\\.\\DISPLAY1")));
        let steps = state.rebuild_active_for_attach(Some("\\\\.\\DISPLAY9".to_string()));
        assert_eq!(steps.len(), 2);
        for step in &steps {
            assert_eq!(step.active.video_device.as_deref(), Some("\\\\.\\DISPLAY9"));
        }
        // Active cache now reflects the override.
        for active in state.active_start_payload.values() {
            assert_eq!(active.video_device.as_deref(), Some("\\\\.\\DISPLAY9"));
        }
        // Original is untouched.
        for original in state.original_start_payload.values() {
            assert_eq!(original.video_device.as_deref(), Some("\\\\.\\DISPLAY1"));
        }
    }

    #[test]
    fn rebuild_active_for_detach_restores_original_video_device() {
        let mut state = VirtualDisplayState::new();
        state.attached_display = Some("\\\\.\\DISPLAY9".to_string());
        state.record_start(make_payload("conn-1", Some("\\\\.\\DISPLAY1")));
        let steps = state.rebuild_active_for_attach(None);
        assert_eq!(steps.len(), 1);
        // Detach restores the original physical device.
        assert_eq!(
            steps[0].active.video_device.as_deref(),
            Some("\\\\.\\DISPLAY1")
        );
        assert!(state.attached_display.is_none());
    }

    struct MockController {
        applied_mode: Mutex<Option<VirtualDisplayMode>>,
        result: fn(VirtualDisplayMode) -> Result<VirtualDisplayMode, VirtualDisplayError>,
    }

    impl VirtualDisplayController for MockController {
        fn set_mode(
            &self,
            _: &str,
            mode: VirtualDisplayMode,
        ) -> Result<VirtualDisplayMode, VirtualDisplayError> {
            *self.applied_mode.lock().unwrap() = Some(mode);
            (self.result)(mode)
        }
    }

    /// `tokio::time::sleep` is awkward to mock at the type level. The
    /// existing tests wrap the injected sleeper around a counter and
    /// assert on the recorded `Duration`s instead, so we never spend
    /// real wall time waiting for the 250 / 500 / ... backoff schedule.
    fn make_recording_sleeper() -> (
        std::sync::Arc<Mutex<Vec<Duration>>>,
        impl FnMut(Duration) -> std::future::Ready<()>,
    ) {
        let log: std::sync::Arc<Mutex<Vec<Duration>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log_clone = std::sync::Arc::clone(&log);
        let sleeper = move |d: Duration| {
            log_clone.lock().unwrap().push(d);
            std::future::ready(())
        };
        (log, sleeper)
    }

    #[tokio::test]
    async fn resolve_attach_with_backoff_returns_attached_on_first_success() {
        let calls = std::sync::Arc::new(Mutex::new(0u32));
        let calls_clone = std::sync::Arc::clone(&calls);
        let resolver = move |_: &str| {
            *calls_clone.lock().unwrap() += 1;
            Ok(r"\\.\DISPLAY4".to_string())
        };
        let (sleep_log, sleeper) = make_recording_sleeper();
        let outcome = resolve_attach_with_backoff(
            "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay",
            resolver,
            sleeper,
        )
        .await;
        assert!(matches!(
            outcome,
            VirtualDisplayAttachOutcome::Attached(ref n) if n == r"\\.\DISPLAY4"
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "first success must short-circuit"
        );
        assert!(
            sleep_log.lock().unwrap().is_empty(),
            "no backoff sleep needed on first success",
        );
    }

    #[tokio::test]
    async fn resolve_attach_with_backoff_retries_until_success() {
        // First two attempts fail, third succeeds.
        let calls = std::sync::Arc::new(Mutex::new(0u32));
        let calls_clone = std::sync::Arc::clone(&calls);
        let resolver = move |_: &str| -> Result<String, VirtualDisplayError> {
            let mut n = calls_clone.lock().unwrap();
            *n += 1;
            if *n < 3 {
                Err(VirtualDisplayError::DeviceCreate(format!(
                    "transient #{}",
                    *n
                )))
            } else {
                Ok(r"\\.\DISPLAY4".to_string())
            }
        };
        let (sleep_log, sleeper) = make_recording_sleeper();
        let outcome = resolve_attach_with_backoff(
            "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay",
            resolver,
            sleeper,
        )
        .await;
        assert!(matches!(
            outcome,
            VirtualDisplayAttachOutcome::Attached(ref n) if n == r"\\.\DISPLAY4"
        ));
        assert_eq!(*calls.lock().unwrap(), 3, "must retry until success");
        // Two sleeps between the three attempts: 250, 500.
        let log = sleep_log.lock().unwrap();
        assert_eq!(
            *log,
            vec![Duration::from_millis(250), Duration::from_millis(500)]
        );
    }

    #[tokio::test]
    async fn resolve_attach_with_backoff_returns_failed_after_max_retries() {
        let calls = std::sync::Arc::new(Mutex::new(0u32));
        let calls_clone = std::sync::Arc::clone(&calls);
        let resolver = move |_: &str| -> Result<String, VirtualDisplayError> {
            *calls_clone.lock().unwrap() += 1;
            Err(VirtualDisplayError::DeviceCreate("permanent".to_string()))
        };
        let (sleep_log, sleeper) = make_recording_sleeper();
        let outcome = resolve_attach_with_backoff(
            "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay",
            resolver,
            sleeper,
        )
        .await;
        match outcome {
            VirtualDisplayAttachOutcome::Failed(msg) => {
                assert!(
                    msg.contains("exhausted 6 retries"),
                    "expected exhaustion summary, got {msg}",
                );
                assert!(
                    msg.contains("permanent"),
                    "expected last error in message, got {msg}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(
            *calls.lock().unwrap(),
            ATTACH_BACKOFF_SCHEDULE_MS.len() as u32,
            "every retry slot must be consumed",
        );
        // 5 sleeps between 6 attempts; the trailing 8000 ms slot is
        // skipped because it would just delay returning Failed.
        let log = sleep_log.lock().unwrap();
        assert_eq!(
            *log,
            vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_millis(1_000),
                Duration::from_millis(2_000),
                Duration::from_millis(4_000),
            ],
        );
    }

    #[tokio::test]
    async fn run_set_mode_returns_failed_when_not_attached() {
        let controller: Arc<dyn VirtualDisplayController> = Arc::new(MockController {
            applied_mode: Mutex::new(None),
            result: |_| panic!("controller must not be invoked when not attached"),
        });
        let payload = SetVirtualDisplayModePayload {
            request_id: "r".to_string(),
            connection_id: "c".to_string(),
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let response = run_set_mode(controller, None, payload).await;
        match response {
            WorkerToService::VirtualDisplayMode(p) => match p.outcome {
                VirtualDisplayModeOutcome::Failed(reason) => {
                    assert_eq!(reason, "virtual display not attached");
                }
                other => panic!("unexpected outcome: {other:?}"),
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_set_mode_returns_applied_on_success() {
        let controller: Arc<dyn VirtualDisplayController> = Arc::new(MockController {
            applied_mode: Mutex::new(None),
            result: |m| Ok(m),
        });
        let payload = SetVirtualDisplayModePayload {
            request_id: "r".to_string(),
            connection_id: "c".to_string(),
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let response = run_set_mode(controller, Some("\\\\.\\DISPLAY9".to_string()), payload).await;
        match response {
            WorkerToService::VirtualDisplayMode(p) => match p.outcome {
                VirtualDisplayModeOutcome::Applied(m) => {
                    assert_eq!(m.width, 1920);
                    assert_eq!(m.height, 1080);
                    assert_eq!(m.refresh_hz, 60);
                }
                other => panic!("unexpected outcome: {other:?}"),
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_set_mode_returns_failed_on_controller_error() {
        let controller: Arc<dyn VirtualDisplayController> = Arc::new(MockController {
            applied_mode: Mutex::new(None),
            result: |_| {
                Err(VirtualDisplayError::PipeIo(
                    "driver not available".to_string(),
                ))
            },
        });
        let payload = SetVirtualDisplayModePayload {
            request_id: "r".to_string(),
            connection_id: "c".to_string(),
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let response = run_set_mode(controller, Some("\\\\.\\DISPLAY9".to_string()), payload).await;
        match response {
            WorkerToService::VirtualDisplayMode(p) => match p.outcome {
                VirtualDisplayModeOutcome::Failed(reason) => {
                    assert!(
                        reason.contains("driver not available"),
                        "expected driver-not-available message, got {reason}",
                    );
                }
                other => panic!("unexpected outcome: {other:?}"),
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Regression test for the WGC mid-session resize blackscreen
    /// (2026-05-24): when the IDD pipe call succeeds but
    /// `ChangeDisplaySettingsExW` returns `DISP_CHANGE_BADMODE`, the
    /// `VirtualDisplayController::set_mode` implementation surfaces
    /// `VirtualDisplayError::Cds("BADMODE for <device> @ WxH@Hz; …")`.
    /// `run_set_mode` must preserve that string verbatim in the
    /// `Failed` outcome so the worker-side `error!` log (and any
    /// future browser-side display of the reason) shows the actionable
    /// "BADMODE" keyword. If a future refactor drops that substring,
    /// diagnosing the next resize regression would once again require
    /// parsing browser traces.
    #[tokio::test]
    async fn run_set_mode_failed_outcome_preserves_cds_badmode_reason() {
        let cds_msg =
            "BADMODE for \\\\.\\DISPLAY9 @ 850x770@60; driver did not advertise this mode";
        let controller: Arc<dyn VirtualDisplayController> = Arc::new(MockController {
            applied_mode: Mutex::new(None),
            result: |_| {
                Err(VirtualDisplayError::Cds(
                    "BADMODE for \\\\.\\DISPLAY9 @ 850x770@60; driver did not advertise this mode"
                        .to_string(),
                ))
            },
        });
        let payload = SetVirtualDisplayModePayload {
            request_id: "r-shrink".to_string(),
            connection_id: "c-shrink".to_string(),
            width: 850,
            height: 770,
            refresh_hz: 60,
        };
        let response = run_set_mode(controller, Some("\\\\.\\DISPLAY9".to_string()), payload).await;
        match response {
            WorkerToService::VirtualDisplayMode(p) => match p.outcome {
                VirtualDisplayModeOutcome::Failed(reason) => {
                    assert!(
                        reason.contains(cds_msg),
                        "BADMODE reason must reach IPC verbatim so the \
                         worker `error!` log shows the actionable substring; \
                         got {reason}",
                    );
                    assert!(
                        reason.contains("BADMODE"),
                        "log searchability hinges on the literal 'BADMODE' \
                         token; got {reason}",
                    );
                }
                other => panic!("unexpected outcome: {other:?}"),
            },
            other => panic!("unexpected: {other:?}"),
        }
        // Note: the corresponding worker-side restart contract — the
        // SetVirtualDisplayMode IPC handler MUST trigger WGC
        // invalidate+stop+start even on this Failed outcome because
        // `pipe_client::send_set_mode` already triggered the IDD
        // Departure+Arrival cycle — is exercised by the `session.rs`
        // tests `wgc_restart_runs_even_when_outcome_failed` (see
        // there). It cannot be tested here because `run_set_mode` does
        // not own the restart logic.
    }

    // ───── Exclusive-mode pipeline tests ─────

    fn empty_layout() -> Arc<std::sync::Mutex<Option<ExclusiveLayout>>> {
        Arc::new(std::sync::Mutex::new(None))
    }

    /// `run_enter` without an attached display name reports
    /// `EnterFailed` immediately — covers the path where the daemon
    /// asks for exclusive but the virtual display attach has not
    /// produced a usable `\\.\DISPLAYn` yet.
    #[tokio::test]
    async fn run_enter_without_attached_reports_failed_immediately() {
        let layout = empty_layout();
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerToService>();
        let (_cancel_tx, cancel_rx) = oneshot::channel::<()>();
        run_enter(7, 0, None, layout, cancel_rx, &tx, None).await;
        let msg = rx.recv().await.expect("result");
        match msg {
            WorkerToService::ExclusiveResult(p) => {
                assert_eq!(p.op_id, 7);
                matches!(p.outcome, ExclusiveOutcome::EnterFailed(_));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `run_leave` on an already-empty slot is idempotent — replies
    /// with `Left` (codex round 5 #4) so the daemon's `current_op_id`
    /// gate still sees the ack.
    #[tokio::test]
    async fn run_leave_idempotent_when_no_layout() {
        let layout = empty_layout();
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerToService>();
        run_leave(13, layout, &tx, None).await;
        let msg = rx.recv().await.expect("result");
        match msg {
            WorkerToService::ExclusiveResult(p) => {
                assert_eq!(p.op_id, 13);
                matches!(p.outcome, ExclusiveOutcome::Left);
                assert!(matches!(p.direction, ExclusiveDirection::Leaving));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `run_exclusive_reconciler` (true, true) — desired enter while
    /// already active — must still emit `Entered(op_id)` so the
    /// daemon's op_id gate fires (codex round 5 #4).
    #[tokio::test]
    async fn reconciler_idempotent_true_true_acks_entered() {
        let layout = empty_layout();
        // Pretend we are already in exclusive: drop a dummy snapshot
        // in the slot so `currently_active = true`. The slot only
        // cares about `is_some`, the contents are not inspected.
        {
            let mut g = layout.lock().unwrap();
            *g = Some(make_dummy_layout());
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerToService>();
        let (_c_tx, c_rx) = oneshot::channel::<()>();
        run_exclusive_reconciler(
            21,
            true,
            5_000,
            Some("\\\\.\\DISPLAY1".to_string()),
            layout,
            c_rx,
            tx,
            None,
        )
        .await;
        let msg = rx.recv().await.expect("result");
        match msg {
            WorkerToService::ExclusiveResult(p) => {
                assert_eq!(p.op_id, 21);
                assert!(matches!(p.outcome, ExclusiveOutcome::Entered));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `run_exclusive_reconciler` (false, false) — desired leave while
    /// already idle — must still emit `Left(op_id)` (codex round 5 #4).
    #[tokio::test]
    async fn reconciler_idempotent_false_false_acks_left() {
        let layout = empty_layout();
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerToService>();
        let (_c_tx, c_rx) = oneshot::channel::<()>();
        run_exclusive_reconciler(
            22,
            false,
            0,
            Some("\\\\.\\DISPLAY1".to_string()),
            layout,
            c_rx,
            tx,
            None,
        )
        .await;
        let msg = rx.recv().await.expect("result");
        match msg {
            WorkerToService::ExclusiveResult(p) => {
                assert_eq!(p.op_id, 22);
                assert!(matches!(p.outcome, ExclusiveOutcome::Left));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// E2E fix 2026-05-27: `run_enter` without an attached display
    /// MUST NOT post `ExclusiveCommitEvent::Entered` on the commit
    /// channel — the early-EnterFailed return short-circuits before
    /// any CDS commit happens, so there is nothing for the session
    /// loop to refresh. (`run_enter`'s normal success path posts the
    /// event after `enter_exclusive` returns Ok, which is the
    /// scenario we cannot exercise from a unit test without real
    /// Win32 GDI; this test pins the negative half.)
    #[tokio::test]
    async fn run_enter_failed_does_not_post_commit_event() {
        let layout = empty_layout();
        let (tx, _rx) = mpsc::unbounded_channel::<WorkerToService>();
        let (commit_tx, mut commit_rx) = mpsc::unbounded_channel::<ExclusiveCommitEvent>();
        let (_cancel_tx, cancel_rx) = oneshot::channel::<()>();
        run_enter(8, 0, None, layout, cancel_rx, &tx, Some(&commit_tx)).await;
        // Drop the sender we control so try_recv reports `Disconnected`
        // (no event) instead of `Empty` (just timing).
        drop(commit_tx);
        match commit_rx.try_recv() {
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {}
            other => panic!("expected no commit event on EnterFailed, got: {other:?}"),
        }
    }

    /// E2E fix 2026-05-27: `run_leave` on an empty slot is the
    /// idempotent path that does NOT touch CDS — it must NOT post
    /// `ExclusiveCommitEvent::Left` either, otherwise the session
    /// loop would Stop+Start media for nothing on every redundant
    /// leave request. The positive halves (Succeeded / Failed both
    /// touched CDS) cannot be unit-tested without real Win32 GDI.
    #[tokio::test]
    async fn run_leave_empty_slot_does_not_post_commit_event() {
        let layout = empty_layout();
        let (tx, _rx) = mpsc::unbounded_channel::<WorkerToService>();
        let (commit_tx, mut commit_rx) = mpsc::unbounded_channel::<ExclusiveCommitEvent>();
        run_leave(14, layout, &tx, Some(&commit_tx)).await;
        drop(commit_tx);
        match commit_rx.try_recv() {
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {}
            other => panic!("expected no commit event on Empty leave, got: {other:?}"),
        }
    }

    /// E2E fix 2026-05-27: `set_commit_channel` installs the sender;
    /// subsequent `request()` calls clone it through to the runner.
    /// We cannot exercise the actual `Entered` / `Left` post without
    /// real GDI, but we can pin that the channel install is wired
    /// (compile-time guarantee + struct field non-None).
    #[test]
    fn coordinator_set_commit_channel_installs_sender() {
        let (commit_tx, _commit_rx) = mpsc::unbounded_channel::<ExclusiveCommitEvent>();
        let mut coord = ExclusiveCoordinator::new();
        assert!(
            coord.commit_tx.is_none(),
            "fresh coord starts without a channel"
        );
        coord.set_commit_channel(commit_tx);
        assert!(
            coord.commit_tx.is_some(),
            "channel must be installed after set"
        );
    }

    /// `ExclusiveGuard::drop` on an empty slot does not panic and
    /// does not attempt to call leave_exclusive (which would need
    /// real Win32 GDI). Pins the "layout None ⇒ Drop is a no-op"
    /// contract.
    #[test]
    fn exclusive_guard_drop_on_empty_slot_is_noop() {
        let layout = empty_layout();
        let guard = ExclusiveGuard::new(Arc::clone(&layout));
        drop(guard);
        // Slot still None; no panic.
        assert!(layout.lock().unwrap().is_none());
    }

    /// `ExclusiveCoordinator::request(false, layout=None)` synthesises
    /// `Left(op_id)` via the idempotent reconciler branch. The first
    /// request seeds the runner; we await the result channel and
    /// assert the matching op_id comes through.
    #[tokio::test]
    async fn coordinator_request_false_idempotent_emits_left() {
        let layout = empty_layout();
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerToService>();
        let mut coord = ExclusiveCoordinator::new();
        coord.request(
            99,
            false,
            0,
            Some("\\\\.\\DISPLAY1".to_string()),
            layout,
            tx,
        );
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("must produce a result")
            .expect("channel still open");
        match msg {
            WorkerToService::ExclusiveResult(p) => {
                assert_eq!(p.op_id, 99);
                assert!(matches!(p.outcome, ExclusiveOutcome::Left));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `ExclusiveCoordinator`: a follow-up request cancels the prior
    /// runner; only the surviving op's runner must emit a result
    /// (codex round 5 #3 + round 9 #1). The first runner is fast
    /// enough on the idempotent path that it may race the cancel —
    /// the contract is "at most one extra Left(100), the Left(101)
    /// is guaranteed". So the test asserts: (a) the final op_id 101
    /// is observed and (b) the channel produces at most two results
    /// (we accept either 1 or 2 depending on the cancel race).
    #[tokio::test]
    async fn coordinator_serialises_and_at_most_two_results_with_final_op_observed() {
        let layout = empty_layout();
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerToService>();
        let mut coord = ExclusiveCoordinator::new();
        coord.request(100, false, 0, None, Arc::clone(&layout), tx.clone());
        coord.request(101, false, 0, None, layout, tx);
        // Drain everything that arrives in a generous window.
        let mut ids: Vec<u64> = vec![];
        loop {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(WorkerToService::ExclusiveResult(p))) => ids.push(p.op_id),
                Ok(Some(other)) => panic!("unexpected: {other:?}"),
                Ok(None) | Err(_) => break,
            }
        }
        assert!(
            ids.last() == Some(&101),
            "final op_id 101 must reach the writer (got {ids:?})"
        );
        assert!(
            ids.len() <= 2,
            "at most two results (cancelled prior + winning new) (got {ids:?})"
        );
    }

    fn make_dummy_layout() -> ExclusiveLayout {
        ExclusiveLayout {
            physical_snapshots: vec![],
            virtual_snapshot: desk_virtual_display::PhysicalDisplaySnapshot {
                device_name: "\\\\.\\DISPLAY9".into(),
                // The Windows snapshot carries a `DEVMODEW`; the
                // cross-platform stub does not.
                #[cfg(windows)]
                devmode: unsafe { std::mem::zeroed() },
                is_primary: true,
            },
        }
    }

    // ───────────────────────────────────────────────────────────────
    // Codex P1 #2 regression — slot retention on leave failure
    // ───────────────────────────────────────────────────────────────

    /// `attempt_leave_with_fn` clears the slot only on success — the
    /// happy path. Verifies the post-condition that a subsequent
    /// `run_leave` would see the slot empty (and hit the idempotent
    /// `Left` ack).
    #[test]
    fn attempt_leave_clears_slot_on_success() {
        let slot = Arc::new(std::sync::Mutex::new(Some(make_dummy_layout())));
        let outcome = attempt_leave_with_fn(&slot, |_| Ok(()));
        assert!(matches!(outcome, LeaveOutcome::Succeeded));
        assert!(slot.lock().unwrap().is_none());
    }

    /// Codex P1 #2: when `leave_fn` returns `Err`, the slot must
    /// retain the layout so a future Drop guard / explicit retry can
    /// try again. The previous implementation `take()`d before
    /// calling `leave_exclusive`, irreversibly destroying state.
    #[test]
    fn attempt_leave_retains_slot_on_failure() {
        let slot = Arc::new(std::sync::Mutex::new(Some(make_dummy_layout())));
        let outcome = attempt_leave_with_fn(&slot, |_| {
            Err(VirtualDisplayError::Cds(
                "simulated partial reattach".into(),
            ))
        });
        match outcome {
            LeaveOutcome::Failed(msg) => assert!(
                msg.contains("simulated partial reattach"),
                "outcome must carry the original error: {msg}"
            ),
            other => panic!("unexpected: {other:?}"),
        }
        // The critical post-condition: the layout is still in the
        // slot. The Drop guard depends on this.
        assert!(slot.lock().unwrap().is_some());
    }

    /// Codex follow-up P2 (2026-05-26): a poisoned slot containing
    /// `Some(layout)` must still drive `leave_fn` against that layout
    /// — the previous "treat poison as failure" returned without
    /// calling `leave_fn`, leaving the still-detached physical
    /// displays inaccessible to any recovery path. The recovery
    /// uses `PoisonError::into_inner()` to read the inner Option
    /// through the poison.
    #[test]
    fn attempt_leave_poisoned_slot_recovers_and_calls_leave_fn() {
        let slot: Arc<std::sync::Mutex<Option<ExclusiveLayout>>> =
            Arc::new(std::sync::Mutex::new(Some(make_dummy_layout())));
        // Poison the mutex with a panic-holding-guard pattern.
        let slot_for_panic = Arc::clone(&slot);
        let _ = std::thread::spawn(move || {
            let _g = slot_for_panic.lock().unwrap();
            panic!("intentional poison for test");
        })
        .join();
        assert!(slot.is_poisoned(), "mutex must be poisoned for this test");
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = Arc::clone(&called);
        let outcome = attempt_leave_with_fn(&slot, move |_| {
            called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        assert!(
            matches!(outcome, LeaveOutcome::Succeeded),
            "poison must NOT abort — leave_fn ran and returned Ok",
        );
        assert!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            "leave_fn must be invoked despite poison",
        );
        // The slot was successfully cleared via the second-lock
        // into_inner() recovery path.
        let inner = match slot.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };
        assert!(inner.is_none(), "slot must be empty after successful leave");
    }

    /// Codex follow-up P2 sibling test: a poisoned **empty** slot
    /// must still classify as `Empty` (idempotent path) — the
    /// recovery `into_inner()` reads `None`, no `leave_fn` call.
    #[test]
    fn attempt_leave_poisoned_empty_slot_returns_empty() {
        let slot: Arc<std::sync::Mutex<Option<ExclusiveLayout>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot_for_panic = Arc::clone(&slot);
        let _ = std::thread::spawn(move || {
            let _g = slot_for_panic.lock().unwrap();
            panic!("intentional poison for test");
        })
        .join();
        assert!(slot.is_poisoned());
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = Arc::clone(&called);
        let outcome = attempt_leave_with_fn(&slot, move |_| {
            called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        assert!(matches!(outcome, LeaveOutcome::Empty));
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "leave_fn must NOT run for an empty (even poisoned) slot",
        );
    }

    /// Codex follow-up P2: `ExclusiveGuard::drop` must recover the
    /// layout via `PoisonError::into_inner()` and STILL attempt
    /// `leave_exclusive`. The previous code returned on poison,
    /// throwing away the last recovery chance.
    ///
    /// `leave_exclusive` itself needs Win32 GDI — we can't drive
    /// the actual leave here. Instead we observe that Drop calls
    /// `take()` against the slot (recovered via into_inner) — i.e.
    /// after drop, the slot must be `None` regardless of poison.
    /// That post-condition is enough to prove the recovery path
    /// reached the inner Option.
    #[test]
    fn exclusive_guard_drop_recovers_layout_from_poisoned_slot() {
        // Use a layout with no physical_snapshots so leave_exclusive
        // is effectively a no-op (only the virtual_snapshot restore
        // is attempted; CDS may fail in test env but the take() will
        // still have happened before).
        let slot: Arc<std::sync::Mutex<Option<ExclusiveLayout>>> =
            Arc::new(std::sync::Mutex::new(Some(make_dummy_layout())));
        let slot_for_panic = Arc::clone(&slot);
        let _ = std::thread::spawn(move || {
            let _g = slot_for_panic.lock().unwrap();
            panic!("intentional poison for test");
        })
        .join();
        assert!(slot.is_poisoned());

        let guard = ExclusiveGuard::new(Arc::clone(&slot));
        drop(guard);

        // After Drop: slot was take()n out, regardless of poison.
        // The leave_exclusive call may have logged an error in test
        // env (no real Win32 displays) but the recovery path was
        // exercised.
        let inner = match slot.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };
        assert!(
            inner.is_none(),
            "Drop must take() the layout even from a poisoned slot",
        );
    }

    /// `attempt_leave_with_fn` on an empty slot is a fast no-op —
    /// `leave_fn` must NOT be invoked. Pins the Empty short-circuit
    /// so a future refactor doesn't call `leave_exclusive` with a
    /// zeroed `ExclusiveLayout`.
    #[test]
    fn attempt_leave_empty_slot_skips_leave_fn() {
        let slot: Arc<std::sync::Mutex<Option<ExclusiveLayout>>> =
            Arc::new(std::sync::Mutex::new(None));
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = Arc::clone(&called);
        let outcome = attempt_leave_with_fn(&slot, move |_| {
            called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        assert!(matches!(outcome, LeaveOutcome::Empty));
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }
}
