//! Private receipt storage resolved relative to held directory descriptors.
use std::{
    ffi::{CString, OsStr},
    fs::{File, Permissions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Component, Path},
};

pub(super) struct Directory {
    file: File,
}

impl Directory {
    pub(super) fn root(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::other("journal path must be absolute"));
        }
        // Resolve every component without following links. Subsequent operations
        // use openat on the held descriptor, even if an ancestor is renamed.
        let mut directory = Self {
            file: File::open("/")?,
        };
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => directory = directory.descend(name, true)?,
                _ => return Err(io::Error::other("invalid journal path component")),
            }
        }
        directory.protect()?;
        Ok(directory)
    }

    pub(super) fn child(&self, name: &OsStr, create: bool) -> io::Result<Self> {
        let directory = self.descend(name, create)?;
        directory.protect()?;
        Ok(directory)
    }

    fn descend(&self, name: &OsStr, create: bool) -> io::Result<Self> {
        let name = component(name)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mut raw = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags) };
        if raw < 0 && create && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            if unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) } != 0
                && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
            {
                return Err(io::Error::last_os_error());
            }
            self.sync()?;
            raw = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags) };
        }
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file: unsafe { File::from_raw_fd(raw) },
        })
    }

    fn protect(&self) -> io::Result<()> {
        if self.file.metadata()?.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::other(
                "journal directory belongs to another user",
            ));
        }
        self.file.set_permissions(Permissions::from_mode(0o700))
    }

    pub(super) fn file(&self, name: &str, create: bool) -> io::Result<File> {
        let name = component(OsStr::new(name))?;
        let flags = libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if create {
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
            } else {
                libc::O_RDONLY
            };
        let raw = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(raw) };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::other(
                "journal requires an owned regular file without aliases",
            ));
        }
        file.set_permissions(Permissions::from_mode(0o600))?;
        Ok(file)
    }

    pub(super) fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

fn component(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::other("invalid journal entry name"));
    }
    CString::new(bytes).map_err(io::Error::other)
}
