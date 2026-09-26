//! A stalled native thread retains its slot even after its caller stops waiting.
use std::{
    io,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

struct Budget {
    active: AtomicUsize,
    limit: usize,
}
pub(super) struct Permit(Arc<Budget>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}
impl Budget {
    fn acquire(self: &Arc<Self>) -> io::Result<Permit> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.limit).then(|| active + 1)
            })
            .map_err(|_| io::Error::other("PipeWire capture thread budget is exhausted"))?;
        Ok(Permit(self.clone()))
    }
}

pub(super) fn reserve() -> io::Result<Permit> {
    static BUDGET: OnceLock<Arc<Budget>> = OnceLock::new();
    BUDGET
        .get_or_init(|| {
            Arc::new(Budget {
                active: AtomicUsize::new(0),
                limit: 8,
            })
        })
        .acquire()
}

/// A false result means the native thread still owns resources and its permit.
/// Dropping the JoinHandle detaches it; it does not cancel native code.
pub(super) fn finish(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
    }
    if handle.join().is_err() {
        log::warn!("PipeWire capture thread panicked during shutdown");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timed_out_thread_keeps_its_slot_until_native_work_actually_stops() {
        let budget = Arc::new(Budget {
            active: AtomicUsize::new(0),
            limit: 1,
        });
        let permit = budget.acquire().unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let (done, completed) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _ = wait.recv();
            drop(permit);
            done.send(()).unwrap();
        });
        assert!(!finish(thread, Duration::ZERO));
        assert!(budget.acquire().is_err());
        release.send(()).unwrap();
        completed.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(budget.acquire().is_ok());
    }
}
