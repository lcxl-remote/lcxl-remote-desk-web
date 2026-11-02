use objc2_foundation::{
    NSSearchPathDirectory, NSSearchPathDomainMask, NSSearchPathForDirectoriesInDomains,
};
use std::{io, path::PathBuf};

pub(super) fn current() -> io::Result<PathBuf> {
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: Foundation returns an autoreleased array valid for this pool.
        let paths = unsafe {
            NSSearchPathForDirectoriesInDomains(
                NSSearchPathDirectory::NSDownloadsDirectory,
                NSSearchPathDomainMask::NSUserDomainMask,
                objc2::runtime::Bool::new(true),
            )
        };
        unsafe { paths.as_ref().firstObject() }
            .map(|path| PathBuf::from(path.to_string()))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "interactive user's downloads directory is unavailable",
                )
            })
    })
}
