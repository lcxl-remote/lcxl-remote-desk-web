use std::{ffi::CString, io, os::unix::ffi::OsStrExt, path::Path};

pub(super) fn publish(temporary: &Path, target: &Path) -> io::Result<()> {
    let from = CString::new(temporary.as_os_str().as_bytes())?;
    let to = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: both C strings remain alive throughout the synchronous syscall.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
