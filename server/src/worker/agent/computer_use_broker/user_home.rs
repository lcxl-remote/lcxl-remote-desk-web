//! Resolve the Home directory from the interactive worker's native user
//! identity. Model input and inherited service environment variables are not
//! accepted as path authority.

use std::path::Path;

fn bounded_absolute(path: &Path) -> Option<String> {
    let value = path.to_str()?;
    (path.is_absolute()
        && path.is_dir()
        && !value.is_empty()
        && value.len() <= 4096
        && !value.chars().any(char::is_control))
    .then(|| value.to_owned())
}

#[cfg(windows)]
pub(super) fn current() -> Option<String> {
    windows::current()
}

#[cfg(unix)]
pub(super) fn current() -> Option<String> {
    unix::current()
}

#[cfg(not(any(windows, unix)))]
pub(super) fn current() -> Option<String> {
    None
}

#[cfg(windows)]
mod windows {
    use super::bounded_absolute;
    use std::path::PathBuf;
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_Profile, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
    };

    pub(super) fn current() -> Option<String> {
        let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_Profile, KF_FLAG_DEFAULT, None) }.ok()?;
        let converted = unsafe { raw.to_string() }.ok().map(PathBuf::from);
        unsafe { CoTaskMemFree(Some(raw.0.cast())) };
        bounded_absolute(converted?.as_path())
    }
}

#[cfg(unix)]
mod unix {
    use super::bounded_absolute;
    use std::{ffi::CStr, path::PathBuf};

    pub(super) fn current() -> Option<String> {
        let uid = unsafe { libc::geteuid() };
        if uid != unsafe { libc::getuid() } {
            return None;
        }
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buffer = vec![0_u8; 65_536];
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
            return None;
        }
        use std::os::unix::ffi::OsStrExt;
        let home = PathBuf::from(std::ffi::OsStr::from_bytes(
            unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes(),
        ));
        bounded_absolute(&home)
    }
}

#[cfg(test)]
mod tests {
    use super::bounded_absolute;

    #[test]
    fn home_must_be_absolute_and_safe_for_runtime_projection() {
        assert!(bounded_absolute(std::path::Path::new("relative/home")).is_none());
        let current = std::env::current_dir().unwrap();
        assert_eq!(
            bounded_absolute(&current),
            current.to_str().map(ToOwned::to_owned)
        );
        let missing = current.join(format!("missing-home-{}", uuid::Uuid::new_v4()));
        assert!(bounded_absolute(&missing).is_none());
    }
}
