//! Blocking native client shared by local consent frontends. It grants no consent.
use super::{MAX_MESSAGE, Request, Response};
use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::Path,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputControlReport {
    pub grabbed: usize,
    pub failed: usize,
    pub skipped: usize,
}

/// Keep alive only for the locally approved operation period. Dropping it closes
/// the connection; a successful finish receipt confirms the worker released it.
pub struct NativeInputControlClient {
    stream: UnixStream,
    report: InputControlReport,
}

/// A native frontend can interrupt a blocking wait from its UI thread. Dropping
/// this handle also disconnects, so frontend shutdown cannot orphan its period.
pub struct InputControlCancellation(UnixStream);
impl InputControlCancellation {
    /// Request graceful release; the owning waiter still verifies the Ended ack.
    pub fn request_stop(&self) -> io::Result<()> {
        let mut stream = &self.0;
        stream.write_all(&[0])
    }

    pub fn cancel(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}
impl Drop for InputControlCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl NativeInputControlClient {
    /// Call after explicit local consent, on a blocking thread, with all keys
    /// released. Does not elevate, change permissions, or retry failed requests.
    pub fn connect(path: &Path, seconds: u16, accept_partial: bool) -> io::Result<Self> {
        let request = Request {
            version: 1,
            duration_secs: seconds,
            accept_partial,
        };
        request.validate()?;
        let uid = unsafe { libc::geteuid() };
        if uid == 0 || uid != unsafe { libc::getuid() } {
            return Err(io::Error::other(
                "Run as the ordinary desktop user, not root",
            ));
        }
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::other(
                "Expected a private socket owned by the desktop user",
            ));
        }
        let mut stream = UnixStream::connect(path)?;
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut size,
            )
        };
        if result != 0 || size as usize != std::mem::size_of::<libc::ucred>() || cred.uid != uid {
            return Err(io::Error::other("Control worker identity mismatch"));
        }
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let bytes = serde_json::to_vec(&request)?;
        stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        stream.write_all(&bytes)?;
        let report = admit(read(&mut stream)?, accept_partial)?;
        stream.set_read_timeout(Some(Duration::from_secs(u64::from(seconds) + 5)))?;
        Ok(Self { stream, report })
    }

    pub fn report(&self) -> InputControlReport {
        self.report
    }

    pub fn cancellation(&self) -> io::Result<InputControlCancellation> {
        self.stream.try_clone().map(InputControlCancellation)
    }

    /// Wait for release after local escape, expiry or worker revocation. An EOF
    /// or transport error is not a release acknowledgement; never replay start.
    pub fn wait(mut self) -> io::Result<()> {
        ended(read(&mut self.stream)?)
    }

    /// Explicit stop, waiting for the release acknowledgement with a short bound.
    pub fn finish(mut self) -> io::Result<()> {
        self.stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        self.stream.write_all(&[0])?;
        ended(read(&mut self.stream)?)
    }
}
impl Drop for NativeInputControlClient {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}
fn ended(response: Response) -> io::Result<()> {
    match response {
        Response::Ended => Ok(()),
        _ => Err(io::Error::other("Missing input release acknowledgement")),
    }
}
fn admit(response: Response, accept_partial: bool) -> io::Result<InputControlReport> {
    match response {
        Response::Active {
            grabbed,
            failed,
            skipped,
        } if grabbed > 0
            && grabbed.saturating_add(failed).saturating_add(skipped) <= 128
            && (accept_partial || failed == 0) =>
        {
            Ok(InputControlReport {
                grabbed,
                failed,
                skipped,
            })
        }
        Response::Unavailable { reason } => Err(io::Error::other(reason)),
        _ => Err(io::Error::other(
            "Invalid or already ended input control admission",
        )),
    }
}
fn read(stream: &mut UnixStream) -> io::Result<Response> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_MESSAGE {
        return Err(io::Error::other("Control response exceeds bound"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(stream: UnixStream) -> NativeInputControlClient {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        NativeInputControlClient {
            stream,
            report: InputControlReport {
                grabbed: 1,
                failed: 0,
                skipped: 0,
            },
        }
    }

    #[test]
    fn frontend_cancellation_unblocks_wait_without_claiming_release_ack() {
        let (client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let client = fixture(client);
        let cancellation = client.cancellation().unwrap();
        let waiter = std::thread::spawn(move || client.wait());
        drop(cancellation);
        assert!(waiter.join().unwrap().is_err());
        assert_eq!(server.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn finish_requires_worker_end_ack_and_drop_disconnects() {
        let (client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let worker = std::thread::spawn(move || {
            let mut stop = [1];
            server.read_exact(&mut stop).unwrap();
            assert_eq!(stop, [0]);
            let bytes = serde_json::to_vec(&Response::Ended).unwrap();
            server
                .write_all(&(bytes.len() as u32).to_be_bytes())
                .unwrap();
            server.write_all(&bytes).unwrap();
            assert_eq!(server.read(&mut [0]).unwrap(), 0);
        });
        fixture(client).finish().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn admission_does_not_silently_accept_partial_or_empty_coverage() {
        for (grabbed, failed, skipped, partial, ok) in [
            (0, 0, 1, true, false),
            (1, 1, 0, false, false),
            (1, 1, 0, true, true),
            (129, 0, 0, true, false),
            (1, 0, 0, false, true),
            (usize::MAX, 1, 0, true, false),
        ] {
            assert_eq!(
                admit(
                    Response::Active {
                        grabbed,
                        failed,
                        skipped
                    },
                    partial
                )
                .is_ok(),
                ok
            );
        }
        assert!(
            ended(Response::Active {
                grabbed: 1,
                failed: 0,
                skipped: 0
            })
            .is_err()
        );
        ended(Response::Ended).unwrap();
    }
}
