//! The host's own ceiling on how many commands may run at once.
//!
//! The manager schedules executions across a fleet and can cap them centrally,
//! but that cap only binds work the manager dispatched. A control end talking to
//! an open-source signal server reaches this host directly, and a host that
//! trusted the central cap alone would accept as many concurrent commands as it
//! was sent. The limit therefore lives here too, applied to every path.
//!
//! # Why this is not the ledger
//!
//! The ledger answers "did this dispatch already happen", and its answers are
//! permanent so a redelivery can never run twice. Capacity asks "how many are
//! running *now*", and stale answers are actively harmful: an execution the host
//! lost track of would occupy a slot for ever and the host would slowly refuse
//! all work. The two want opposite lifetimes, so they are kept apart.
//!
//! # Self-healing
//!
//! A slot is held under a deadline derived from the command's own timeout rather
//! than released only on completion. If a worker dies without reporting, its
//! slots free themselves once the commands could no longer legitimately still be
//! running — no crash-recovery plumbing required, and no path by which a lost
//! execution permanently shrinks the host's capacity.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Grace added to a command's own timeout before its slot is reclaimed, covering
/// the spawn, the result's trip back, and clock jitter. Generous on purpose:
/// reclaiming a slot early would let one more command run than the ceiling
/// allows, which is the failure this exists to prevent.
const SLOT_GRACE: Duration = Duration::from_secs(30);

/// Refusal returned when the host is already at its ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityFull {
    pub running: usize,
    pub limit: usize,
}

impl std::fmt::Display for CapacityFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this device is already running {} of {} permitted commands",
            self.running, self.limit
        )
    }
}

/// Live count of executions this host has accepted and not yet accounted for.
#[derive(Default)]
pub struct ExecCapacity {
    /// Generation → the moment its slot may be reclaimed even without a report.
    slots: Mutex<HashMap<String, Instant>>,
}

impl ExecCapacity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a slot for `generation`, or refuse if the host is at `limit`.
    ///
    /// Re-admitting a generation that already holds a slot is not an error and
    /// does not consume a second one: the ledger has already decided whether that
    /// dispatch may proceed, and capacity must not double-count one execution.
    pub fn try_admit(
        &self,
        generation: &str,
        limit: usize,
        timeout: Duration,
    ) -> Result<(), CapacityFull> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        slots.retain(|_, deadline| *deadline > now);

        if slots.contains_key(generation) {
            return Ok(());
        }
        if slots.len() >= limit {
            return Err(CapacityFull {
                running: slots.len(),
                limit,
            });
        }
        slots.insert(generation.to_string(), now + timeout + SLOT_GRACE);
        Ok(())
    }

    /// Give back a slot once the execution is accounted for — it finished, or it
    /// never started.
    pub fn release(&self, generation: &str) {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(generation);
    }

    /// How many slots are held right now, ignoring any whose deadline has passed.
    pub fn in_flight(&self) -> usize {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        slots.retain(|_, deadline| *deadline > now);
        slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(10);

    /// The ceiling is enforced, and a released slot is immediately reusable.
    #[test]
    fn the_ceiling_bounds_concurrent_commands() {
        let cap = ExecCapacity::new();
        assert!(cap.try_admit("a", 2, T).is_ok());
        assert!(cap.try_admit("b", 2, T).is_ok());
        assert_eq!(
            cap.try_admit("c", 2, T),
            Err(CapacityFull {
                running: 2,
                limit: 2
            })
        );

        cap.release("a");
        assert!(cap.try_admit("c", 2, T).is_ok());
        assert_eq!(cap.in_flight(), 2);
    }

    /// Re-admitting the same dispatch does not consume a second slot; whether a
    /// redelivery may proceed at all is the ledger's decision, not capacity's.
    #[test]
    fn one_dispatch_never_holds_two_slots() {
        let cap = ExecCapacity::new();
        assert!(cap.try_admit("a", 1, T).is_ok());
        assert!(cap.try_admit("a", 1, T).is_ok());
        assert_eq!(cap.in_flight(), 1);
    }

    /// A slot whose command could no longer be running is reclaimed without any
    /// report, so a worker that died without answering cannot permanently shrink
    /// the host's capacity.
    #[test]
    fn a_lost_execution_does_not_hold_its_slot_for_ever() {
        let cap = ExecCapacity::new();
        // A command whose deadline is already in the past: the worker vanished.
        cap.slots
            .lock()
            .unwrap()
            .insert("lost".to_string(), Instant::now() - Duration::from_secs(1));

        assert_eq!(cap.in_flight(), 0, "the stale slot was not reclaimed");
        assert!(cap.try_admit("fresh", 1, T).is_ok());
    }

    /// A limit of zero refuses everything rather than admitting one.
    #[test]
    fn a_zero_limit_admits_nothing() {
        let cap = ExecCapacity::new();
        assert!(cap.try_admit("a", 0, T).is_err());
    }
}
