//! Darwin metadata capture and private-store ACL enforcement through open handles.
use serde::Serialize;
use std::os::darwin::fs::MetadataExt as _;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::File,
    io,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
};
const ACL_TYPE_EXTENDED: c_int = 0x100;

/// Volume UUID survives a detach/remount; st_dev identifies only this mount.
pub(crate) fn volume_uuid(file: &File) -> io::Result<[u8; 16]> {
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut bytes = [0u8; 20];
    let result = unsafe {
        libc::fgetattrlist(
            file.as_raw_fd(),
            (&mut attrs as *mut libc::attrlist).cast(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if u32::from_ne_bytes(bytes[..4].try_into().unwrap()) != 20 {
        return Err(io::Error::other("Volume identity is unavailable"));
    }
    let uuid: [u8; 16] = bytes[4..].try_into().unwrap();
    if uuid == [0; 16] {
        return Err(io::Error::other("Volume identity is unavailable"));
    }
    Ok(uuid)
}
unsafe extern "C" {
    fn acl_init(count: c_int) -> *mut c_void;
    fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
    fn acl_set_fd_np(fd: c_int, acl: *mut c_void, kind: c_int) -> c_int;
    fn acl_to_text(acl: *mut c_void, size: *mut isize) -> *mut c_char;
    fn acl_free(value: *mut c_void) -> c_int;
}
struct Acl(*mut c_void);
impl Drop for Acl {
    fn drop(&mut self) {
        unsafe {
            acl_free(self.0);
        }
    }
}

pub fn make_private(file: &File) -> io::Result<()> {
    let acl = Acl(unsafe { acl_init(0) });
    if acl.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    if unsafe { acl_set_fd_np(file.as_raw_fd(), acl.0, ACL_TYPE_EXTENDED) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    created_seconds: i64,
    created_nanoseconds: i64,
    acl_text: Option<String>,
    xattrs_hex: std::collections::BTreeMap<String, String>,
}

/// Captures metadata before mutation; the returned manifest is not executable.
pub fn capture_metadata(file: &File, original_path: &str) -> io::Result<Vec<u8>> {
    let stat = file.metadata()?;
    let acl_ptr = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    let acl_text = if acl_ptr.is_null() {
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::ENOENT)) {
            return Err(error);
        }
        None
    } else {
        let acl = Acl(acl_ptr);
        let mut size = 0;
        let text = unsafe { acl_to_text(acl.0, &mut size) };
        if text.is_null() {
            return Err(io::Error::last_os_error());
        }
        let text_owner = Acl(text.cast());
        if size < 0 || size as usize > super::MAX_METADATA_BYTES {
            return Err(super::invalid("ACL metadata exceeds its bound"));
        }
        let value = unsafe { CStr::from_ptr(text_owner.0.cast()) }
            .to_str()
            .map_err(|_| super::invalid("ACL metadata is not UTF-8"))?
            .to_string();
        Some(value)
    };
    let count = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    if count as usize > super::MAX_METADATA_BYTES {
        return Err(super::invalid(
            "extended attribute names exceed their bound",
        ));
    }
    let mut names = vec![0u8; count as usize];
    let read =
        unsafe { libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len(), 0) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    names.truncate(read as usize);
    let mut xattrs_hex = std::collections::BTreeMap::new();
    let mut total = 0usize;
    for name in names.split(|b| *b == 0).filter(|name| !name.is_empty()) {
        let name_text = std::str::from_utf8(name)
            .map_err(|_| super::invalid("extended attribute name is not UTF-8"))?;
        let name =
            CString::new(name).map_err(|_| super::invalid("invalid extended attribute name"))?;
        let size = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        total = total.saturating_add((size as usize).saturating_mul(2));
        if total > super::MAX_METADATA_BYTES {
            return Err(super::invalid(
                "extended attribute content exceeds its bound",
            ));
        }
        let mut value = vec![0u8; size as usize];
        let read = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        value.truncate(read as usize);
        xattrs_hex.insert(
            name_text.to_owned(),
            value.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        );
    }
    let metadata = Metadata {
        original_path: original_path.into(),
        mode: stat.mode(),
        uid: stat.uid(),
        gid: stat.gid(),
        accessed_seconds: stat.atime(),
        accessed_nanoseconds: stat.atime_nsec(),
        modified_seconds: stat.mtime(),
        modified_nanoseconds: stat.mtime_nsec(),
        created_seconds: stat.st_birthtime(),
        created_nanoseconds: stat.st_birthtime_nsec(),
        acl_text,
        xattrs_hex,
    };
    let bytes = serde_json::to_vec(&metadata)?;
    if bytes.len() > super::MAX_METADATA_BYTES {
        return Err(super::invalid("file recovery metadata exceeds its bound"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_archive_preserves_acl_resource_fork_and_original_metadata() {
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.txt");
        std::fs::write(&path, b"before").unwrap();
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone allow read"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let file = File::open(&path).unwrap();
        let resource = [0u8, 255, 17, 42];
        assert_eq!(
            unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    c"com.apple.ResourceFork".as_ptr(),
                    resource.as_ptr().cast(),
                    resource.len(),
                    0,
                    0,
                )
            },
            0
        );
        let stat = file.metadata().unwrap();
        let metadata = capture_metadata(&file, path.to_str().unwrap()).unwrap();
        let captured: serde_json::Value = serde_json::from_slice(&metadata).unwrap();
        assert_eq!(captured["mode"], stat.mode());
        assert_eq!(captured["uid"], stat.uid());
        assert_eq!(captured["gid"], stat.gid());
        assert_eq!(captured["modified_seconds"], stat.mtime());
        assert_eq!(captured["modified_nanoseconds"], stat.mtime_nsec());
        assert_eq!(captured["created_seconds"], stat.st_birthtime());
        assert_eq!(captured["created_nanoseconds"], stat.st_birthtime_nsec());
        assert_eq!(captured["xattrs_hex"]["com.apple.ResourceFork"], "00ff112a");
        assert!(captured["acl_text"].as_str().unwrap().contains("allow"));
        let scope = crate::Scope {
            authority: "a".into(),
            device: "d".into(),
            os_user: "u".into(),
            owner: "o".into(),
        };
        std::fs::create_dir(root.path().join("private")).unwrap();
        let vault = crate::Vault::open(&root.path().join("private")).unwrap();
        let mut locked = vault.lock().unwrap();
        let record = locked
            .backup(crate::BackupRequest {
                scope: scope.clone(),
                conversation: "c",
                operation: "op",
                generation: "g",
                file_name: "source.txt",
                content: b"before",
                metadata: &metadata,
                now_ms: 1000,
            })
            .unwrap();
        assert!(record.bytes >= metadata.len() as u64 + 6);
        std::fs::write(&path, b"later").unwrap();
        let package = locked.export_package(&scope, &record.id, 1001).unwrap();
        let mut archive = zip::ZipArchive::new(io::Cursor::new(package)).unwrap();
        let mut text = String::new();
        archive
            .by_name("before.txt")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "before");
        let mut manifest = String::new();
        archive
            .by_name("metadata.json")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        let exported: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(exported["filesystem"], captured);
        assert_eq!(archive.len(), 2);
        assert_eq!(std::fs::read(&path).unwrap(), b"later");
    }

    #[test]
    fn captures_acl_and_binary_xattrs_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.txt");
        std::fs::write(&path, b"before").unwrap();
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone allow read"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let file = File::open(&path).unwrap();
        let value = [0u8, 255, 42];
        assert_eq!(
            unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    c"com.lcxl.recovery-test".as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            },
            0
        );
        let metadata: serde_json::Value =
            serde_json::from_slice(&capture_metadata(&file, path.to_str().unwrap()).unwrap())
                .unwrap();
        assert!(metadata["acl_text"].as_str().unwrap().contains("allow"));
        assert_eq!(metadata["xattrs_hex"]["com.lcxl.recovery-test"], "00ff2a");
        assert_eq!(metadata["uid"], unsafe { libc::geteuid() });
        assert!(metadata["created_seconds"].as_i64().unwrap() > 0);
        make_private(&file).unwrap();
        let private: serde_json::Value =
            serde_json::from_slice(&capture_metadata(&file, path.to_str().unwrap()).unwrap())
                .unwrap();
        assert!(
            !private["acl_text"]
                .as_str()
                .unwrap_or_default()
                .contains("allow")
        );
        assert_eq!(private["xattrs_hex"], metadata["xattrs_hex"]);
    }
}
