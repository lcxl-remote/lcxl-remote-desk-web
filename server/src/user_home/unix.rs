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
