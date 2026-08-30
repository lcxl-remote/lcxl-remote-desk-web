//! Linux systemd installation lifecycle for ServiceDaemon mode.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub const SERVICE_UNIT_NAME: &str = "lcxl-remote-desk.service";
const SYSTEM_UNIT_PATH: &str = "/etc/systemd/system/lcxl-remote-desk.service";
const SYSTEM_CONFIG_PATH: &str = "/etc/lcxl-remote-desk/config.toml";
const INSTALLED_SERVER_NAME: &str = "lcxl-remote-desk-server";
const DEFAULT_INSTALL_DIR: &str = "/usr/lib/lcxl-remote-desk";
pub const EXPERIMENTAL_INSTALL_ENV: &str = "LRD_EXPERIMENTAL_LINUX_SERVICE_DAEMON";

type ServiceResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

trait SystemctlOps {
    fn succeeds(&self, args: &[&str]) -> bool;
    fn run(&self, args: &[&str]) -> ServiceResult<()>;
}

struct RealSystemctl;

impl SystemctlOps for RealSystemctl {
    fn succeeds(&self, args: &[&str]) -> bool {
        Command::new("systemctl")
            .args(args)
            .status()
            .is_ok_and(|status| status.success())
    }

    fn run(&self, args: &[&str]) -> ServiceResult<()> {
        let status = Command::new("systemctl").args(args).status()?;
        if !status.success() {
            return Err(format!("systemctl {} exited with {status}", args.join(" ")).into());
        }
        Ok(())
    }
}

struct InstallArtifacts<'a> {
    source_executable: &'a Path,
    installed_executable: &'a Path,
    source_static: Option<&'a Path>,
    installed_static: &'a Path,
    migration_source: Option<&'a Path>,
    system_config: &'a Path,
    unit: &'a [u8],
    system_unit: &'a Path,
    polkit_policy: &'a [u8],
    polkit_policy_path: &'a Path,
}

pub fn install_service(
    install_dir: &str,
    source_config: Option<&Path>,
    experimental_opt_in: bool,
) -> ServiceResult<()> {
    if !experimental_opt_in {
        return Err(format!(
            "Linux ServiceDaemon installation is not production-ready; set {EXPERIMENTAL_INSTALL_ENV}=1 only for development validation"
        )
        .into());
    }
    require_root()?;
    let install_dir = validate_install_dir(Path::new(install_dir))?;
    let source_executable = std::env::current_exe()?;
    validate_install_source(&source_executable)?;
    let system_config = Path::new(SYSTEM_CONFIG_PATH);
    let migration_source = if system_config.exists() {
        None
    } else {
        let source_config = source_config.ok_or(
            "first Linux service installation requires the current user config for token migration",
        )?;
        validate_config_source(source_config)?;
        Some(source_config)
    };

    fs::create_dir_all(&install_dir)?;
    fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o755))?;
    let installed_executable = install_dir.join(INSTALLED_SERVER_NAME);
    let source_static = source_executable
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .join("static");
    let unit = render_unit(&installed_executable, &install_dir);
    install_artifacts_transactionally(
        InstallArtifacts {
            source_executable: &source_executable,
            installed_executable: &installed_executable,
            source_static: source_static.is_dir().then_some(source_static.as_path()),
            installed_static: &install_dir.join("static"),
            migration_source,
            system_config,
            unit: unit.as_bytes(),
            system_unit: Path::new(SYSTEM_UNIT_PATH),
            polkit_policy: crate::daemon::linux_privileged_exec::POLKIT_POLICY_XML.as_bytes(),
            polkit_policy_path: Path::new(crate::daemon::linux_privileged_exec::POLKIT_POLICY_PATH),
        },
        &RealSystemctl,
    )
}

pub fn uninstall_service() -> ServiceResult<()> {
    require_root()?;
    uninstall_service_transactionally(
        Path::new(SYSTEM_UNIT_PATH),
        Path::new(crate::daemon::linux_privileged_exec::POLKIT_POLICY_PATH),
        &RealSystemctl,
    )
}

fn require_root() -> ServiceResult<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("Linux service management requires root".into());
    }
    Ok(())
}

fn validate_install_dir(path: &Path) -> ServiceResult<PathBuf> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
        || path == Path::new("/")
        || path
            .to_str()
            .is_none_or(|value| value.chars().any(|ch| ch.is_whitespace() || ch == '%'))
    {
        return Err("install path must be a specific absolute directory".into());
    }
    if path != Path::new(DEFAULT_INSTALL_DIR) {
        return Err(format!("Linux service install path is fixed at {DEFAULT_INSTALL_DIR}").into());
    }
    Ok(path.to_path_buf())
}

fn validate_install_source(path: &Path) -> ServiceResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err("service install source is not a regular file".into());
    }
    if metadata.mode() & 0o022 != 0 {
        return Err("service install source is group/world-writable".into());
    }
    Ok(())
}

fn validate_config_source(path: &Path) -> ServiceResult<()> {
    if !path.is_absolute() {
        return Err("config migration source must be absolute".into());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err("config migration source is not a regular file".into());
    }
    Ok(())
}

fn render_unit(executable: &Path, install_dir: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=LCXL Remote Desktop ServiceDaemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         Type=simple\n\
         User=root\n\
         Group=root\n\
         WorkingDirectory={}\n\
         ExecStart={} --startup-mode service-daemon\n\
         Restart=on-failure\n\
         RestartSec=2s\n\
         RuntimeDirectory=lcxl-remote-desk\n\
         RuntimeDirectoryMode=0700\n\
         StateDirectory=lcxl-remote-desk\n\
         StateDirectoryMode=0700\n\
         LogsDirectory=lcxl-remote-desk\n\
         LogsDirectoryMode=0750\n\
         LimitNOFILE=65536\n\
         TasksMax=512\n\
         ProtectSystem=full\n\
         ReadWritePaths=/etc/lcxl-remote-desk /var/lib/lcxl-remote-desk /var/log/lcxl-remote-desk /run/lcxl-remote-desk\n\
         ProtectKernelTunables=true\n\
         ProtectKernelModules=true\n\
         ProtectControlGroups=false\n\
         RestrictSUIDSGID=true\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        install_dir.display(),
        executable.display(),
    )
}

fn install_artifacts_transactionally(
    artifacts: InstallArtifacts<'_>,
    systemctl: &dyn SystemctlOps,
) -> ServiceResult<()> {
    let was_enabled = systemctl.succeeds(&["is-enabled", "--quiet", SERVICE_UNIT_NAME]);
    let was_active = systemctl.succeeds(&["is-active", "--quiet", SERVICE_UNIT_NAME]);
    let mut transaction = ArtifactTransaction::default();

    let staged = (|| -> io::Result<()> {
        transaction.replace_file_from(
            artifacts.source_executable,
            artifacts.installed_executable,
            0o755,
        )?;
        if let Some(source_static) = artifacts.source_static {
            transaction.replace_directory(source_static, artifacts.installed_static)?;
        }
        if let Some(source_config) = artifacts.migration_source {
            transaction.replace_file_from(source_config, artifacts.system_config, 0o600)?;
        }
        transaction.replace_file_bytes(artifacts.unit, artifacts.system_unit, 0o644)?;
        transaction.replace_file_bytes(
            artifacts.polkit_policy,
            artifacts.polkit_policy_path,
            0o644,
        )?;
        Ok(())
    })();
    if let Err(error) = staged {
        let rollback = transaction.rollback();
        return Err(with_rollback_detail(
            "staging Linux service artifacts",
            error,
            rollback,
        ));
    }

    let activation = (|| -> ServiceResult<()> {
        systemctl.run(&["daemon-reload"])?;
        systemctl.run(&["enable", SERVICE_UNIT_NAME])?;
        systemctl.run(&["restart", SERVICE_UNIT_NAME])?;
        Ok(())
    })();
    if let Err(error) = activation {
        let mut rollback_errors = Vec::new();

        // Undo runtime state before removing a newly installed unit/symlink.
        // For an upgrade that was already enabled/active, restore artifacts
        // first and then reload/restart the previous version below.
        if !was_active && let Err(restore_error) = systemctl.run(&["stop", SERVICE_UNIT_NAME]) {
            rollback_errors.push(format!("stop new service: {restore_error}"));
        }
        if !was_enabled && let Err(restore_error) = systemctl.run(&["disable", SERVICE_UNIT_NAME]) {
            rollback_errors.push(format!("disable new service: {restore_error}"));
        }
        if let Err(restore_error) = transaction.rollback() {
            rollback_errors.push(restore_error);
        }
        if let Err(restore_error) = systemctl.run(&["daemon-reload"]) {
            rollback_errors.push(format!("reload restored units: {restore_error}"));
        }
        if was_enabled && let Err(restore_error) = systemctl.run(&["enable", SERVICE_UNIT_NAME]) {
            rollback_errors.push(format!("restore enabled state: {restore_error}"));
        }
        if was_active && let Err(restore_error) = systemctl.run(&["restart", SERVICE_UNIT_NAME]) {
            rollback_errors.push(format!("restart restored service: {restore_error}"));
        }

        let rollback = if rollback_errors.is_empty() {
            Ok(())
        } else {
            Err(rollback_errors.join("; "))
        };
        return Err(with_rollback_detail(
            "activating Linux service",
            error,
            rollback,
        ));
    }

    if let Err(error) = transaction.commit() {
        // The active target files are already the requested version. Backup
        // cleanup failure must be visible in logs without falsely reporting
        // that the running service failed to install.
        log::warn!("Linux service installed but transaction backup cleanup failed: {error}");
    }
    Ok(())
}

fn uninstall_service_transactionally(
    system_unit: &Path,
    polkit_policy: &Path,
    systemctl: &dyn SystemctlOps,
) -> ServiceResult<()> {
    let unit_exists = validate_optional_regular_file(system_unit, "systemd unit")?;
    let policy_exists = validate_optional_regular_file(polkit_policy, "polkit policy")?;

    let was_enabled =
        unit_exists && systemctl.succeeds(&["is-enabled", "--quiet", SERVICE_UNIT_NAME]);
    let was_active =
        unit_exists && systemctl.succeeds(&["is-active", "--quiet", SERVICE_UNIT_NAME]);
    let mut transaction = ArtifactTransaction::default();
    let removal = (|| -> ServiceResult<()> {
        // Do not unlink a unit whose process we failed to stop: that would
        // leave an unmanageable root daemon running until reboot.
        if unit_exists {
            systemctl.run(&["disable", "--now", SERVICE_UNIT_NAME])?;
            transaction.remove_file(system_unit)?;
        }
        if policy_exists {
            transaction.remove_file(polkit_policy)?;
        }
        systemctl.run(&["daemon-reload"])?;
        Ok(())
    })();
    if let Err(error) = removal {
        let mut rollback_errors = Vec::new();
        if let Err(restore_error) = transaction.rollback() {
            rollback_errors.push(restore_error);
        }
        if let Err(restore_error) = systemctl.run(&["daemon-reload"]) {
            rollback_errors.push(format!("reload restored unit: {restore_error}"));
        }
        if was_enabled && let Err(restore_error) = systemctl.run(&["enable", SERVICE_UNIT_NAME]) {
            rollback_errors.push(format!("restore enabled state: {restore_error}"));
        }
        if was_active && let Err(restore_error) = systemctl.run(&["restart", SERVICE_UNIT_NAME]) {
            rollback_errors.push(format!("restart restored service: {restore_error}"));
        }
        let rollback = if rollback_errors.is_empty() {
            Ok(())
        } else {
            Err(rollback_errors.join("; "))
        };
        return Err(with_rollback_detail(
            "uninstalling Linux service",
            error,
            rollback,
        ));
    }

    if let Err(error) = transaction.commit() {
        log::warn!("Linux service uninstalled but unit backup cleanup failed: {error}");
    }
    Ok(())
}

fn validate_optional_regular_file(path: &Path, label: &str) -> ServiceResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(true)
        }
        Ok(_) => Err(format!("refusing to remove non-regular {label} {}", path.display()).into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn with_rollback_detail(
    stage: &str,
    primary: impl std::fmt::Display,
    rollback: Result<(), String>,
) -> Box<dyn std::error::Error + Send + Sync> {
    match rollback {
        Ok(()) => format!("failed while {stage}: {primary}; previous installation restored").into(),
        Err(rollback) => {
            format!("failed while {stage}: {primary}; rollback was incomplete: {rollback}").into()
        }
    }
}

#[derive(Default)]
struct ArtifactTransaction {
    undo: Vec<ArtifactUndo>,
}

enum ArtifactUndo {
    RemoveFile(PathBuf),
    RemoveDirectory(PathBuf),
    RestoreFile { target: PathBuf, backup: PathBuf },
    RestoreDirectory { target: PathBuf, backup: PathBuf },
}

impl ArtifactTransaction {
    fn remove_file(&mut self, target: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(target)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to remove non-regular file {}", target.display()),
            ));
        }
        let backup = transaction_path(target, "previous");
        fs::rename(target, &backup)?;
        self.undo.push(ArtifactUndo::RestoreFile {
            target: target.to_path_buf(),
            backup,
        });
        Ok(())
    }

    fn replace_file_from(&mut self, source: &Path, target: &Path, mode: u32) -> io::Result<()> {
        let bytes = fs::read(source)?;
        self.replace_file_bytes(&bytes, target, mode)
    }

    fn replace_file_bytes(&mut self, bytes: &[u8], target: &Path, mode: u32) -> io::Result<()> {
        let staged = stage_file(bytes, target, mode)?;
        if let Err(error) = self.activate_staged_file(&staged, target) {
            let _ = fs::remove_file(staged);
            return Err(error);
        }
        Ok(())
    }

    fn activate_staged_file(&mut self, staged: &Path, target: &Path) -> io::Result<()> {
        match fs::symlink_metadata(target) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("refusing to replace non-regular file {}", target.display()),
                    ));
                }
                let backup = transaction_path(target, "previous");
                fs::rename(target, &backup)?;
                if let Err(error) = fs::rename(staged, target) {
                    let _ = fs::rename(&backup, target);
                    return Err(error);
                }
                self.undo.push(ArtifactUndo::RestoreFile {
                    target: target.to_path_buf(),
                    backup,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::rename(staged, target)?;
                self.undo
                    .push(ArtifactUndo::RemoveFile(target.to_path_buf()));
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn replace_directory(&mut self, source: &Path, target: &Path) -> io::Result<()> {
        let staging = transaction_path(target, "staging");
        if let Err(error) = copy_directory(source, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        match fs::symlink_metadata(target) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    let _ = fs::remove_dir_all(&staging);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("refusing to replace non-directory {}", target.display()),
                    ));
                }
                let backup = transaction_path(target, "previous");
                if let Err(error) = fs::rename(target, &backup) {
                    let _ = fs::remove_dir_all(&staging);
                    return Err(error);
                }
                if let Err(error) = fs::rename(&staging, target) {
                    let _ = fs::rename(&backup, target);
                    let _ = fs::remove_dir_all(&staging);
                    return Err(error);
                }
                self.undo.push(ArtifactUndo::RestoreDirectory {
                    target: target.to_path_buf(),
                    backup,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Err(error) = fs::rename(&staging, target) {
                    let _ = fs::remove_dir_all(&staging);
                    return Err(error);
                }
                self.undo
                    .push(ArtifactUndo::RemoveDirectory(target.to_path_buf()));
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        }
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        while let Some(undo) = self.undo.pop() {
            let result = match undo {
                ArtifactUndo::RemoveFile(target) => remove_file_if_exists(&target),
                ArtifactUndo::RemoveDirectory(target) => remove_directory_if_exists(&target),
                ArtifactUndo::RestoreFile { target, backup } => {
                    remove_file_if_exists(&target).and_then(|()| fs::rename(backup, target))
                }
                ArtifactUndo::RestoreDirectory { target, backup } => {
                    remove_directory_if_exists(&target).and_then(|()| fs::rename(backup, target))
                }
            };
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn commit(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        while let Some(undo) = self.undo.pop() {
            let result = match undo {
                ArtifactUndo::RestoreFile { backup, .. } => remove_file_if_exists(&backup),
                ArtifactUndo::RestoreDirectory { backup, .. } => {
                    remove_directory_if_exists(&backup)
                }
                ArtifactUndo::RemoveFile(_) | ArtifactUndo::RemoveDirectory(_) => Ok(()),
            };
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

fn transaction_path(target: &Path, label: &str) -> PathBuf {
    target.with_extension(format!("{label}-{}", uuid::Uuid::new_v4()))
}

fn stage_file(bytes: &[u8], target: &Path, mode: u32) -> io::Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = transaction_path(target, "staging");
    if let Err(error) = fs::write(&temporary, bytes) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::set_permissions(&temporary, fs::Permissions::from_mode(mode)) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(temporary)
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_directory_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn copy_directory(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "static source contains a non-directory root",
        ));
    }
    fs::create_dir(target)?;
    fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "static source contains a symlink",
            ));
        }
        if metadata.is_dir() {
            copy_directory(&source_path, &target_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &target_path)?;
            fs::set_permissions(&target_path, fs::Permissions::from_mode(0o644))?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "static source contains a special file",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FailingRestartSystemctl {
        enabled: bool,
        active: bool,
        fail_restart_once: AtomicBool,
        calls: Mutex<Vec<Vec<String>>>,
    }

    struct FailingReloadSystemctl {
        fail_reload_once: AtomicBool,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl SystemctlOps for FailingReloadSystemctl {
        fn succeeds(&self, args: &[&str]) -> bool {
            matches!(
                args,
                ["is-enabled", "--quiet", SERVICE_UNIT_NAME]
                    | ["is-active", "--quiet", SERVICE_UNIT_NAME]
            )
        }

        fn run(&self, args: &[&str]) -> ServiceResult<()> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|value| (*value).to_string()).collect());
            if args == ["daemon-reload"] && self.fail_reload_once.swap(false, Ordering::AcqRel) {
                return Err(io::Error::other("injected daemon-reload failure").into());
            }
            Ok(())
        }
    }

    impl SystemctlOps for FailingRestartSystemctl {
        fn succeeds(&self, args: &[&str]) -> bool {
            match args {
                ["is-enabled", "--quiet", SERVICE_UNIT_NAME] => self.enabled,
                ["is-active", "--quiet", SERVICE_UNIT_NAME] => self.active,
                _ => false,
            }
        }

        fn run(&self, args: &[&str]) -> ServiceResult<()> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|value| (*value).to_string()).collect());
            if args == ["restart", SERVICE_UNIT_NAME]
                && self.fail_restart_once.swap(false, Ordering::AcqRel)
            {
                return Err(io::Error::other("injected restart failure").into());
            }
            Ok(())
        }
    }

    #[test]
    fn unit_runs_the_installed_binary_as_service_daemon() {
        let unit = render_unit(
            Path::new("/usr/lib/lcxl-remote-desk/lcxl-remote-desk-server"),
            Path::new("/usr/lib/lcxl-remote-desk"),
        );
        assert!(unit.contains(
            "ExecStart=/usr/lib/lcxl-remote-desk/lcxl-remote-desk-server --startup-mode service-daemon"
        ));
        assert!(unit.contains("User=root"));
        assert!(unit.contains("RuntimeDirectoryMode=0700"));
        assert!(!unit.contains("Environment="));
        assert!(!unit.contains("PrivateTmp=true"));
    }

    #[test]
    fn install_directory_validation_rejects_root_and_parent_components() {
        assert!(validate_install_dir(Path::new("/")).is_err());
        assert!(validate_install_dir(Path::new("relative/path")).is_err());
        assert!(validate_install_dir(Path::new("/opt/../tmp/lcxl")).is_err());
        assert_eq!(
            validate_install_dir(Path::new("/usr/lib/lcxl-remote-desk")).unwrap(),
            Path::new("/usr/lib/lcxl-remote-desk")
        );
        assert!(validate_install_dir(Path::new("/opt/lcxl-remote-desk")).is_err());
    }

    #[test]
    fn atomic_copy_sets_requested_private_mode() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("config.toml");
        let mut transaction = ArtifactTransaction::default();
        transaction
            .replace_file_bytes(b"secret = true\n", &target, 0o600)
            .unwrap();
        transaction.commit().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"secret = true\n");
        assert_eq!(fs::metadata(target).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn failed_restart_restores_every_previous_artifact_and_runtime_state() {
        let root = tempfile::tempdir().unwrap();
        let source_root = root.path().join("source");
        let install_root = root.path().join("install");
        let source_static = source_root.join("static");
        let installed_static = install_root.join("static");
        let source_executable = source_root.join("server");
        let installed_executable = install_root.join("server");
        let source_config = source_root.join("config.toml");
        let system_config = root.path().join("etc/config.toml");
        let system_unit = root.path().join("systemd/service.service");
        let polkit_policy = root.path().join("polkit/action.policy");

        fs::create_dir_all(&source_static).unwrap();
        fs::create_dir_all(&installed_static).unwrap();
        fs::create_dir_all(system_unit.parent().unwrap()).unwrap();
        fs::create_dir_all(polkit_policy.parent().unwrap()).unwrap();
        fs::write(&source_executable, b"new executable").unwrap();
        fs::write(source_static.join("index.html"), b"new static").unwrap();
        fs::write(&source_config, b"new config").unwrap();
        fs::write(&installed_executable, b"old executable").unwrap();
        fs::write(installed_static.join("index.html"), b"old static").unwrap();
        fs::write(&system_unit, b"old unit").unwrap();
        fs::write(&polkit_policy, b"old policy").unwrap();

        let systemctl = FailingRestartSystemctl {
            enabled: true,
            active: true,
            fail_restart_once: AtomicBool::new(true),
            calls: Mutex::new(Vec::new()),
        };
        let error = install_artifacts_transactionally(
            InstallArtifacts {
                source_executable: &source_executable,
                installed_executable: &installed_executable,
                source_static: Some(&source_static),
                installed_static: &installed_static,
                migration_source: Some(&source_config),
                system_config: &system_config,
                unit: b"new unit",
                system_unit: &system_unit,
                polkit_policy: b"new policy",
                polkit_policy_path: &polkit_policy,
            },
            &systemctl,
        )
        .unwrap_err();

        assert!(error.to_string().contains("previous installation restored"));
        assert_eq!(fs::read(&installed_executable).unwrap(), b"old executable");
        assert_eq!(
            fs::read(installed_static.join("index.html")).unwrap(),
            b"old static"
        );
        assert_eq!(fs::read(&system_unit).unwrap(), b"old unit");
        assert_eq!(fs::read(&polkit_policy).unwrap(), b"old policy");
        assert!(!system_config.exists());
        assert_eq!(
            systemctl.calls.lock().unwrap().as_slice(),
            [
                vec!["daemon-reload".to_string()],
                vec!["enable".to_string(), SERVICE_UNIT_NAME.to_string()],
                vec!["restart".to_string(), SERVICE_UNIT_NAME.to_string()],
                vec!["daemon-reload".to_string()],
                vec!["enable".to_string(), SERVICE_UNIT_NAME.to_string()],
                vec!["restart".to_string(), SERVICE_UNIT_NAME.to_string()],
            ]
        );
    }

    #[test]
    fn failed_first_install_removes_new_artifacts_and_disables_the_unit() {
        let root = tempfile::tempdir().unwrap();
        let source_root = root.path().join("source");
        let install_root = root.path().join("install");
        let source_static = source_root.join("static");
        let installed_static = install_root.join("static");
        let source_executable = source_root.join("server");
        let installed_executable = install_root.join("server");
        let source_config = source_root.join("config.toml");
        let system_config = root.path().join("etc/config.toml");
        let system_unit = root.path().join("systemd/service.service");
        let polkit_policy = root.path().join("polkit/action.policy");

        fs::create_dir_all(&source_static).unwrap();
        fs::write(&source_executable, b"new executable").unwrap();
        fs::write(source_static.join("index.html"), b"new static").unwrap();
        fs::write(&source_config, b"new config").unwrap();

        let systemctl = FailingRestartSystemctl {
            enabled: false,
            active: false,
            fail_restart_once: AtomicBool::new(true),
            calls: Mutex::new(Vec::new()),
        };
        let error = install_artifacts_transactionally(
            InstallArtifacts {
                source_executable: &source_executable,
                installed_executable: &installed_executable,
                source_static: Some(&source_static),
                installed_static: &installed_static,
                migration_source: Some(&source_config),
                system_config: &system_config,
                unit: b"new unit",
                system_unit: &system_unit,
                polkit_policy: b"new policy",
                polkit_policy_path: &polkit_policy,
            },
            &systemctl,
        )
        .unwrap_err();

        assert!(error.to_string().contains("previous installation restored"));
        assert!(!installed_executable.exists());
        assert!(!installed_static.exists());
        assert!(!system_config.exists());
        assert!(!system_unit.exists());
        assert!(!polkit_policy.exists());
        assert_eq!(
            systemctl.calls.lock().unwrap().as_slice(),
            [
                vec!["daemon-reload".to_string()],
                vec!["enable".to_string(), SERVICE_UNIT_NAME.to_string()],
                vec!["restart".to_string(), SERVICE_UNIT_NAME.to_string()],
                vec!["stop".to_string(), SERVICE_UNIT_NAME.to_string()],
                vec!["disable".to_string(), SERVICE_UNIT_NAME.to_string()],
                vec!["daemon-reload".to_string()],
            ]
        );
    }

    #[test]
    fn failed_uninstall_reload_restores_the_unit_and_previous_runtime_state() {
        let root = tempfile::tempdir().unwrap();
        let system_unit = root.path().join("systemd/service.service");
        let polkit_policy = root.path().join("polkit/action.policy");
        fs::create_dir_all(system_unit.parent().unwrap()).unwrap();
        fs::create_dir_all(polkit_policy.parent().unwrap()).unwrap();
        fs::write(&system_unit, b"old unit").unwrap();
        fs::write(&polkit_policy, b"old policy").unwrap();
        let systemctl = FailingReloadSystemctl {
            fail_reload_once: AtomicBool::new(true),
            calls: Mutex::new(Vec::new()),
        };

        let error = uninstall_service_transactionally(&system_unit, &polkit_policy, &systemctl)
            .unwrap_err();

        assert!(error.to_string().contains("previous installation restored"));
        assert_eq!(fs::read(&system_unit).unwrap(), b"old unit");
        assert_eq!(fs::read(&polkit_policy).unwrap(), b"old policy");
        assert_eq!(
            systemctl.calls.lock().unwrap().as_slice(),
            [
                vec![
                    "disable".to_string(),
                    "--now".to_string(),
                    SERVICE_UNIT_NAME.to_string(),
                ],
                vec!["daemon-reload".to_string()],
                vec!["daemon-reload".to_string()],
                vec!["enable".to_string(), SERVICE_UNIT_NAME.to_string()],
                vec!["restart".to_string(), SERVICE_UNIT_NAME.to_string()],
            ]
        );
    }
}
