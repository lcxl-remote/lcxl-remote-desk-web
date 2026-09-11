//! Native element identities live on one persistent thread, independently of
//! observation snapshots and authorization leases. Native handles never cross
//! threads (in particular, UIA interfaces never outlive their COM apartment).

use std::cell::Cell;
use std::sync::{OnceLock, mpsc};

use desk_agent_protocol::{AgentError, AgentErrorKind};

thread_local! { static ON_UI_THREAD: Cell<bool> = const { Cell::new(false) }; }
type Job = Box<dyn FnOnce() + Send>;

fn error(message: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::SessionUnavailable,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

pub(crate) fn run<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, AgentError> + Send + 'static,
) -> Result<T, AgentError> {
    if ON_UI_THREAD.with(Cell::get) {
        return operation();
    }
    static THREAD: OnceLock<Result<mpsc::SyncSender<Job>, String>> = OnceLock::new();
    let sender = THREAD.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<Job>(8);
        std::thread::Builder::new()
            .name("native-ui-identity".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                ON_UI_THREAD.with(|flag| flag.set(true));
                #[cfg(windows)]
                let initialized = unsafe {
                    windows::Win32::System::Com::CoInitializeEx(
                        None,
                        windows::Win32::System::Com::COINIT_MULTITHREADED,
                    )
                };
                #[cfg(windows)]
                if initialized.is_err() {
                    return;
                }
                // Keep the apartment alive for every retained UIA element.
                // Thread-local native handles are released when this thread exits.
                while let Ok(job) = receiver.recv() {
                    job();
                }
            })
            .map(|_| sender)
            .map_err(|e| e.to_string())
    });
    let sender = sender
        .as_ref()
        .map_err(|_| error("native UI thread could not start"))?;
    let (reply, result) = mpsc::sync_channel(1);
    sender
        .try_send(Box::new(move || {
            let _ = reply.send(operation());
        }))
        .map_err(|_| {
            error("native UI worker is busy or unavailable; this request was not executed")
        })?;
    result
        .recv()
        .map_err(|_| error("native UI worker stopped before returning a result"))?
}

pub(super) trait NativeElement {
    fn same_element(&self, other: &Self) -> bool;
    /// Transient permission/transport errors must never count as destruction.
    fn definitely_destroyed(&self) -> bool;
}

struct Entry<T> {
    process_id: u32,
    process_start: u64,
    key: Vec<u8>,
    element: T,
    id: String,
}

pub(super) struct IdentityStore<T> {
    entries: Vec<Entry<T>>,
    limit: usize,
    sweep: usize,
}

impl<T: NativeElement> IdentityStore<T> {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            entries: Vec::new(),
            limit,
            sweep: 0,
        }
    }

    pub(super) fn get(&mut self, id: &str, process_id: u32, process_start: u64) -> Option<T>
    where
        T: Clone,
    {
        let index = self.entries.iter().position(|entry| {
            entry.id == id && entry.process_id == process_id && entry.process_start == process_start
        })?;
        if self.entries[index].element.definitely_destroyed() {
            self.entries.remove(index);
            return None;
        }
        Some(self.entries[index].element.clone())
    }

    pub(super) fn retained_ids(&self) -> std::collections::HashSet<String> {
        self.entries.iter().map(|entry| entry.id.clone()).collect()
    }

    pub(super) fn identify(
        &mut self,
        process_id: u32,
        process_start: u64,
        key: Vec<u8>,
        element: T,
    ) -> Result<String, AgentError> {
        self.entries
            .retain(|e| e.process_id != process_id || e.process_start == process_start);
        let same = self.entries.iter().position(|e| {
            e.process_id == process_id
                && e.process_start == process_start
                && e.key == key
                && e.element.same_element(&element)
        });
        if let Some(index) = same {
            if !self.entries[index].element.definitely_destroyed() {
                return Ok(self.entries[index].id.clone());
            }
            self.entries.remove(index);
        }
        if self.entries.len() >= self.limit {
            // Bounded cleanup; never evict a live identity to make room.
            for _ in 0..32.min(self.entries.len()) {
                self.sweep %= self.entries.len();
                if self.entries[self.sweep].element.definitely_destroyed() {
                    self.entries.remove(self.sweep);
                    break;
                }
                self.sweep += 1;
            }
        }
        if self.entries.len() >= self.limit {
            return Err(AgentError {
                kind: AgentErrorKind::OutputLimitExceeded,
                message: "native UI identity capacity reached; live element IDs were preserved"
                    .into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.entries.push(Entry {
            process_id,
            process_start,
            key,
            element,
            id: id.clone(),
        });
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    #[derive(Clone)]
    struct Element {
        identity: u32,
        dead: Rc<Cell<bool>>,
    }
    impl NativeElement for Element {
        fn same_element(&self, other: &Self) -> bool {
            self.identity == other.identity
        }
        fn definitely_destroyed(&self) -> bool {
            self.dead.get()
        }
    }
    fn element(identity: u32) -> Element {
        Element {
            identity,
            dead: Rc::new(Cell::new(false)),
        }
    }
    #[test]
    fn identity_survives_repeated_observation_but_not_destruction_or_process_restart() {
        let mut store = IdentityStore::new(8);
        let first = element(1);
        let id = store.identify(10, 20, vec![1], first.clone()).unwrap();
        assert_eq!(store.identify(10, 20, vec![1], first.clone()).unwrap(), id);
        assert!(store.get(&id, 10, 20).is_some());
        assert!(store.get(&id, 11, 20).is_none());
        assert!(store.get(&id, 10, 21).is_none());
        first.dead.set(true);
        assert!(store.get(&id, 10, 20).is_none());
        let rebuilt = store.identify(10, 20, vec![1], element(1)).unwrap();
        assert_ne!(
            id, rebuilt,
            "even a recycled native key cannot resurrect the old ID"
        );
        assert_ne!(
            rebuilt,
            store.identify(10, 21, vec![1], element(1)).unwrap()
        );
    }
    #[test]
    fn collisions_and_capacity_never_retarget_or_evict_live_elements() {
        let mut store = IdentityStore::new(2);
        let a = element(1);
        let id = store.identify(1, 1, vec![7], a.clone()).unwrap();
        assert_ne!(id, store.identify(1, 1, vec![7], element(2)).unwrap());
        assert!(store.identify(1, 1, vec![9], element(3)).is_err());
        assert_eq!(id, store.identify(1, 1, vec![7], a).unwrap());
    }
    #[test]
    fn native_calls_share_a_thread_and_nested_calls_do_not_deadlock() {
        let a = run(|| Ok(std::thread::current().id())).unwrap();
        let b = run(|| run(|| Ok(std::thread::current().id()))).unwrap();
        assert_eq!(a, b);
    }
}
