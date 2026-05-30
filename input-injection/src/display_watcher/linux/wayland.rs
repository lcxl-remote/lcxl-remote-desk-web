//! Wayland display-change watcher implementation.
//!
//! Owns its own Wayland connection and listens on the core `wl_registry`
//! / `wl_output` protocol — supported by every compositor (GNOME, KDE,
//! wlroots) — for output add/remove and per-output geometry / mode /
//! scale changes. Each reconfiguration is forwarded as a single
//! [`DisplayChangeEvent`], mirroring the X11 RandR and Windows
//! `WM_DISPLAYCHANGE` paths so the worker can refresh per-connection
//! mouse geometry without tearing down the connection.
//!
//! A burst of `wl_output` events (a mode change emits Geometry + Mode +
//! Scale + Done together) is coalesced into one event via
//! [`EventCoalescer`]: handlers only set a dirty flag, and the poll loop
//! emits at most one event per dispatch round.
//!
//! ## Shutdown
//!
//! The socket fd is polled with a short timeout (`libc::poll`) rather
//! than blocking, so the thread observes the stop flag and exits
//! promptly when the watcher is dropped.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

use super::super::error::DisplayWatcherError;
use super::{DisplayChangeEvent, POLL_INTERVAL};

/// Highest `wl_output` version this watcher binds. Version 4 adds the
/// `name` / `description` events; we don't depend on them, but binding
/// the newest version the server offers keeps event delivery complete.
const OUR_MAX_WL_OUTPUT_VERSION: u32 = 4;

/// `wl_output.release` is a request introduced in `wl_output` version 3.
/// Sending it to an older-version object is a protocol error, so the
/// destructor is gated on the bound version.
fn wl_output_release_supported(version: u32) -> bool {
    version >= 3
}

/// A bound `wl_output` plus the version it was bound at (needed to decide
/// whether `release` may be sent on removal — see
/// [`wl_output_release_supported`]).
struct BoundOutput {
    output: WlOutput,
    version: u32,
}

/// Coalesces a burst of change notifications into a single monotonic
/// event. Handlers call [`mark_dirty`](Self::mark_dirty); the poll loop
/// calls [`take_event`](Self::take_event) once per round.
#[derive(Default)]
struct EventCoalescer {
    seq: u64,
    dirty: bool,
}

impl EventCoalescer {
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Clears the startup-enumeration dirtiness so the initial output
    /// roundtrip is not reported as a change.
    fn reset(&mut self) {
        self.dirty = false;
    }

    /// Returns one event (advancing `seq`) iff something changed since
    /// the last call. `seq` is monotonic across the watcher's lifetime.
    fn take_event(&mut self) -> Option<DisplayChangeEvent> {
        if self.dirty {
            self.dirty = false;
            self.seq += 1;
            Some(DisplayChangeEvent { seq: self.seq })
        } else {
            None
        }
    }
}

/// Decision for a `GlobalRemove` of a registry name, derived from
/// whether the name was tracked and at what version. Pure so the
/// add/remove/double-remove bookkeeping is unit-testable without a live
/// `wl_output` proxy.
#[derive(Debug, PartialEq, Eq)]
enum ForgetOutcome {
    /// The name was not a tracked output (e.g. a non-output global, or a
    /// duplicate removal). No-op.
    NotTracked,
    /// The tracked output was removed; `release` indicates whether
    /// `wl_output.release` may be sent for the bound version.
    Forgotten { release: bool },
}

fn forget_decision(removed_version: Option<u32>) -> ForgetOutcome {
    match removed_version {
        None => ForgetOutcome::NotTracked,
        Some(version) => ForgetOutcome::Forgotten {
            release: wl_output_release_supported(version),
        },
    }
}

/// Watcher dispatch state: the set of bound outputs and the change
/// coalescer.
struct OutputWatchState {
    outputs: HashMap<u32, BoundOutput>,
    coalescer: EventCoalescer,
}

impl OutputWatchState {
    fn new() -> Self {
        Self {
            outputs: HashMap::new(),
            coalescer: EventCoalescer::default(),
        }
    }
}

impl Dispatch<WlRegistry, ()> for OutputWatchState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => {
                let bind_ver = version.min(OUR_MAX_WL_OUTPUT_VERSION);
                // Bind with the registry name as user data so the output's
                // own events are routed to the `Dispatch<WlOutput, u32>`
                // impl below.
                let output: WlOutput = registry.bind(name, bind_ver, qh, name);
                state.outputs.insert(
                    name,
                    BoundOutput {
                        output,
                        version: bind_ver,
                    },
                );
                state.coalescer.mark_dirty();
            }
            wl_registry::Event::GlobalRemove { name } => {
                let removed = state.outputs.remove(&name);
                match forget_decision(removed.as_ref().map(|b| b.version)) {
                    ForgetOutcome::NotTracked => {}
                    ForgetOutcome::Forgotten { release } => {
                        if release && let Some(b) = removed {
                            b.output.release();
                        }
                        state.coalescer.mark_dirty();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for OutputWatchState {
    fn event(
        state: &mut Self,
        _output: &WlOutput,
        _event: wayland_client::protocol::wl_output::Event,
        _name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Any output event (Geometry / Mode / Scale / Name / Description
        // / Done) signals a possible geometry change; coalesce them.
        state.coalescer.mark_dirty();
    }
}

/// Outcome of waiting on the Wayland socket fd.
enum PollOutcome {
    /// Data is available, or the socket signalled an error / hangup.
    /// Both route into `guard.read()`, which drains pending events and,
    /// on a broken socket, surfaces the real error so the watcher exits
    /// instead of spinning silently.
    Readable,
    /// Timed out or was interrupted — the caller re-checks the stop flag
    /// and loops.
    Idle,
}

fn poll_readable(fd: std::os::fd::RawFd, timeout: std::time::Duration) -> PollOutcome {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
    // POLLHUP / POLLERR / POLLNVAL are reported in `revents` regardless of
    // `events`; treat them as readable so `read()` propagates the failure
    // (a dead compositor socket must stop the watcher, not spin).
    let signalled = libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    if ret > 0 && (pfd.revents & signalled) != 0 {
        PollOutcome::Readable
    } else {
        PollOutcome::Idle
    }
}

fn run_event_loop(
    conn: &Connection,
    queue: &mut EventQueue<OutputWatchState>,
    state: &mut OutputWatchState,
    stop: &Arc<AtomicBool>,
    tx: &mpsc::UnboundedSender<DisplayChangeEvent>,
) -> Result<(), DisplayWatcherError> {
    while !stop.load(Ordering::Acquire) {
        conn.flush()?;

        let Some(guard) = conn.prepare_read() else {
            // Events are already buffered without touching the socket.
            queue.dispatch_pending(state)?;
            if let Some(ev) = state.coalescer.take_event()
                && tx.send(ev).is_err()
            {
                return Ok(());
            }
            continue;
        };

        let readable = matches!(
            poll_readable(guard.connection_fd().as_raw_fd(), POLL_INTERVAL),
            PollOutcome::Readable
        );
        if readable {
            guard.read()?;
        } else {
            // Timeout / interrupt: release the read intent and re-check
            // the stop flag.
            drop(guard);
        }

        queue.dispatch_pending(state)?;
        if let Some(ev) = state.coalescer.take_event()
            && tx.send(ev).is_err()
        {
            return Ok(());
        }
    }
    Ok(())
}

pub(super) fn runner(
    tx: mpsc::UnboundedSender<DisplayChangeEvent>,
    stop: Arc<AtomicBool>,
    init_tx: std::sync::mpsc::Sender<Result<(), DisplayWatcherError>>,
) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(e.into()));
            return;
        }
    };
    let mut queue = conn.new_event_queue::<OutputWatchState>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = OutputWatchState::new();
    // Bind the outputs that already exist.
    if let Err(e) = queue.roundtrip(&mut state) {
        let _ = init_tx.send(Err(e.into()));
        return;
    }
    // The startup enumeration is the baseline, not a change.
    state.coalescer.reset();

    if init_tx.send(Ok(())).is_err() {
        return;
    }

    if let Err(e) = run_event_loop(&conn, &mut queue, &mut state, &stop, &tx) {
        log::warn!("display-watcher: Wayland event loop stopped: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalescer_emits_one_event_per_dirty_round() {
        let mut c = EventCoalescer::default();
        assert!(c.take_event().is_none(), "clean state yields nothing");
        c.mark_dirty();
        assert_eq!(c.take_event().map(|e| e.seq), Some(1));
        assert!(c.take_event().is_none(), "consumed — nothing left");
    }

    #[test]
    fn coalescer_collapses_a_burst_into_one_event() {
        let mut c = EventCoalescer::default();
        c.mark_dirty();
        c.mark_dirty();
        c.mark_dirty();
        assert_eq!(c.take_event().map(|e| e.seq), Some(1), "burst -> one event");
        assert!(c.take_event().is_none());
    }

    #[test]
    fn coalescer_seq_is_monotonic_across_rounds() {
        let mut c = EventCoalescer::default();
        c.mark_dirty();
        assert_eq!(c.take_event().map(|e| e.seq), Some(1));
        c.mark_dirty();
        assert_eq!(c.take_event().map(|e| e.seq), Some(2));
    }

    #[test]
    fn reset_drops_startup_dirtiness() {
        let mut c = EventCoalescer::default();
        c.mark_dirty();
        c.reset();
        assert!(
            c.take_event().is_none(),
            "startup enumeration is not a change"
        );
    }

    #[test]
    fn release_is_gated_on_version_three() {
        assert!(!wl_output_release_supported(2));
        assert!(wl_output_release_supported(3));
        assert!(wl_output_release_supported(4));
    }

    #[test]
    fn forget_decision_models_add_remove_double_remove() {
        // Mirrors the registry bookkeeping over a name->version map so the
        // add / remove / double-remove path is covered without a live
        // proxy.
        let mut versions: HashMap<u32, u32> = HashMap::new();
        versions.insert(10, 4); // bound output, version 4
        versions.insert(11, 2); // bound output, older version

        // First removal of a tracked v4 output: forgotten, release ok.
        assert_eq!(
            forget_decision(versions.remove(&10)),
            ForgetOutcome::Forgotten { release: true }
        );
        // Double removal of the same name: not tracked anymore (no-op).
        assert_eq!(
            forget_decision(versions.remove(&10)),
            ForgetOutcome::NotTracked
        );
        // Removal of a v2 output: forgotten but release must not be sent.
        assert_eq!(
            forget_decision(versions.remove(&11)),
            ForgetOutcome::Forgotten { release: false }
        );
        // Unknown name (e.g. a non-output global): not tracked.
        assert_eq!(
            forget_decision(versions.remove(&99)),
            ForgetOutcome::NotTracked
        );
    }
}
