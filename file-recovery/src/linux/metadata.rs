//! Capture and reproduce Linux security metadata through retained file handles.
use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::{collections::BTreeMap, ffi::CString, fs::File, io, os::fd::AsRawFd};

const LIMIT: usize = super::super::MAX_METADATA_BYTES;

fn invalid(reason: &str) -> io::Error {
    super::super::invalid(reason)
}

fn attributes(file: &File) -> io::Result<BTreeMap<String, Vec<u8>>> {
    let count = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    if count as usize > LIMIT {
        return Err(invalid("extended attribute list exceeds its bound"));
    }
    let mut names = vec![0u8; count as usize];
    let read =
        unsafe { libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len()) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    if read as usize != names.len() {
        return Err(invalid("extended attributes changed during capture"));
    }
    let mut output = BTreeMap::new();
    let mut total = names.len();
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let text = std::str::from_utf8(name)
            .map_err(|_| invalid("extended attribute name is not UTF-8"))?;
        let key = CString::new(name).map_err(|_| invalid("invalid extended attribute name"))?;
        let size =
            unsafe { libc::fgetxattr(file.as_raw_fd(), key.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        total = total.saturating_add(size as usize);
        if total > LIMIT / 4 {
            return Err(invalid(
                "extended attributes exceed recovery metadata budget",
            ));
        }
        let mut value = vec![0u8; size as usize];
        let read = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                key.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        if read as usize != value.len() {
            return Err(invalid("extended attribute changed during capture"));
        }
        output.insert(text.to_owned(), value);
    }
    Ok(output)
}

#[derive(Serialize)]
struct Metadata {
    original_path: String,
    mode: u32,
    uid: u32,
    gid: u32,
    accessed_seconds: i64,
    accessed_nanoseconds: i64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    xattrs: BTreeMap<String, Vec<u8>>,
}

pub fn capture_metadata(file: &File, original_path: &str) -> io::Result<Vec<u8>> {
    let stat = file.metadata()?;
    let metadata = Metadata {
        original_path: original_path.into(),
        mode: stat.mode() & 0o7777,
        uid: stat.uid(),
        gid: stat.gid(),
        accessed_seconds: stat.atime(),
        accessed_nanoseconds: stat.atime_nsec(),
        modified_seconds: stat.mtime(),
        modified_nanoseconds: stat.mtime_nsec(),
        xattrs: attributes(file)?,
    };
    let after = file.metadata()?;
    if (stat.ctime(), stat.ctime_nsec()) != (after.ctime(), after.ctime_nsec()) {
        return Err(invalid("file metadata changed during capture"));
    }
    let bytes = serde_json::to_vec(&metadata)?;
    if bytes.len() > LIMIT {
        return Err(invalid("file metadata exceeds recovery budget"));
    }
    Ok(bytes)
}

pub fn make_private(file: &File) -> io::Result<()> {
    for name in [c"system.posix_acl_access", c"system.posix_acl_default"] {
        if unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENODATA) {
                return Err(error);
            }
        }
    }
    let mode = if file.metadata()?.is_dir() {
        0o700
    } else {
        0o600
    };
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn copy_metadata(source: &File, target: &File) -> io::Result<()> {
    super::validate_source(source)?;
    let stat = source.metadata()?;
    let expected = attributes(source)?;
    if expected.contains_key("security.capability")
        || expected.contains_key("security.ima")
        || expected.contains_key("security.evm")
    {
        return Err(invalid(
            "file integrity or executable capability metadata cannot be preserved across a content change",
        ));
    }
    let target_stat = target.metadata()?;
    if (target_stat.uid(), target_stat.gid()) != (stat.uid(), stat.gid())
        && unsafe { libc::fchown(target.as_raw_fd(), stat.uid(), stat.gid()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fchmod(target.as_raw_fd(), stat.mode() & 0o777) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for name in attributes(target)?
        .keys()
        .filter(|name| !expected.contains_key(*name))
    {
        let name = CString::new(name.as_bytes()).map_err(|_| invalid("invalid attribute"))?;
        if unsafe { libc::fremovexattr(target.as_raw_fd(), name.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    for (name, value) in &expected {
        let name = CString::new(name.as_bytes()).map_err(|_| invalid("invalid attribute"))?;
        if unsafe {
            libc::fsetxattr(
                target.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let times = [
        libc::timespec {
            tv_sec: stat.atime(),
            tv_nsec: stat.atime_nsec(),
        },
        libc::timespec {
            tv_sec: stat.mtime(),
            tv_nsec: stat.mtime_nsec(),
        },
    ];
    if unsafe { libc::futimens(target.as_raw_fd(), times.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let after = target.metadata()?;
    if (after.uid(), after.gid(), after.mode() & 0o777)
        != (stat.uid(), stat.gid(), stat.mode() & 0o777)
        || attributes(target)? != expected
    {
        return Err(invalid(
            "staged security metadata does not match the source",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_keeps_binary_xattrs_and_owner() {
        let file = tempfile::tempfile().unwrap();
        let bytes = [0u8, 255, 42];
        assert_eq!(
            unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    c"user.lcxl-test".as_ptr(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    0,
                )
            },
            0
        );
        let metadata: serde_json::Value =
            serde_json::from_slice(&capture_metadata(&file, "/selected.txt").unwrap()).unwrap();
        assert_eq!(
            metadata["xattrs"]["user.lcxl-test"],
            serde_json::json!([0, 255, 42])
        );
        assert_eq!(metadata["uid"], unsafe { libc::geteuid() });
    }
}
