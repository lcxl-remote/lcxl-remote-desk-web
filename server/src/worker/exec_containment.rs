//! Process-tree reclamation for an executed command.
//!
//! Killing the process we spawned is not the same as stopping what it started.
//! A command that forks, launches a helper, or backgrounds a task leaves those
//! descendants running when the direct child is killed — so a timeout would
//! bound the command we can see while the work it started continues
//! indefinitely. Every execution therefore runs inside a container the whole
//! tree belongs to, and reclaiming the container reclaims the tree.
//!
//! # Two-stage identity
//!
//! Where the platform allows it, the container is created and named *before* the
//! spawn, so a host that crashes mid-spawn can still find and clean up what it
//! started. Where it does not, the identity only exists once there is a pid.
//!
//! | Platform | Named before spawn | Identity |
//! |---|---|---|
//! | Windows | yes — the job object is created first | job name |
//! | Linux / macOS | no — a process group is named by its leader's pid | `pgid:<n>` |
//!
//! On Unix that leaves a window between reserving an execution and knowing how
//! to reclaim it. A host that dies inside that window cannot say what it started,
//! which is why the ledger records such an execution as indeterminate rather than
//! claiming either outcome.
//!
//! # Fail closed
//!
//! If a container cannot be established the command does not run. An execution
//! that cannot be reclaimed is exactly the thing this exists to prevent, and
//! running it anyway would leave a process the host has no way to stop.
//!
//! # Verification status
//!
//! The Unix backend is covered by tests that spawn a real descendant and assert
//! it dies with the command. The Windows backend compiles against the bound
//! `windows` crate's signatures but its process-tree reclamation has not yet been
//! exercised by an equivalent test, so treat its runtime behaviour as unverified.

use tokio::process::{Child, Command};

/// Whether this host can enforce the **native-hard** containment tier (aggregate
/// CPU / memory / process-count hard limits), as opposed to the baseline tier every
/// platform provides (bounded wall time + process-tree recycle + concurrency).
///
/// A plan whose template declares `required_enforcement = native_hard` is refused
/// before dispatch on a host that returns `false`, so it can never run under weaker
/// containment than it demanded. The baseline containment above is unconditional;
/// this only gates the *extra* hard caps. It is `false` on every platform today —
/// the native-hard backend (Linux cgroup `cpu.max` / `memory.max` / `pids.max`,
/// Windows Job Object rate/commit limits) is a follow-on — so a native-hard
/// template is currently unschedulable everywhere (fail-closed, by design).
pub fn provides_native_hard() -> bool {
    false
}

/// Why containment could not be established. The command must not be spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentError(pub desk_agent_protocol::native_diagnostic::NativeDiagnostic);
impl ContainmentError {
    fn message(message: &str) -> Self {
        Self(
            desk_agent_protocol::native_diagnostic::NativeDiagnostic::new(
                desk_agent_protocol::native_diagnostic::DiagnosticStage::ProcessContainment,
                "prepare process containment",
                "application",
                None,
                message,
            ),
        )
    }
}

impl std::fmt::Display for ContainmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.0.operation, self.0.message)
    }
}

/// A container holding one execution's process tree.
///
/// Dropping it reclaims the tree, so every exit path — normal return, timeout,
/// error, panic — leaves nothing behind. A command that needs to outlive its own
/// invocation must hand the work to the platform's service manager rather than
/// background a child, which is the distinction this enforces.
pub struct Containment {
    inner: Platform,
    /// Set once the container can be named: before the spawn where the platform
    /// allows, otherwise after.
    identity: Option<String>,
}

impl Containment {
    /// Establish a container for an execution identified by `generation`.
    ///
    /// Called before the spawn. `generation` only names the container for
    /// diagnosis; reclamation never depends on it.
    pub fn prepare(generation: &str) -> Result<Self, ContainmentError> {
        let (inner, identity) = Platform::prepare(generation)?;
        Ok(Self { inner, identity })
    }

    /// How to find this container again, if it can be named yet. `None` on a
    /// platform that cannot name one until the child exists.
    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    /// Configure the command so its children land inside the container.
    pub fn apply(&self, cmd: &mut Command) {
        self.inner.apply(cmd);
    }

    /// Bind the spawned child to the container and fill in the identity where it
    /// only becomes knowable now.
    ///
    /// A failure here means the child is running but unreclaimable, so the caller
    /// must kill it rather than proceed.
    pub fn adopt(&mut self, child: &Child) -> Result<(), ContainmentError> {
        self.inner.adopt(child)?;
        if let Some(identity) = self.inner.identity_after_adopt() {
            self.identity = Some(identity);
        }
        Ok(())
    }

    /// Reclaim the whole tree now, rather than waiting for the drop.
    pub fn reclaim(
        &mut self,
    ) -> Result<(), desk_agent_protocol::native_diagnostic::NativeDiagnostic> {
        self.inner.reclaim()
    }
}

impl Drop for Containment {
    fn drop(&mut self) {
        let _ = self.inner.reclaim();
    }
}

// ============================ Unix ============================

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "exec_containment/unix.rs"]
mod imp;

// ============================ Windows ============================

#[cfg(target_os = "windows")]
#[path = "exec_containment/windows.rs"]
mod imp;

// ============================ Unsupported ============================

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[path = "exec_containment/unsupported.rs"]
mod imp;

use imp::Platform;

#[cfg(test)]
mod tests {
    use super::*;

    /// The containment is moved into a `tokio::spawn`ed future that drives the
    /// execution, so it must be `Send` on every platform — including Windows,
    /// where it wraps a raw job-object handle that is not `Send` on its own. This
    /// pins the invariant at compile time, so a change that reintroduces a
    /// `!Send` field fails here rather than at a distant spawn site.
    #[test]
    fn containment_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Containment>();
    }
}
