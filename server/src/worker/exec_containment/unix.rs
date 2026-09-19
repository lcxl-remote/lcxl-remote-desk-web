use super::*;

/// A POSIX process group. The child becomes the group leader, so everything it
/// spawns inherits the group and a single signal reaches all of it.
///
/// Deliberately not cgroups on Linux: creating one requires a delegated subtree
/// the host usually does not have when it runs as an ordinary user, so it would
/// fail closed on exactly the common deployment. A process group works
/// unprivileged. It is escapable — a descendant may call `setsid` — but nothing
/// short of cgroups prevents that, and the practical leak this closes is the
/// ordinary child that simply inherits the group.
pub struct Platform {
    pgid: Option<libc::pid_t>,
}

impl Platform {
    pub fn prepare(_generation: &str) -> Result<(Self, Option<String>), ContainmentError> {
        // A process group is named by its leader, which does not exist yet.
        Ok((Self { pgid: None }, None))
    }

    pub fn apply(&self, cmd: &mut Command) {
        // 0 means "new group led by the child", so the group id is the child pid.
        cmd.process_group(0);
    }

    pub fn adopt(&mut self, child: &Child) -> Result<(), ContainmentError> {
        let pid = child.id().ok_or_else(|| {
            ContainmentError::message("the child exited before it could be contained")
        })?;
        self.pgid = Some(pid as libc::pid_t);
        Ok(())
    }

    pub fn identity_after_adopt(&self) -> Option<String> {
        self.pgid.map(|p| format!("pgid:{p}"))
    }

    pub fn reclaim(
        &mut self,
    ) -> Result<(), desk_agent_protocol::native_diagnostic::NativeDiagnostic> {
        let Some(pgid) = self.pgid.take() else {
            return Ok(());
        };
        // Negative pid addresses the whole group. ESRCH (already gone) is the
        // normal case after a clean exit and is not worth reporting.
        if unsafe { libc::kill(-pgid, libc::SIGKILL) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                self.pgid = Some(pgid);
                return Err(
                    desk_agent_protocol::native_diagnostic::NativeDiagnostic::from_io(
                        desk_agent_protocol::native_diagnostic::DiagnosticStage::ProcessCleanup,
                        "kill process group",
                        &error,
                    ),
                );
            }
        }
        Ok(())
    }
}
