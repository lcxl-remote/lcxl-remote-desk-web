//! Resolve catalog roots from the actual non-root session user.
use super::catalog::CatalogCollection;
use std::{ffi::CStr, path::PathBuf};

pub(crate) struct CatalogHost {
    pub(crate) session_id: String,
    pub(crate) home: PathBuf,
    #[cfg(target_os = "linux")]
    pub(crate) user_name: String,
}

pub(crate) fn current() -> Result<CatalogHost, &'static str> {
    let uid = unsafe { libc::geteuid() };
    if uid == 0 || uid != unsafe { libc::getuid() } {
        return Err("application catalog requires a non-root user session");
    }
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0u8; 65536];
    let mut result = std::ptr::null_mut();
    if unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    } != 0
        || result.is_null()
        || entry.pw_dir.is_null()
    {
        return Err("user profile is unavailable");
    }
    use std::os::unix::ffi::OsStrExt;
    let home = PathBuf::from(std::ffi::OsStr::from_bytes(
        unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes(),
    ));
    if !home.is_absolute() {
        return Err("user home is not absolute");
    }
    #[cfg(target_os = "linux")]
    let session_id = {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("no desktop runtime directory")?;
        validate_runtime_directory(std::path::Path::new(&runtime), uid)?;
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("no registered graphical session environment");
        }
        let session =
            std::env::var("XDG_SESSION_ID").map_err(|_| "desktop session id unavailable")?;
        format!("{uid}:{session}")
    };
    #[cfg(target_os = "macos")]
    let session_id = format!("{uid}:{}", unsafe { libc::getsid(0) });
    #[cfg(target_os = "linux")]
    let user_name = if entry.pw_name.is_null() {
        return Err("user name is unavailable");
    } else {
        unsafe { CStr::from_ptr(entry.pw_name) }
            .to_str()
            .map_err(|_| "user name is not UTF-8")?
            .to_owned()
    };
    Ok(CatalogHost {
        session_id,
        home,
        #[cfg(target_os = "linux")]
        user_name,
    })
}

#[cfg(target_os = "linux")]
fn validate_runtime_directory(path: &std::path::Path, uid: u32) -> Result<(), &'static str> {
    use std::os::unix::fs::MetadataExt;
    if !path.is_absolute() {
        return Err("desktop runtime directory is not absolute");
    }
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| "desktop runtime directory unavailable")?;
    if metadata.uid() != uid || !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
        return Err("desktop runtime directory identity mismatch");
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::validate_runtime_directory;

    #[test]
    fn runtime_directory_must_not_depend_on_worker_cwd() {
        assert_eq!(
            validate_runtime_directory(std::path::Path::new("."), unsafe { libc::geteuid() }),
            Err("desktop runtime directory is not absolute")
        );
    }
}

impl CatalogHost {
    pub(crate) fn enumerate(&self) -> CatalogCollection {
        #[cfg(target_os = "macos")]
        {
            super::catalog::macos::enumerate(&self.home)
        }
        #[cfg(target_os = "linux")]
        {
            let split_paths = |name: &str| {
                std::env::var_os(name)
                    .map(|value| std::env::split_paths(&value).collect())
                    .unwrap_or_default()
            };
            super::catalog::linux::enumerate(&super::catalog::linux::DesktopEnvironment {
                home: self.home.clone(),
                data_home: std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
                data_dirs: split_paths("XDG_DATA_DIRS"),
                executable_dirs: split_paths("PATH"),
                current_desktops: std::env::var("XDG_CURRENT_DESKTOP")
                    .unwrap_or_default()
                    .split(':')
                    .map(str::to_owned)
                    .collect(),
                locale: ["LC_ALL", "LC_MESSAGES", "LANG"]
                    .into_iter()
                    .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
                    .unwrap_or_default(),
            })
        }
    }
}
