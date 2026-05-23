//! Tiny abstraction over external process invocation. Lets the
//! Windows-only install/uninstall logic be unit-tested against
//! deterministic mock outputs instead of really spawning
//! `pnputil` / `powershell`.

use std::process::Command;

use crate::InstallerError;

pub(crate) struct CommandOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub(crate) trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, InstallerError>;
}

pub(crate) struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, InstallerError> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(InstallerError::Io)?;
        Ok(CommandOutput {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
