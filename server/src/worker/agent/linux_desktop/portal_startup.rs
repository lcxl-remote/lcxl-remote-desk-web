//! The shared portable broker outlives recycled worker tasks. Startup restore
//! must not become a remote-triggered retry through media worker recovery.
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub(crate) struct PortalStartupRestore(AtomicBool);

impl PortalStartupRestore {
    /// Consume before probing: failed initialization must not create a later
    /// automatic retry. Explicit local Portal actions use their separate path.
    pub(crate) fn claim(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }
}
