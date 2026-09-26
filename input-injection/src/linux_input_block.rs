//! Best-effort blocking of the input devices present at acquisition time.
//! This module neither grants device permissions nor creates virtual devices.
mod native;
#[cfg(test)]
mod tests;

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockReport {
    pub grabbed: usize,
    pub failed: usize,
    pub skipped: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndReason {
    Released,
    LocalEscape,
    Expired,
    DeviceLost,
}

struct State {
    active: AtomicBool,
    stop: AtomicBool,
}

/// Dropping the guard releases the device handles. Automatic expiry and local
/// escape also release them; neither proves already queued input was withdrawn.
pub struct InputBlock {
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
    report: BlockReport,
}

impl InputBlock {
    /// Call from a blocking context after local consent. The callback runs only
    /// after devices have been released and must not wait for this thread.
    pub fn acquire(
        duration: Duration,
        on_end: impl FnOnce(EndReason) + Send + 'static,
    ) -> io::Result<Self> {
        validate_duration(duration)?;
        let (devices, report) = native::acquire()?;
        Self::start(devices, report, duration, on_end)
    }

    fn start<D: Device>(
        devices: Vec<D>,
        report: BlockReport,
        duration: Duration,
        on_end: impl FnOnce(EndReason) + Send + 'static,
    ) -> io::Result<Self> {
        validate_duration(duration)?;
        if devices.is_empty() {
            return Err(io::Error::other("No input device was blocked"));
        }
        let state = Arc::new(State {
            active: AtomicBool::new(true),
            stop: AtomicBool::new(false),
        });
        let owner = state.clone();
        let deadline = Instant::now() + duration;
        let thread = thread::Builder::new()
            .name("linux-input-block".into())
            .spawn(move || {
                let mut held = Held {
                    devices,
                    state: owner,
                };
                let reason = run(&mut held, deadline);
                // Revoke activity and release handles before publishing completion.
                drop(held);
                on_end(reason);
            })?;
        Ok(Self {
            state,
            thread: Some(thread),
            report,
        })
    }

    pub fn report(&self) -> BlockReport {
        self.report
    }
    pub fn is_active(&self) -> bool {
        self.state.active.load(Ordering::Acquire)
    }
    pub fn release(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        self.state.active.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            // A completion callback may indirectly drop its own public guard.
            if thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}
impl Drop for InputBlock {
    fn drop(&mut self) {
        self.release();
    }
}

fn validate_duration(duration: Duration) -> io::Result<()> {
    if duration.is_zero() || duration > Duration::from_secs(300) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Input blocking requires a duration greater than zero and at most 300 seconds",
        ));
    }
    Ok(())
}

trait Device: Send + 'static {
    /// Nonblocking bounded polling. True means the local escape chord fired.
    fn escape(&mut self) -> io::Result<bool>;
}
struct Held<D> {
    devices: Vec<D>,
    state: Arc<State>,
}
impl<D> Drop for Held<D> {
    fn drop(&mut self) {
        self.state.active.store(false, Ordering::Release);
    }
}
fn run<D: Device>(held: &mut Held<D>, deadline: Instant) -> EndReason {
    loop {
        if held.state.stop.load(Ordering::Acquire) {
            return EndReason::Released;
        }
        if Instant::now() >= deadline {
            return EndReason::Expired;
        }
        for device in &mut held.devices {
            match device.escape() {
                Ok(true) => return EndReason::LocalEscape,
                Ok(false) => {}
                Err(_) => return EndReason::DeviceLost,
            }
        }
        thread::park_timeout(Duration::from_millis(10));
    }
}
