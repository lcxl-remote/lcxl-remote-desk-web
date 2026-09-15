//! One private Office workspace per OS user, shared across desktop sessions.
use desk_file_recovery::windows::PrivateDirectory;
use std::{os::windows::io::AsRawHandle, path::PathBuf};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{GETFINALPATHNAMEBYHANDLE_FLAGS, GetFinalPathNameByHandleW},
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessRecord {
    pub version: u32,
    pub pid: u32,
    pub created: u64,
}

pub(super) fn open() -> anyhow::Result<(PathBuf, PrivateDirectory)> {
    crate::file_recovery_service::platform_user::current()?;
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None) }?;
    let path = unsafe { raw.to_string() }.map(PathBuf::from);
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    let base = path?;
    let root = PrivateDirectory::open_data_root(&base, "lcxl-remote-desk-office")?;
    // A packaged launcher may redirect LocalAppData into its private cache.
    // Excel is an independent COM server and does not inherit that namespace.
    // Use the actual pinned directory's path, never reconstruct the alias.
    let mut buffer = vec![0u16; 32768];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            HANDLE(root.handle().as_raw_handle()),
            &mut buffer,
            GETFINALPATHNAMEBYHANDLE_FLAGS(0),
        )
    } as usize;
    anyhow::ensure!(
        length > 0 && length < buffer.len(),
        "Office workspace physical path unavailable"
    );
    let physical = String::from_utf16(&buffer[..length])?;
    let path = physical
        .strip_prefix(r"\\?\")
        .ok_or_else(|| anyhow::anyhow!("Office workspace has no DOS path"))?;
    anyhow::ensure!(
        path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && path.as_bytes().get(1..3) == Some(b":\\"),
        "Office workspace must be on a local drive"
    );
    root.validate()?;
    Ok((PathBuf::from(path), root))
}

/// Caller holds root.try_lock() throughout this inventory and any activation.
pub(super) fn ensure_no_inflight(
    path: &std::path::Path,
    root: &PrivateDirectory,
) -> anyhow::Result<()> {
    for (index, entry) in std::fs::read_dir(path)?.enumerate() {
        anyhow::ensure!(index < 65, "Excel private workspace cleanup is required");
        let entry = entry?;
        if entry.file_name() == "lock" {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid Office workspace name"))?;
        anyhow::ensure!(
            uuid::Uuid::parse_str(&name).is_ok(),
            "unexpected Office workspace entry"
        );
        let child = root.open_child(&name)?;
        child.validate()?;
        if entry.path().join("inflight").try_exists()? {
            let lock = child
                .try_lock()?
                .ok_or_else(|| anyhow::anyhow!("Office workspace is still busy"))?;
            let record: ProcessRecord = serde_json::from_slice(&lock.read("process.json", 1024)?)?;
            anyhow::ensure!(
                record.version == 1 && record.pid != 0 && record.created != 0,
                "invalid owned Excel process record"
            );
            anyhow::ensure!(
                super::native::recorded_process_exited(&record)?,
                "previous Excel activation or shutdown remains unresolved"
            );
            drop(lock);
        }
        // The root lock excludes every helper. A missing inflight marker means
        // activation has not started or shutdown has already been confirmed;
        // otherwise the process proof above is required before any cleanup.
        // Retry markerless leftovers too, including interrupted normal cleanup.
        remove_fixed_files(&entry.path())?;
        drop(child);
        std::fs::remove_dir(entry.path())?;
    }
    Ok(())
}

pub(super) fn remove_fixed_files(directory: &std::path::Path) -> anyhow::Result<()> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows::Win32::{Foundation::HANDLE, Storage::FileSystem::*};
    for name in [
        "input.xlsx",
        "calculated.xlsx",
        "lock",
        "inflight",
        // Keep the process proof until the unresolved marker is gone. A failed
        // deletion must not strand an inflight workspace without its proof.
        "process.json",
    ] {
        let file = match std::fs::OpenOptions::new()
            .access_mode((FILE_READ_ATTRIBUTES | DELETE).0)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(directory.join(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        desk_file_recovery::windows::file_identity(
            &file,
            desk_file_recovery::windows::FileKind::File,
        )?;
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        unsafe {
            SetFileInformationByHandle(
                HANDLE(file.as_raw_handle()),
                FileDispositionInfo,
                (&info as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        }?;
        drop(file);
    }
    Ok(())
}

pub(super) fn available() -> anyhow::Result<()> {
    let (path, root) = open()?;
    let _lock = root
        .try_lock()?
        .ok_or_else(|| anyhow::anyhow!("another user-session Excel calculation is running"))?;
    ensure_no_inflight(&path, &root)
}
