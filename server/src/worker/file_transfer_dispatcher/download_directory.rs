//! Resolve downloads for the interactive worker, never the daemon's environment.
use std::{io, path::PathBuf};

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

pub(super) fn current() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    let directory = linux::current()?;
    #[cfg(target_os = "macos")]
    let directory = macos::current()?;
    #[cfg(windows)]
    let directory = windows::current()?;
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let directory: PathBuf = return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "downloads directory is unsupported",
    ));

    if !directory.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "downloads directory must be absolute",
        ));
    }
    std::fs::create_dir_all(&directory)?;
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_identity_ignores_inherited_service_environment() {
        #[cfg(target_os = "macos")]
        let resolve = macos::current;
        #[cfg(target_os = "linux")]
        let resolve = linux::current;
        #[cfg(windows)]
        let resolve = windows::current;
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        {
            const EXPECTED: &str = "LRD_TEST_EXPECTED_DOWNLOAD_DIRECTORY";
            if let Some(expected) = std::env::var_os(EXPECTED) {
                assert_eq!(resolve().unwrap(), PathBuf::from(expected));
                return;
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "worker::file_transfer_dispatcher::download_directory::tests::native_identity_ignores_inherited_service_environment", "--nocapture"])
                .env(EXPECTED, resolve().unwrap())
                .env("HOME", "/tmp/file-drop-other-account")
                .env("XDG_CONFIG_HOME", "/tmp/file-drop-other-configuration")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
}
