//! Serialize monitor replacement with synchronous state publication.
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(crate) struct MonitorOwner(Mutex<u64>);

#[derive(Clone)]
pub(crate) struct MonitorLease {
    owner: Arc<MonitorOwner>,
    generation: u64,
}

impl MonitorOwner {
    pub(crate) fn claim(self: &Arc<Self>) -> MonitorLease {
        let current = self.invalidate();
        MonitorLease {
            owner: self.clone(),
            generation: *current,
        }
    }

    pub(crate) fn invalidate(&self) -> std::sync::MutexGuard<'_, u64> {
        let mut current = self.0.lock().expect("Linux monitor owner lock");
        *current = current
            .checked_add(1)
            .expect("Linux monitor owner exhausted");
        current
    }
}

impl MonitorLease {
    pub(crate) fn current(&self) -> bool {
        self.with_current(|| ()).is_some()
    }

    // Keep the owner lock until publication finishes; a check followed by an
    // unlocked write would allow an old task to overwrite its replacement.
    pub(crate) fn with_current<T>(&self, publish: impl FnOnce() -> T) -> Option<T> {
        let current = self.owner.0.lock().expect("Linux monitor owner lock");
        (*current == self.generation).then(publish)
    }

    pub(crate) fn revoke(&self, clear: impl FnOnce()) {
        let mut current = self.owner.0.lock().expect("Linux monitor owner lock");
        if *current == self.generation {
            *current = current
                .checked_add(1)
                .expect("Linux monitor owner exhausted");
            clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn replacement_waits_for_inflight_publication() {
        use std::sync::mpsc;
        use std::time::Duration;
        let owner = Arc::new(MonitorOwner::default());
        let old = owner.claim();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let publication = std::thread::spawn(move || {
            old.with_current(|| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            })
            .unwrap();
            old
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (claimed_tx, claimed_rx) = mpsc::channel();
        let replacement = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let new = owner.claim();
            claimed_tx.send(()).unwrap();
            new
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let premature = claimed_rx.recv_timeout(Duration::from_millis(50));
        release_tx.send(()).unwrap();
        let old = publication.join().unwrap();
        let new = replacement.join().unwrap();
        assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
        assert!(!old.current());
        assert!(new.current());
    }

    #[test]
    fn replaced_monitor_cannot_publish_or_clear_replacement() {
        let owner = Arc::new(MonitorOwner::default());
        let old = owner.claim();
        let state = AtomicUsize::new(1);
        let new = owner.claim();
        new.with_current(|| state.store(2, Ordering::SeqCst))
            .unwrap();
        assert!(
            old.with_current(|| state.store(3, Ordering::SeqCst))
                .is_none()
        );
        old.revoke(|| state.store(0, Ordering::SeqCst));
        assert_eq!(state.load(Ordering::SeqCst), 2);
        assert!(new.current());
        new.revoke(|| state.store(0, Ordering::SeqCst));
        assert_eq!(state.load(Ordering::SeqCst), 0);
        assert!(!new.current());
        assert!(
            new.with_current(|| state.store(4, Ordering::SeqCst))
                .is_none()
        );
    }
}
