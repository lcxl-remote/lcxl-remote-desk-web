use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const APP_DIR_UNIX: &str = "lcxl-remote-desk";
#[cfg(target_os = "windows")]
const APP_DIR_WINDOWS: &str = "LCXL Remote Desktop";
#[cfg(target_os = "macos")]
const APP_ID_MACOS: &str = "com.lcxl.remote-desk";
const REMOTE_ACCESS_STATE_FILE: &str = "remote-access-state.toml";
const LOCAL_ACCESS_SOCKET_FILE: &str = "remote-access-control.sock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostDataScope {
    Machine,
    System,
    User,
}

impl HostDataScope {
    pub fn current() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::Machine
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: geteuid has no preconditions and cannot fail.
            if unsafe { libc::geteuid() } == 0 {
                Self::System
            } else {
                Self::User
            }
        }
        #[cfg(target_os = "macos")]
        {
            Self::User
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
        {
            Self::User
        }
    }
}

#[derive(Debug)]
pub enum HostDataPathError {
    MissingAbsolutePath(&'static str),
    UnsupportedScope {
        platform: &'static str,
        scope: HostDataScope,
    },
    MissingConfigParent(PathBuf),
    Io(io::Error),
}

impl fmt::Display for HostDataPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingAbsolutePath(name) => {
                write!(formatter, "{name} is missing or is not an absolute path")
            }
            Self::UnsupportedScope { platform, scope } => {
                write!(
                    formatter,
                    "{scope:?} host-data scope is unsupported on {platform}"
                )
            }
            Self::MissingConfigParent(path) => {
                write!(formatter, "config path has no parent: {}", path.display())
            }
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for HostDataPathError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for HostDataPathError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDataPaths {
    scope: HostDataScope,
    explicit_config: bool,
    profile_identity: String,
    config_file: PathBuf,
    config_lock_file: PathBuf,
    data_root: PathBuf,
    runtime_root: Option<PathBuf>,
    log_dir: PathBuf,
    signal_db_dir: PathBuf,
    exec_ledger_dir: PathBuf,
    remote_access_state_file: PathBuf,
    local_access_endpoint: Option<PathBuf>,
}

impl HostDataPaths {
    pub fn resolve(
        scope: HostDataScope,
        config_override: Option<&Path>,
    ) -> Result<Self, HostDataPathError> {
        let defaults = Self::platform_defaults(scope)?;
        match config_override {
            Some(path) => Self::from_explicit(scope, path, defaults.log_dir),
            None => Ok(defaults),
        }
    }

    pub fn resolve_current(config_override: Option<&Path>) -> Result<Self, HostDataPathError> {
        Self::resolve(HostDataScope::current(), config_override)
    }

    pub fn for_test(root: impl AsRef<Path>) -> Result<Self, HostDataPathError> {
        let root = absolute_path(root.as_ref())?;
        let config_file = root.join("config").join("config.toml");
        Self::assemble(
            HostDataScope::User,
            false,
            config_file,
            root.join("data"),
            Some(root.join("run")),
            root.join("logs"),
            root.join("data"),
            root.join("data"),
            root.join("data").join(REMOTE_ACCESS_STATE_FILE),
            Some(root.join("run").join(LOCAL_ACCESS_SOCKET_FILE)),
        )
    }

    pub fn ensure_directories(&self) -> Result<(), HostDataPathError> {
        let config_parent = self
            .config_file
            .parent()
            .ok_or_else(|| HostDataPathError::MissingConfigParent(self.config_file.clone()))?;
        create_private_directory(config_parent)?;
        create_private_directory(&self.data_root)?;
        if let Some(runtime_root) = &self.runtime_root {
            create_private_directory(runtime_root)?;
        }
        fs::create_dir_all(&self.log_dir)?;
        Ok(())
    }

    pub fn scope(&self) -> HostDataScope {
        self.scope
    }

    pub fn is_explicit(&self) -> bool {
        self.explicit_config
    }

    pub fn explicit_config_file(&self) -> Option<&Path> {
        self.explicit_config.then_some(self.config_file.as_path())
    }

    pub fn profile_identity(&self) -> &str {
        &self.profile_identity
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub fn config_lock_file(&self) -> &Path {
        &self.config_lock_file
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn runtime_root(&self) -> Option<&Path> {
        self.runtime_root.as_deref()
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub fn signal_db_dir(&self) -> &Path {
        &self.signal_db_dir
    }

    pub fn exec_ledger_dir(&self) -> &Path {
        &self.exec_ledger_dir
    }

    pub fn remote_access_state_file(&self) -> &Path {
        &self.remote_access_state_file
    }

    pub fn local_access_endpoint(&self) -> Option<&Path> {
        self.local_access_endpoint.as_deref()
    }

    fn from_explicit(
        scope: HostDataScope,
        config_override: &Path,
        log_dir: PathBuf,
    ) -> Result<Self, HostDataPathError> {
        let mut config_file = absolute_path(config_override)?;
        config_file.set_extension("toml");
        let parent = config_file
            .parent()
            .ok_or_else(|| HostDataPathError::MissingConfigParent(config_file.clone()))?
            .to_path_buf();

        #[cfg(target_os = "windows")]
        let runtime_root = None;
        #[cfg(not(target_os = "windows"))]
        let runtime_root = Some(parent.clone());

        Self::assemble(
            scope,
            true,
            config_file,
            parent.clone(),
            runtime_root.clone(),
            log_dir,
            parent.clone(),
            parent.clone(),
            parent.join(REMOTE_ACCESS_STATE_FILE),
            runtime_root.map(|root| root.join(LOCAL_ACCESS_SOCKET_FILE)),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        scope: HostDataScope,
        explicit_config: bool,
        config_file: PathBuf,
        data_root: PathBuf,
        runtime_root: Option<PathBuf>,
        log_dir: PathBuf,
        signal_db_dir: PathBuf,
        exec_ledger_dir: PathBuf,
        remote_access_state_file: PathBuf,
        local_access_endpoint: Option<PathBuf>,
    ) -> Result<Self, HostDataPathError> {
        for (name, path) in [
            ("config file", config_file.as_path()),
            ("data root", data_root.as_path()),
            ("log directory", log_dir.as_path()),
            ("signal database directory", signal_db_dir.as_path()),
            ("execution ledger directory", exec_ledger_dir.as_path()),
            (
                "remote-access state file",
                remote_access_state_file.as_path(),
            ),
        ] {
            require_absolute(name, path)?;
        }
        if let Some(path) = runtime_root.as_deref() {
            require_absolute("runtime root", path)?;
        }
        if let Some(path) = local_access_endpoint.as_deref() {
            require_absolute("local-access endpoint", path)?;
        }

        let mut config_lock_file = config_file.clone();
        config_lock_file.set_extension("locale.lock");
        let profile_identity = profile_identity(&config_file);

        Ok(Self {
            scope,
            explicit_config,
            profile_identity,
            config_file,
            config_lock_file,
            data_root,
            runtime_root,
            log_dir,
            signal_db_dir,
            exec_ledger_dir,
            remote_access_state_file,
            local_access_endpoint,
        })
    }

    #[cfg(target_os = "windows")]
    fn platform_defaults(scope: HostDataScope) -> Result<Self, HostDataPathError> {
        if scope != HostDataScope::Machine {
            return Err(HostDataPathError::UnsupportedScope {
                platform: "windows",
                scope,
            });
        }
        let root = windows_program_data()?.join(APP_DIR_WINDOWS);
        Self::assemble(
            scope,
            false,
            root.join("config").join("config.toml"),
            root.join("data"),
            None,
            root.join("logs"),
            root.join("data"),
            root.join("data"),
            root.join("data").join(REMOTE_ACCESS_STATE_FILE),
            None,
        )
    }

    #[cfg(target_os = "linux")]
    fn platform_defaults(scope: HostDataScope) -> Result<Self, HostDataPathError> {
        match scope {
            HostDataScope::System => Self::assemble(
                scope,
                false,
                PathBuf::from("/etc/lcxl-remote-desk/config.toml"),
                PathBuf::from("/var/lib/lcxl-remote-desk"),
                Some(PathBuf::from("/run/lcxl-remote-desk")),
                PathBuf::from("/var/log/lcxl-remote-desk"),
                PathBuf::from("/var/lib/lcxl-remote-desk"),
                PathBuf::from("/var/lib/lcxl-remote-desk"),
                PathBuf::from("/var/lib/lcxl-remote-desk").join(REMOTE_ACCESS_STATE_FILE),
                Some(PathBuf::from("/run/lcxl-remote-desk").join(LOCAL_ACCESS_SOCKET_FILE)),
            ),
            HostDataScope::User => {
                let home = absolute_environment_path("HOME");
                let config_home = absolute_environment_path("XDG_CONFIG_HOME")
                    .or_else(|| home.as_ref().map(|path| path.join(".config")));
                let state_home = absolute_environment_path("XDG_STATE_HOME")
                    .or_else(|| home.as_ref().map(|path| path.join(".local").join("state")));
                let config_home = config_home.ok_or(HostDataPathError::MissingAbsolutePath(
                    "XDG_CONFIG_HOME or HOME",
                ))?;
                let data_root = state_home
                    .ok_or(HostDataPathError::MissingAbsolutePath(
                        "XDG_STATE_HOME or HOME",
                    ))?
                    .join(APP_DIR_UNIX);
                let runtime_root = absolute_environment_path("XDG_RUNTIME_DIR")
                    .map(|path| path.join(APP_DIR_UNIX))
                    .unwrap_or_else(|| data_root.join("run"));
                Self::assemble(
                    scope,
                    false,
                    config_home.join(APP_DIR_UNIX).join("config.toml"),
                    data_root.clone(),
                    Some(runtime_root.clone()),
                    data_root.join("logs"),
                    data_root.clone(),
                    data_root.clone(),
                    data_root.join(REMOTE_ACCESS_STATE_FILE),
                    Some(runtime_root.join(LOCAL_ACCESS_SOCKET_FILE)),
                )
            }
            HostDataScope::Machine => Err(HostDataPathError::UnsupportedScope {
                platform: "linux",
                scope,
            }),
        }
    }

    #[cfg(target_os = "macos")]
    fn platform_defaults(scope: HostDataScope) -> Result<Self, HostDataPathError> {
        if scope != HostDataScope::User {
            return Err(HostDataPathError::UnsupportedScope {
                platform: "macos",
                scope,
            });
        }
        let home = absolute_environment_path("HOME")
            .ok_or(HostDataPathError::MissingAbsolutePath("HOME"))?;
        let application_support = home
            .join("Library")
            .join("Application Support")
            .join(APP_ID_MACOS);
        let data_root = application_support.join("data");
        let runtime_root = data_root.join("run");
        Self::assemble(
            scope,
            false,
            application_support.join("config").join("config.toml"),
            data_root.clone(),
            Some(runtime_root.clone()),
            home.join("Library").join("Logs").join(APP_DIR_UNIX),
            data_root.clone(),
            data_root.clone(),
            data_root.join(REMOTE_ACCESS_STATE_FILE),
            Some(runtime_root.join(LOCAL_ACCESS_SOCKET_FILE)),
        )
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    fn platform_defaults(scope: HostDataScope) -> Result<Self, HostDataPathError> {
        Err(HostDataPathError::UnsupportedScope {
            platform: std::env::consts::OS,
            scope,
        })
    }
}

fn require_absolute(name: &'static str, path: &Path) -> Result<(), HostDataPathError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(HostDataPathError::MissingAbsolutePath(name))
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, HostDataPathError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::path::absolute(path)?)
}

fn absolute_environment_path(name: &'static str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

fn profile_identity(path: &Path) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        for byte in path.as_os_str().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        for word in path.as_os_str().encode_wide() {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        for byte in path.to_string_lossy().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

fn create_private_directory(path: &Path) -> Result<(), HostDataPathError> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_program_data() -> Result<PathBuf, HostDataPathError> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    // SAFETY: the known-folder id and flags are valid, and no access token is
    // supplied. Windows allocates the returned string with CoTaskMemAlloc.
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None) }
        .map_err(|error| io::Error::other(error.to_string()))?;
    // SAFETY: SHGetKnownFolderPath returns a valid NUL-terminated UTF-16 string.
    let converted = unsafe { raw.to_string() }
        .map(PathBuf::from)
        .map_err(|error| io::Error::other(error.to_string()));
    // SAFETY: raw was allocated by SHGetKnownFolderPath and is released exactly once.
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    let path = converted?;
    require_absolute("ProgramData known folder", &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_override_is_absolute_and_normalized_once() {
        let root = std::env::temp_dir().join(format!("host-data-paths-{}", std::process::id()));
        let paths = HostDataPaths::resolve(HostDataScope::User, Some(&root.join("profile")))
            .expect("explicit paths");
        assert_eq!(paths.config_file(), root.join("profile.toml"));
        assert_eq!(paths.data_root(), root);
        assert!(paths.is_explicit());
        assert_eq!(paths.explicit_config_file(), Some(paths.config_file()));
    }

    #[test]
    fn test_layout_separates_config_data_runtime_and_logs() {
        let root =
            std::env::temp_dir().join(format!("host-data-paths-layout-{}", std::process::id()));
        let paths = HostDataPaths::for_test(&root).expect("test paths");
        assert_eq!(paths.config_file(), root.join("config/config.toml"));
        assert_eq!(paths.signal_db_dir(), root.join("data"));
        assert_eq!(paths.exec_ledger_dir(), root.join("data"));
        assert_eq!(
            paths.remote_access_state_file(),
            root.join("data/remote-access-state.toml")
        );
        assert_eq!(
            paths.local_access_endpoint(),
            Some(root.join("run/remote-access-control.sock").as_path())
        );
        assert!(!paths.is_explicit());
    }

    #[test]
    fn profile_identity_changes_with_config_path() {
        let first =
            HostDataPaths::for_test(std::env::temp_dir().join("host-profile-first")).unwrap();
        let second =
            HostDataPaths::for_test(std::env::temp_dir().join("host-profile-second")).unwrap();
        assert_ne!(first.profile_identity(), second.profile_identity());
    }
}
