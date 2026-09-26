use super::*;
use std::sync::{Mutex, mpsc};

struct FakeDevice {
    released: Arc<AtomicBool>,
    outcome: u8,
}
impl Device for FakeDevice {
    fn escape(&mut self) -> io::Result<bool> {
        match self.outcome {
            1 => Ok(true),
            2 => Err(io::Error::other("device ended")),
            _ => Ok(false),
        }
    }
}
impl Drop for FakeDevice {
    fn drop(&mut self) {
        self.released.store(true, Ordering::Release);
    }
}

#[test]
fn escape_expiry_and_device_failure_release_before_callback() {
    for (outcome, duration, expected) in [
        (1, Duration::from_secs(1), EndReason::LocalEscape),
        (2, Duration::from_secs(1), EndReason::DeviceLost),
        (0, Duration::from_millis(5), EndReason::Expired),
    ] {
        let released = Arc::new(AtomicBool::new(false));
        let callback_released = released.clone();
        let (tx, rx) = mpsc::channel();
        let guard = InputBlock::start(
            vec![FakeDevice { released, outcome }],
            BlockReport {
                grabbed: 1,
                failed: 2,
                skipped: 3,
            },
            duration,
            move |reason| {
                tx.send((reason, callback_released.load(Ordering::Acquire)))
                    .unwrap()
            },
        )
        .unwrap();
        assert_eq!(
            guard.report(),
            BlockReport {
                grabbed: 1,
                failed: 2,
                skipped: 3
            }
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            (expected, true)
        );
        assert!(!guard.is_active());
    }
}

#[test]
fn dropping_guard_releases_and_joins_worker() {
    let released = Arc::new(AtomicBool::new(false));
    let reason = Arc::new(Mutex::new(None));
    let callback_reason = reason.clone();
    let guard = InputBlock::start(
        vec![FakeDevice {
            released: released.clone(),
            outcome: 0,
        }],
        BlockReport {
            grabbed: 1,
            ..Default::default()
        },
        Duration::from_secs(60),
        move |value| *callback_reason.lock().unwrap() = Some(value),
    )
    .unwrap();
    drop(guard);
    assert!(released.load(Ordering::Acquire));
    assert_eq!(*reason.lock().unwrap(), Some(EndReason::Released));
}

#[test]
fn empty_and_unbounded_requests_cannot_become_active() {
    assert!(
        InputBlock::start(
            Vec::<FakeDevice>::new(),
            BlockReport::default(),
            Duration::from_secs(1),
            |_| {}
        )
        .is_err()
    );
    assert!(validate_duration(Duration::ZERO).is_err());
    assert!(validate_duration(Duration::from_secs(301)).is_err());
}
