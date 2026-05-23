//! Integration tests over the public discovery API. Uses a temp dir
//! so we never touch the host's real `<exe_dir>/drivers/` layout.

use desk_virtual_display_driver_ops::{
    DRIVER_CAT_BASENAME, DRIVER_DLL_BASENAME, DRIVER_HW_ID, DRIVER_INF_BASENAME,
    DRIVER_WUDFRD_BASENAME, DriverFiles, discover_driver_files_in,
};
use std::fs::{create_dir_all, write};
use tempfile::tempdir;

#[test]
fn from_dir_builds_expected_basenames_without_filesystem_access() {
    let dir = std::path::PathBuf::from("nonexistent/anywhere");
    let files = DriverFiles::from_dir(dir.clone());
    assert_eq!(files.dir, dir);
    assert!(files.inf.ends_with(DRIVER_INF_BASENAME));
    assert!(files.cat.ends_with(DRIVER_CAT_BASENAME));
    assert!(files.dll.ends_with(DRIVER_DLL_BASENAME));
    assert!(files.wudfrd.ends_with(DRIVER_WUDFRD_BASENAME));
}

#[test]
fn discover_returns_none_when_directory_missing() {
    let tmp = tempdir().unwrap();
    assert!(discover_driver_files_in(tmp.path()).unwrap().is_none());
}

#[test]
fn discover_returns_none_when_one_file_missing() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path().join("drivers").join(DRIVER_HW_ID);
    create_dir_all(&dir).unwrap();
    write(dir.join(DRIVER_INF_BASENAME), b"").unwrap();
    write(dir.join(DRIVER_CAT_BASENAME), b"").unwrap();
    write(dir.join(DRIVER_DLL_BASENAME), b"").unwrap();
    // WUDFRD.dll deliberately absent.
    assert!(discover_driver_files_in(tmp.path()).unwrap().is_none());
}

#[test]
fn discover_returns_some_when_all_files_present() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path().join("drivers").join(DRIVER_HW_ID);
    create_dir_all(&dir).unwrap();
    write(dir.join(DRIVER_INF_BASENAME), b"").unwrap();
    write(dir.join(DRIVER_CAT_BASENAME), b"").unwrap();
    write(dir.join(DRIVER_DLL_BASENAME), b"").unwrap();
    write(dir.join(DRIVER_WUDFRD_BASENAME), b"").unwrap();
    let files = discover_driver_files_in(tmp.path())
        .unwrap()
        .expect("all four files present");
    assert!(files.inf.ends_with(DRIVER_INF_BASENAME));
    assert!(files.cat.ends_with(DRIVER_CAT_BASENAME));
    assert!(files.dll.ends_with(DRIVER_DLL_BASENAME));
    assert!(files.wudfrd.ends_with(DRIVER_WUDFRD_BASENAME));
    assert!(files.dir.ends_with(DRIVER_HW_ID));
}

#[cfg(not(target_os = "windows"))]
mod non_windows {
    use desk_virtual_display_driver_ops::{
        DriverFiles, InstallerError, install, query_install_status, uninstall_all,
    };

    #[test]
    fn query_status_reports_not_installed() {
        let st = query_install_status().unwrap();
        assert_eq!(st.installed, Some(false));
        assert_eq!(st.installed_oem_infs, Some(Vec::new()));
        assert!(!st.files_available);
    }

    #[test]
    fn install_returns_unsupported() {
        let files = DriverFiles::from_dir(std::path::PathBuf::from("/tmp/x"));
        let err = install(&files).unwrap_err();
        assert!(matches!(err, InstallerError::Unsupported));
    }

    #[test]
    fn uninstall_all_returns_unsupported() {
        let err = uninstall_all().unwrap_err();
        assert!(matches!(err, InstallerError::Unsupported));
    }
}
