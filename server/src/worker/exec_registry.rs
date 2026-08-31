//! The worker's handles on the commands it is currently running.
//!
//! A cancel arrives as a message on the IPC loop, long after the execution it
//! names was handed to a task of its own. The registry is what connects the two:
//! it maps a generation to the stop switch of the execution running under it.
//!
//! # Only the living are here
//!
//! An entry exists for exactly as long as its command is running. This is
//! deliberately *not* a record of what ran — the durable ledger is that, and it
//! outlives the process. Asking the registry about a finished execution correctly
//! finds nothing, which is why a cancel that finds no entry is answered from the
//! ledger rather than treated as an error: the command was very likely just
//! finishing as the cancel arrived.
//!
//! # Registration precedes the run
//!
//! The entry goes in before the execution starts, not once it is up. A cancel
//! that arrives during startup would otherwise find nothing and be reported as
//! "unknown", and the command it meant to stop would run on unimpeded.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::worker::exec::ExecCancel;
use desk_agent_protocol::exec_pty::{PtyCloseReason, PtyInputFrame, PtyResizeFrame};
use portable_pty::{MasterPty, PtySize};

/// The stop switches of every command this worker currently has running.
#[derive(Clone, Default)]
pub struct ExecRegistry {
    running: Arc<Mutex<HashMap<String, ExecCancel>>>,
    pty: Arc<Mutex<HashMap<String, PtyControl>>>,
}

struct PtyControl {
    stream_id: String,
    session_target_id: String,
    registration_generation: u64,
    worker_incarnation: u64,
    next_sequence: u64,
    input_frames: u64,
    input_bytes: u64,
    stop_reason: Option<PtyCloseReason>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PtyInputStats {
    pub frames: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyControlFailure {
    pub reason: PtyCloseReason,
    pub message: String,
}

#[derive(Debug)]
pub enum PtyControlCommand {
    Input(PtyInputFrame),
    Resize(PtyResizeFrame),
}

pub const PTY_CONTROL_QUEUE_CAP: usize = 32;

impl PtyControlFailure {
    fn new(reason: PtyCloseReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PtyControlFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Removes its entry when dropped, so an execution cannot be left registered by
/// any exit path — including a panic in the command's own task.
pub struct ExecRegistration {
    registry: ExecRegistry,
    generation: String,
}

impl Drop for ExecRegistration {
    fn drop(&mut self) {
        self.registry
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.generation);
        self.registry
            .pty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.generation);
    }
}

impl ExecRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// One ordered blocking lane per worker. PTY writes and ioctls may block and
    /// must not run on the async IPC loop, while spawning one task per frame
    /// would reorder frames. The bounded queue also gives input floods a hard
    /// memory ceiling.
    pub fn spawn_pty_control_lane(
        &self,
    ) -> (
        tokio::sync::mpsc::Sender<PtyControlCommand>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(PTY_CONTROL_QUEUE_CAP);
        let registry = self.clone();
        let task = tokio::task::spawn_blocking(move || {
            while let Some(command) = rx.blocking_recv() {
                let (generation, stream_id, sequence, input_bytes, result) = match command {
                    PtyControlCommand::Input(frame) => {
                        let generation = frame.execution_generation.clone();
                        let stream_id = frame.stream_id.clone();
                        let sequence = frame.sequence;
                        let input_bytes = frame.data.len();
                        let result = registry.write_pty_input(frame);
                        (generation, stream_id, sequence, input_bytes, result)
                    }
                    PtyControlCommand::Resize(frame) => {
                        let generation = frame.execution_generation.clone();
                        let stream_id = frame.stream_id.clone();
                        let sequence = frame.sequence;
                        let result = registry.resize_pty(frame);
                        (generation, stream_id, sequence, 0, result)
                    }
                };
                if let Err(failure) = result {
                    log::warn!(
                        "PTY control frame rejected generation={} stream={} sequence={} input_bytes={} reason={:?} error={}",
                        generation,
                        stream_id,
                        sequence,
                        input_bytes,
                        failure.reason,
                        failure
                    );
                    registry.stop_pty(&generation, failure.reason);
                }
            }
        });
        (tx, task)
    }

    /// Register `generation` as running and hand back its stop switch.
    ///
    /// The registration must be held for as long as the execution runs; dropping
    /// it deregisters. Re-registering a generation replaces the previous switch,
    /// which cannot happen in practice — the ledger refuses a second dispatch of
    /// one generation before it ever reaches the worker.
    pub fn register(&self, generation: &str) -> (ExecCancel, ExecRegistration) {
        let cancel = ExecCancel::new();
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(generation.to_string(), cancel.clone());
        (
            cancel,
            ExecRegistration {
                registry: self.clone(),
                generation: generation.to_string(),
            },
        )
    }

    /// Stop the execution running under `generation`.
    ///
    /// Reports whether there was one to stop. `false` is not an error: the
    /// execution may have finished microseconds earlier, and the caller answers
    /// from the ledger, which knows what the registry has already forgotten.
    pub fn cancel(&self, generation: &str) -> bool {
        let entry = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(generation)
            .cloned();
        match entry {
            Some(cancel) => {
                cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub fn attach_pty(
        &self,
        generation: &str,
        stream_id: String,
        session_target_id: String,
        registration_generation: u64,
        worker_incarnation: u64,
        writer: Box<dyn Write + Send>,
        master: Box<dyn MasterPty + Send>,
    ) -> Result<(), String> {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if !running.contains_key(generation) {
            return Err("PTY execution is no longer registered".to_string());
        }
        let mut pty = self.pty.lock().unwrap_or_else(|e| e.into_inner());
        if pty.contains_key(generation) {
            return Err("PTY execution already has a live stream".to_string());
        }
        pty.insert(
            generation.to_string(),
            PtyControl {
                stream_id,
                session_target_id,
                registration_generation,
                worker_incarnation,
                next_sequence: 0,
                input_frames: 0,
                input_bytes: 0,
                stop_reason: None,
                writer,
                master,
            },
        );
        Ok(())
    }

    /// Write one opaque frame. Sequence advances exactly once and any mismatch
    /// is terminal: callers cancel the whole execution rather than retry bytes.
    pub fn write_pty_input(&self, frame: PtyInputFrame) -> Result<(), PtyControlFailure> {
        frame.validate().map_err(|message| {
            PtyControlFailure::new(PtyCloseReason::SequenceViolation, message)
        })?;
        let mut pty = self.pty.lock().unwrap_or_else(|e| e.into_inner());
        let control = pty.get_mut(&frame.execution_generation).ok_or_else(|| {
            PtyControlFailure::new(PtyCloseReason::SessionStale, "PTY execution is not live")
        })?;
        validate_frame_binding(
            control,
            &frame.stream_id,
            &frame.session_target_id,
            frame.registration_generation,
            frame.worker_incarnation,
            frame.sequence,
        )?;
        control.next_sequence = control.next_sequence.checked_add(1).ok_or_else(|| {
            PtyControlFailure::new(
                PtyCloseReason::SequenceViolation,
                "PTY input sequence exhausted",
            )
        })?;
        control
            .writer
            .write_all(&frame.data)
            .and_then(|_| control.writer.flush())
            .map_err(|error| {
                PtyControlFailure::new(
                    PtyCloseReason::InternalError,
                    format!("PTY input write failed: {error}"),
                )
            })?;
        control.input_frames = control.input_frames.saturating_add(1);
        control.input_bytes = control
            .input_bytes
            .saturating_add(frame.data.len().min(u64::MAX as usize) as u64);
        Ok(())
    }

    /// Drop the master-side writer and resize handle before waiting for the
    /// output reader to observe EOF. Returning only counters keeps opaque input
    /// bytes out of every completion/error path.
    pub fn detach_pty(&self, generation: &str) -> PtyInputStats {
        self.pty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(generation)
            .map_or_else(PtyInputStats::default, |control| PtyInputStats {
                frames: control.input_frames,
                bytes: control.input_bytes,
            })
    }

    pub fn resize_pty(&self, frame: PtyResizeFrame) -> Result<(), PtyControlFailure> {
        frame.validate().map_err(|message| {
            PtyControlFailure::new(PtyCloseReason::SequenceViolation, message)
        })?;
        let mut pty = self.pty.lock().unwrap_or_else(|e| e.into_inner());
        let control = pty.get_mut(&frame.execution_generation).ok_or_else(|| {
            PtyControlFailure::new(PtyCloseReason::SessionStale, "PTY execution is not live")
        })?;
        validate_frame_binding(
            control,
            &frame.stream_id,
            &frame.session_target_id,
            frame.registration_generation,
            frame.worker_incarnation,
            frame.sequence,
        )?;
        control.next_sequence = control.next_sequence.checked_add(1).ok_or_else(|| {
            PtyControlFailure::new(
                PtyCloseReason::SequenceViolation,
                "PTY input sequence exhausted",
            )
        })?;
        control
            .master
            .resize(PtySize {
                rows: frame.rows,
                cols: frame.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| {
                PtyControlFailure::new(
                    PtyCloseReason::InternalError,
                    format!("PTY resize failed: {error}"),
                )
            })
    }

    /// Record the terminal reason before notifying the executor. The first
    /// reason wins, so a later generic disconnect cannot overwrite a precise
    /// sequence/session failure already observed by the ordered control lane.
    pub fn stop_pty(&self, generation: &str, reason: PtyCloseReason) -> bool {
        if let Some(control) = self
            .pty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(generation)
        {
            control.stop_reason.get_or_insert(reason);
        }
        self.cancel(generation)
    }

    /// Stop only when every volatile carrier fence still names this exact PTY.
    /// A stale teardown is intentionally a no-op even when its generation was
    /// accidentally reused by a later worker incarnation.
    pub fn stop_pty_bound(
        &self,
        generation: &str,
        stream_id: &str,
        session_target_id: &str,
        registration_generation: u64,
        worker_incarnation: u64,
        reason: PtyCloseReason,
    ) -> bool {
        let matches = {
            let mut pty = self.pty.lock().unwrap_or_else(|e| e.into_inner());
            let Some(control) = pty.get_mut(generation) else {
                return false;
            };
            if control.stream_id != stream_id
                || control.session_target_id != session_target_id
                || control.registration_generation != registration_generation
                || control.worker_incarnation != worker_incarnation
            {
                return false;
            }
            control.stop_reason.get_or_insert(reason);
            true
        };
        matches && self.cancel(generation)
    }

    pub fn pty_stop_reason(&self, generation: &str) -> Option<PtyCloseReason> {
        self.pty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(generation)
            .and_then(|control| control.stop_reason)
    }

    /// How many commands are running right now.
    pub fn running(&self) -> usize {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn cancel_all(&self) -> u32 {
        let entries = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for cancel in &entries {
            cancel.cancel();
        }
        entries.len().min(u32::MAX as usize) as u32
    }
}

fn validate_frame_binding(
    control: &PtyControl,
    stream_id: &str,
    session_target_id: &str,
    registration_generation: u64,
    worker_incarnation: u64,
    sequence: u64,
) -> Result<(), PtyControlFailure> {
    if control.stream_id != stream_id {
        return Err(PtyControlFailure::new(
            PtyCloseReason::SessionStale,
            "PTY stream binding is stale",
        ));
    }
    if control.session_target_id != session_target_id
        || control.registration_generation != registration_generation
        || control.worker_incarnation != worker_incarnation
    {
        return Err(PtyControlFailure::new(
            PtyCloseReason::SessionStale,
            "PTY session binding is stale",
        ));
    }
    if control.next_sequence != sequence {
        return Err(PtyControlFailure::new(
            PtyCloseReason::SequenceViolation,
            format!(
                "PTY input sequence violation: expected {}, received {}",
                control.next_sequence, sequence
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registered_execution_can_be_stopped_by_generation() {
        let registry = ExecRegistry::new();
        let (cancel, _guard) = registry.register("gen-1");
        let watcher = cancel.subscribe();
        assert!(!*watcher.borrow());

        assert!(registry.cancel("gen-1"));
        assert!(*watcher.borrow(), "the execution was not asked to stop");
    }

    /// A cancel naming an execution this worker is not running is answered "no",
    /// not an error — the caller falls back to the ledger, which remembers.
    #[test]
    fn cancelling_an_unknown_generation_reports_nothing_to_stop() {
        let registry = ExecRegistry::new();
        assert!(!registry.cancel("never-registered"));
    }

    /// Deregistration happens on drop, so no exit path can leave a finished
    /// execution registered and have a later cancel appear to succeed.
    #[test]
    fn an_execution_deregisters_itself_however_it_ends() {
        let registry = ExecRegistry::new();
        {
            let (_cancel, _guard) = registry.register("gen-1");
            assert_eq!(registry.running(), 1);
        }
        assert_eq!(registry.running(), 0);
        assert!(!registry.cancel("gen-1"));
    }

    /// One generation's cancel does not touch another's.
    #[test]
    fn stopping_one_execution_leaves_the_others_running() {
        let registry = ExecRegistry::new();
        let (a, _ga) = registry.register("gen-a");
        let (b, _gb) = registry.register("gen-b");
        registry.cancel("gen-a");
        assert!(*a.subscribe().borrow());
        assert!(
            !*b.subscribe().borrow(),
            "an unrelated execution was stopped"
        );
    }

    #[test]
    fn cancel_all_stops_every_running_execution() {
        let registry = ExecRegistry::new();
        let (first, _first_guard) = registry.register("gen-a");
        let (second, _second_guard) = registry.register("gen-b");

        assert_eq!(registry.cancel_all(), 2);
        assert!(*first.subscribe().borrow());
        assert!(*second.subscribe().borrow());
    }
}
