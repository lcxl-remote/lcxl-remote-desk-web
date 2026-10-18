//! Test-only verification that the constrained thread polls the product path.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, Weak},
    thread::{self, ThreadId},
};

struct State {
    thread: ThreadId,
    points: HashMap<&'static str, usize>,
}
fn registry() -> &'static Mutex<HashMap<String, Weak<Mutex<State>>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Weak<Mutex<State>>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}
pub(crate) struct Probe {
    id: String,
    state: Arc<Mutex<State>>,
}
pub(crate) fn start(id: &str) -> Probe {
    let state = Arc::new(Mutex::new(State {
        thread: thread::current().id(),
        points: HashMap::new(),
    }));
    assert!(
        registry()
            .lock()
            .unwrap()
            .insert(id.into(), Arc::downgrade(&state))
            .and_then(|old| old.upgrade())
            .is_none()
    );
    Probe {
        id: id.into(),
        state,
    }
}
pub(crate) fn observe(id: &str, point: &'static str) {
    let state = registry().lock().unwrap().get(id).and_then(Weak::upgrade);
    if let Some(state) = state {
        let mut state = state.lock().unwrap();
        assert_eq!(
            state.thread,
            thread::current().id(),
            "{point} escaped the constrained stack thread"
        );
        *state.points.entry(point).or_default() += 1;
    }
}
impl Probe {
    pub(crate) fn count(&self, point: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .points
            .get(point)
            .copied()
            .unwrap_or(0)
    }
    pub(crate) fn assert_points(&self, points: &[&str]) {
        let state = self.state.lock().unwrap();
        for point in points {
            assert!(
                state.points.get(point).copied().unwrap_or(0) > 0,
                "stack regression did not reach {point}"
            );
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}
#[test]
fn probe_rejects_polling_on_another_thread() {
    let _probe = start("cross-thread-positive-control");
    assert!(
        std::thread::spawn(|| observe("cross-thread-positive-control", "poll"))
            .join()
            .is_err()
    );
}
