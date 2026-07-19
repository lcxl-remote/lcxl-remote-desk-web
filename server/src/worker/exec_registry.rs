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
use std::sync::{Arc, Mutex};

use crate::worker::exec::ExecCancel;

/// The stop switches of every command this worker currently has running.
#[derive(Clone, Default)]
pub struct ExecRegistry {
    running: Arc<Mutex<HashMap<String, ExecCancel>>>,
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
    }
}

impl ExecRegistry {
    pub fn new() -> Self {
        Self::default()
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

    /// How many commands are running right now.
    pub fn running(&self) -> usize {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
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
}
