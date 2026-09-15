//! User workers never place recovery material or browser tokens in SYSTEM's
//! daemon directory. The daemon keeps its quota ledger at the configured root.
use desk_file_recovery::windows::PrivateDirectory;
use sha2::{Digest, Sha256};
use std::{
    io,
    path::{Path, PathBuf},
};
use windows::Win32::{
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
};

pub(super) fn prepare(configured_daemon_root: &Path) -> io::Result<PathBuf> {
    crate::file_recovery_service::platform_user::current()?;
    if !configured_daemon_root.is_absolute() {
        return Err(io::Error::other("worker data root must be absolute"));
    }
    // Query the process user's known folder; do not use a SYSTEM-inherited
    // LOCALAPPDATA environment variable or a path supplied by a model.
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None) }
        .map_err(io::Error::other)?;
    let converted = unsafe { raw.to_string() }
        .map(PathBuf::from)
        .map_err(io::Error::other);
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    let local = converted?;
    let app = PrivateDirectory::open_data_root(&local, "lcxl-remote-desk-worker")?;
    let configured = configured_daemon_root
        .to_str()
        .ok_or_else(|| io::Error::other("worker data root encoding unavailable"))?
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase();
    let key = format!("host-{:x}", Sha256::digest(configured.as_bytes()));
    let host = app.open_child(&key)?;
    host.validate()?;
    let result = local.join("lcxl-remote-desk-worker").join(key);
    // The recovery library independently reopens and validates every ancestor.
    let _vault = desk_file_recovery::Vault::open(&result)?;
    Ok(result)
}
