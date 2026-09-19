//! Poll a PTY without allowing a surviving descendant to block result delivery.
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub(crate) fn duplicate(fd: Option<RawFd>) -> std::io::Result<OwnedFd> {
    let fd = fd.ok_or_else(|| std::io::Error::other("PTY has no native descriptor"))?;
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(copy) })
    }
}

pub(crate) fn readable(fd: &impl AsRawFd) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut descriptor, 1, 50) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    if descriptor.revents & libc::POLLNVAL != 0 {
        return Err(std::io::Error::from_raw_os_error(libc::EBADF));
    }
    Ok(result > 0)
}
