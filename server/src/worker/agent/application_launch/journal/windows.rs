//! Hold no-delete directory handles and protect receipts with an explicit user DACL.
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            SetKernelObjectSecurity,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, WRITE_DAC,
        },
    },
    core::PCWSTR,
};

pub(super) struct Directory {
    path: PathBuf,
    // Keep every ancestor pinned until all operations using this path finish.
    guards: Vec<Arc<File>>,
    sid: String,
}

impl Directory {
    pub(super) fn root(path: &Path) -> io::Result<Self> {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(io::Error::other(
                "journal path must be absolute and normalized",
            ));
        }
        let sid = super::super::windows_process::storage_user_sid()
            .map_err(|_| io::Error::other("journal requires a user identity"))?;
        let mut guards = Vec::new();
        let ancestors: Vec<_> = path.ancestors().collect();
        for ancestor in ancestors.into_iter().rev() {
            let private = ancestor == path;
            let file = directory(ancestor, private, true)?;
            if private {
                protect(&file, &sid, true)?;
            }
            guards.push(Arc::new(file));
        }
        Ok(Self {
            path: path.into(),
            guards,
            sid,
        })
    }

    pub(super) fn child(&self, name: &OsStr, create: bool) -> io::Result<Self> {
        if !matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        ) {
            return Err(io::Error::other("invalid journal entry name"));
        }
        let path = self.path.join(name);
        let file = directory(&path, true, create)?;
        protect(&file, &self.sid, true)?;
        let mut guards = self.guards.clone();
        guards.push(Arc::new(file));
        Ok(Self {
            path,
            guards,
            sid: self.sid.clone(),
        })
    }

    pub(super) fn file(&self, name: &str, create: bool) -> io::Result<File> {
        if !matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        ) || name.contains(':')
        {
            return Err(io::Error::other("invalid journal entry name"));
        }
        // GENERIC_READ / GENERIC_WRITE are combined with WRITE_DAC so every
        // existing record also receives the explicit protected user-only ACL.
        let access = if create { 0x40000000 } else { 0x80000000 };
        let file = OpenOptions::new()
            .read(!create)
            .write(create)
            .access_mode(access | WRITE_DAC.0)
            .create_new(create)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(self.path.join(name))?;
        validate(&file, false)?;
        protect(&file, &self.sid, false)?;
        Ok(file)
    }

    pub(super) fn sync(&self) -> io::Result<()> {
        // Each append is flushed with File::sync_all before native invocation.
        // Windows does not provide a general unprivileged directory fsync API.
        Ok(())
    }
}

fn directory(path: &Path, private: bool, create: bool) -> io::Result<File> {
    let open = || {
        OpenOptions::new()
            // Attribute-only access does not participate in share-access checks.
            // Directory read access makes omission of FILE_SHARE_DELETE pin it.
            .access_mode(
                FILE_LIST_DIRECTORY.0
                    | FILE_READ_ATTRIBUTES.0
                    | if private { WRITE_DAC.0 } else { 0 },
            )
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(path)
    };
    let file = match open() {
        Ok(file) => file,
        Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            open()?
        }
        Err(error) => return Err(error),
    };
    validate(&file, true)?;
    Ok(file)
}

fn validate(file: &File, directory: bool) -> io::Result<()> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
        .map_err(io::Error::other)?;
    let metadata = file.metadata()?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file() || information.nNumberOfLinks != 1
        }
    {
        return Err(io::Error::other(
            "journal requires an ordinary filesystem entry without aliases",
        ));
    }
    Ok(())
}

fn protect(file: &File, sid: &str, inherit: bool) -> io::Result<()> {
    let flags = if inherit { "OICI" } else { "" };
    let descriptor: Vec<u16> = format!("D:P(A;{flags};FA;;;{sid})(A;{flags};FA;;;SY)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut security = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(descriptor.as_ptr()),
            1,
            &mut security,
            None,
        )
        .map_err(io::Error::other)?;
        let result = SetKernelObjectSecurity(
            HANDLE(file.as_raw_handle()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            security,
        )
        .map_err(io::Error::other);
        let _ = LocalFree(Some(HLOCAL(security.0)));
        result
    }
}
