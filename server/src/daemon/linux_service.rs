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
const EXPERIMENTAL_INSTALL_ENV: &str = "LRD_EXPERIMENTAL_LINUX_SERVICE_DAEMON";

pub fn install_service(
    install_dir: &str,
    source_config: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if std::env::var(EXPERIMENTAL_INSTALL_ENV).as_deref() != Ok("1") {
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
    copy_file_atomically(&source_executable, &installed_executable, 0o755)?;

    let source_static = source_executable
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .join("static");
    if source_static.is_dir() {
        replace_static_tree(&source_static, &install_dir.join("static"))?;
    }

    if let Some(source_config) = migration_source {
        fs::create_dir_all(system_config.parent().expect("system config has parent"))?;
        copy_file_atomically(source_config, system_config, 0o600)?;
    }

    let unit = render_unit(&installed_executable, &install_dir);
    copy_bytes_atomically(unit.as_bytes(), Path::new(SYSTEM_UNIT_PATH), 0o644)?;
    run_systemctl(["daemon-reload"])?;
    run_systemctl(["enable", SERVICE_UNIT_NAME])?;
    run_systemctl(["restart", SERVICE_UNIT_NAME])?;
    Ok(())
}

pub fn uninstall_service() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    require_root()?;
    if Path::new(SYSTEM_UNIT_PATH).exists() {
        // Do not unlink a unit whose process we failed to stop: that would
        // leave an unmanageable root daemon running until reboot.
        run_systemctl(["disable", "--now", SERVICE_UNIT_NAME])?;
    }
    match fs::remove_file(SYSTEM_UNIT_PATH) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    run_systemctl(["daemon-reload"])?;
    Ok(())
}

fn require_root() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("Linux service management requires root".into());
    }
    Ok(())
}

fn validate_install_dir(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
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

fn validate_install_source(path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err("service install source is not a regular file".into());
    }
    if metadata.mode() & 0o022 != 0 {
        return Err("service install source is group/world-writable".into());
    }
    Ok(())
}

fn validate_config_source(path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

fn run_systemctl<const N: usize>(
    args: [&str; N],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let status = Command::new("systemctl").args(args).status()?;
    if !status.success() {
        return Err(format!("systemctl exited with {status}").into());
    }
    Ok(())
}

fn copy_file_atomically(source: &Path, target: &Path, mode: u32) -> io::Result<()> {
    let bytes = fs::read(source)?;
    copy_bytes_atomically(&bytes, target, mode)
}

fn copy_bytes_atomically(bytes: &[u8], target: &Path, mode: u32) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("service"),
        uuid::Uuid::new_v4()
    ));
    fs::write(&temporary, bytes)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
    fs::rename(&temporary, target)
}

fn replace_static_tree(source: &Path, target: &Path) -> io::Result<()> {
    let staging = target.with_extension(format!("staging-{}", uuid::Uuid::new_v4()));
    copy_directory(source, &staging)?;
    if target.exists() {
        let previous = target.with_extension(format!("previous-{}", uuid::Uuid::new_v4()));
        fs::rename(target, &previous)?;
        if let Err(error) = fs::rename(&staging, target) {
            let _ = fs::rename(&previous, target);
            return Err(error);
        }
        fs::remove_dir_all(previous)?;
    } else {
        fs::rename(staging, target)?;
    }
    Ok(())
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
        copy_bytes_atomically(b"secret = true\n", &target, 0o600).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"secret = true\n");
        assert_eq!(fs::metadata(target).unwrap().mode() & 0o777, 0o600);
    }
}
