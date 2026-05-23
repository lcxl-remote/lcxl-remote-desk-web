//! Windows-only install/uninstall pipeline. The PowerShell
//! `Get-WindowsDriver -Online` path returns structured (locale-stable)
//! data; if that fails (for instance the worker is running without
//! admin), we fall back to text-parsing `pnputil /enum-drivers`. When
//! both fail, the status is reported as unknown (`installed: None`)
//! rather than fabricating a confident "not installed" answer.

use crate::{
    DriverFiles, DriverStatus, InstallerError,
    command::CommandRunner,
    oem,
    parser::{parse_pnputil_enum, parse_ps_get_windows_driver},
};

const PS_PROGRAM: &str = "powershell";
const PS_ARGS: &[&str] = &[
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    "Get-WindowsDriver -Online | Where-Object { $_.OriginalFileName -and ([System.IO.Path]::GetFileName($_.OriginalFileName) -ieq 'LcxlVirtualDisplay.inf') } | Select-Object Driver, OriginalFileName | ConvertTo-Json -Compress",
];

pub(crate) fn query_install_status(
    runner: &dyn CommandRunner,
) -> Result<DriverStatus, InstallerError> {
    let ps_oems = match runner.run(PS_PROGRAM, PS_ARGS) {
        Ok(out) if out.status == Some(0) => match parse_ps_get_windows_driver(&out.stdout) {
            Ok(v) => Some(v),
            Err(e) => {
                log::warn!(
                    "[installer] Get-WindowsDriver JSON parse failed: {e}; falling back to pnputil"
                );
                None
            }
        },
        Ok(out) => {
            log::warn!(
                "[installer] Get-WindowsDriver exit={:?} stderr={}; falling back to pnputil",
                out.status,
                out.stderr.trim()
            );
            None
        }
        Err(e) => {
            log::warn!("[installer] Get-WindowsDriver spawn failed: {e}; falling back to pnputil");
            None
        }
    };

    let oems = if let Some(v) = ps_oems {
        Some(v)
    } else {
        match runner.run("pnputil", &["/enum-drivers"]) {
            Ok(out) if out.status == Some(0) => Some(parse_pnputil_enum(&out.stdout)),
            Ok(out) => {
                log::warn!(
                    "[installer] pnputil /enum-drivers exit={:?} stderr={}",
                    out.status,
                    out.stderr.trim()
                );
                None
            }
            Err(e) => {
                log::warn!("[installer] pnputil /enum-drivers spawn failed: {e}");
                None
            }
        }
    };

    let installed = oems.as_ref().map(|v| !v.is_empty());
    // files_available / files_dir are filled in at the controller
    // layer (it owns the exe-dir path). The installer-level status
    // only reports the driver-side facts.
    Ok(DriverStatus {
        files_available: false,
        files_dir: None,
        installed,
        installed_oem_infs: oems,
    })
}

pub(crate) fn install(
    runner: &dyn CommandRunner,
    files: &DriverFiles,
) -> Result<(), InstallerError> {
    let inf = files.inf.to_str().ok_or_else(|| {
        InstallerError::Parse(format!("non-utf8 inf path: {}", files.inf.display()))
    })?;
    let out = runner.run("pnputil", &["/add-driver", inf, "/install"])?;
    if out.status != Some(0) {
        return Err(InstallerError::CommandFailed {
            command: format!("pnputil /add-driver {inf} /install"),
            exit_code: out.status,
            stderr: out.stderr,
        });
    }
    Ok(())
}

pub(crate) fn uninstall_all(runner: &dyn CommandRunner) -> Result<usize, InstallerError> {
    let status = query_install_status(runner)?;
    let oems = status.installed_oem_infs.ok_or(InstallerError::StatusUnknown)?;
    let mut removed = 0usize;
    for oem_name in oems {
        oem::validate(&oem_name)?;
        let out = runner.run(
            "pnputil",
            &["/delete-driver", &oem_name, "/uninstall", "/force"],
        )?;
        if out.status != Some(0) {
            // Best-effort: log and keep going so a partial uninstall
            // doesn't strand later oem entries. Caller sees how many
            // we actually removed via the return value.
            log::warn!(
                "[installer] pnputil /delete-driver {oem_name} exit={:?} stderr={}; continuing",
                out.status,
                out.stderr.trim()
            );
            continue;
        }
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CommandOutput;
    use std::path::PathBuf;
    use std::sync::Mutex;

    enum MockReply {
        Ok {
            status: Option<i32>,
            stdout: String,
            stderr: String,
        },
        Err(std::io::ErrorKind),
    }

    struct MockRunner {
        replies: Mutex<Vec<MockReply>>,
        invocations: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl MockRunner {
        fn new(replies: Vec<MockReply>) -> Self {
            Self {
                replies: Mutex::new(replies),
                invocations: Mutex::new(Vec::new()),
            }
        }
        fn invocations(&self) -> Vec<(String, Vec<String>)> {
            self.invocations.lock().unwrap().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, InstallerError> {
            self.invocations.lock().unwrap().push((
                program.to_owned(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            let reply = self.replies.lock().unwrap().remove(0);
            match reply {
                MockReply::Ok {
                    status,
                    stdout,
                    stderr,
                } => Ok(CommandOutput {
                    status,
                    stdout,
                    stderr,
                }),
                MockReply::Err(kind) => Err(InstallerError::Io(std::io::Error::new(kind, "mock"))),
            }
        }
    }

    #[test]
    fn ps_success_short_circuits_pnputil() {
        let runner = MockRunner::new(vec![MockReply::Ok {
            status: Some(0),
            stdout: r#"{"Driver":"oem23.inf","OriginalFileName":"X\\LcxlVirtualDisplay.inf"}"#
                .into(),
            stderr: String::new(),
        }]);
        let st = query_install_status(&runner).unwrap();
        assert_eq!(st.installed, Some(true));
        assert_eq!(st.installed_oem_infs, Some(vec!["oem23.inf".into()]));
        let invocations = runner.invocations();
        assert_eq!(invocations.len(), 1, "pnputil must not be invoked");
        assert_eq!(invocations[0].0, PS_PROGRAM);
    }

    #[test]
    fn ps_failure_falls_back_to_pnputil() {
        let runner = MockRunner::new(vec![
            MockReply::Ok {
                status: Some(1),
                stdout: String::new(),
                stderr: "access denied".into(),
            },
            MockReply::Ok {
                status: Some(0),
                stdout: "Published Name : oem55.inf\nOriginal Name : LcxlVirtualDisplay.inf\n"
                    .into(),
                stderr: String::new(),
            },
        ]);
        let st = query_install_status(&runner).unwrap();
        assert_eq!(st.installed, Some(true));
        assert_eq!(st.installed_oem_infs, Some(vec!["oem55.inf".into()]));
        let invocations = runner.invocations();
        assert_eq!(invocations.len(), 2);
        assert_eq!(invocations[1].0, "pnputil");
        assert_eq!(invocations[1].1, vec!["/enum-drivers".to_owned()]);
    }

    #[test]
    fn ps_spawn_failure_falls_back_to_pnputil() {
        let runner = MockRunner::new(vec![
            MockReply::Err(std::io::ErrorKind::NotFound),
            MockReply::Ok {
                status: Some(0),
                stdout: "Published Name : oem55.inf\nOriginal Name : LcxlVirtualDisplay.inf\n"
                    .into(),
                stderr: String::new(),
            },
        ]);
        let st = query_install_status(&runner).unwrap();
        assert_eq!(st.installed, Some(true));
    }

    #[test]
    fn both_paths_failing_returns_unknown_state() {
        let runner = MockRunner::new(vec![
            MockReply::Err(std::io::ErrorKind::NotFound),
            MockReply::Err(std::io::ErrorKind::NotFound),
        ]);
        let st = query_install_status(&runner).unwrap();
        assert_eq!(st.installed, None);
        assert_eq!(st.installed_oem_infs, None);
    }

    #[test]
    fn ps_zero_matches_reports_installed_false_with_empty_vec() {
        let runner = MockRunner::new(vec![MockReply::Ok {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        }]);
        let st = query_install_status(&runner).unwrap();
        assert_eq!(st.installed, Some(false));
        assert_eq!(st.installed_oem_infs, Some(Vec::<String>::new()));
    }

    #[test]
    fn install_invokes_pnputil_add_driver_install_with_correct_args() {
        let runner = MockRunner::new(vec![MockReply::Ok {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        }]);
        let dir = PathBuf::from(if cfg!(target_os = "windows") {
            "C:\\drivers\\LcxlVirtualDisplay"
        } else {
            "/tmp/drivers/LcxlVirtualDisplay"
        });
        let files = DriverFiles::from_dir(dir);
        install(&runner, &files).unwrap();
        let invocations = runner.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].0, "pnputil");
        assert_eq!(invocations[0].1[0], "/add-driver");
        assert!(invocations[0].1[1].ends_with("LcxlVirtualDisplay.inf"));
        assert_eq!(invocations[0].1[2], "/install");
    }

    #[test]
    fn install_returns_command_failed_when_pnputil_nonzero() {
        let runner = MockRunner::new(vec![MockReply::Ok {
            status: Some(1),
            stdout: String::new(),
            stderr: "bad".into(),
        }]);
        let files = DriverFiles::from_dir(PathBuf::from(if cfg!(target_os = "windows") {
            "C:\\x"
        } else {
            "/tmp/x"
        }));
        let err = install(&runner, &files).unwrap_err();
        match err {
            InstallerError::CommandFailed {
                exit_code, stderr, ..
            } => {
                assert_eq!(exit_code, Some(1));
                assert_eq!(stderr, "bad");
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn uninstall_all_with_unknown_status_returns_status_unknown() {
        let runner = MockRunner::new(vec![
            MockReply::Err(std::io::ErrorKind::NotFound),
            MockReply::Err(std::io::ErrorKind::NotFound),
        ]);
        let err = uninstall_all(&runner).unwrap_err();
        assert!(matches!(err, InstallerError::StatusUnknown));
    }

    #[test]
    fn uninstall_all_removes_each_oem_after_validation() {
        let runner = MockRunner::new(vec![
            MockReply::Ok {
                status: Some(0),
                stdout: r#"[{"Driver":"oem23.inf","OriginalFileName":"X\\LcxlVirtualDisplay.inf"},{"Driver":"oem55.inf","OriginalFileName":"Y\\LcxlVirtualDisplay.inf"}]"#.into(),
                stderr: String::new(),
            },
            MockReply::Ok {
                status: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
            MockReply::Ok {
                status: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);
        let removed = uninstall_all(&runner).unwrap();
        assert_eq!(removed, 2);
        let invocations = runner.invocations();
        assert_eq!(invocations.len(), 3);
        assert_eq!(
            invocations[1].1,
            vec![
                "/delete-driver".to_owned(),
                "oem23.inf".to_owned(),
                "/uninstall".to_owned(),
                "/force".to_owned(),
            ]
        );
        assert_eq!(invocations[2].1[1], "oem55.inf");
    }

    #[test]
    fn uninstall_all_continues_on_partial_failure_and_reports_removed_count() {
        let runner = MockRunner::new(vec![
            MockReply::Ok {
                status: Some(0),
                stdout: r#"[{"Driver":"oem23.inf","OriginalFileName":"X\\LcxlVirtualDisplay.inf"},{"Driver":"oem55.inf","OriginalFileName":"Y\\LcxlVirtualDisplay.inf"}]"#.into(),
                stderr: String::new(),
            },
            MockReply::Ok {
                status: Some(1),
                stdout: String::new(),
                stderr: "in use".into(),
            },
            MockReply::Ok {
                status: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
        ]);
        let removed = uninstall_all(&runner).unwrap();
        assert_eq!(removed, 1);
    }

    #[test]
    fn uninstall_all_zero_oems_returns_zero_without_calling_pnputil_delete() {
        let runner = MockRunner::new(vec![MockReply::Ok {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        }]);
        let removed = uninstall_all(&runner).unwrap();
        assert_eq!(removed, 0);
        // Only the status query should have been invoked.
        let invocations = runner.invocations();
        assert_eq!(invocations.len(), 1);
    }
}
