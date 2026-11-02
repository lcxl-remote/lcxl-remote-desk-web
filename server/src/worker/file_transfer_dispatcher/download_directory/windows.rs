use std::{io, path::PathBuf};
use windows::Win32::{
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
};

pub(super) fn current() -> io::Result<PathBuf> {
    // SAFETY: the worker runs under the interactive user's token. Release COM's allocation after copying.
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None) }
        .map_err(io::Error::other)?;
    let result = unsafe { raw.to_string() }
        .map(PathBuf::from)
        .map_err(io::Error::other);
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    result
}
