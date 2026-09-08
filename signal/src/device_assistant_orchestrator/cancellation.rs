//! OSS is single-node: bind cancellation to the authenticated actor and request.
//! Weak entries never keep completed turns alive; registration prunes dead keys.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio_util::sync::CancellationToken;

type Requests = HashMap<(i32, String), Weak<CancellationToken>>;
fn requests() -> &'static Mutex<Requests> {
    static REQUESTS: OnceLock<Mutex<Requests>> = OnceLock::new();
    REQUESTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn register(actor: i32, request: &str) -> Arc<CancellationToken> {
    let mut entries = requests().lock().expect("cancellation registry poisoned");
    entries.retain(|_, token| token.strong_count() > 0);
    let key = (actor, request.to_owned());
    if let Some(token) = entries.get(&key).and_then(Weak::upgrade) {
        return token;
    }
    let token = Arc::new(CancellationToken::new());
    entries.insert(key, Arc::downgrade(&token));
    token
}

pub(crate) fn cancel(actor: i32, request: &str) -> bool {
    let entries = requests().lock().expect("cancellation registry poisoned");
    let Some(token) = entries
        .get(&(actor, request.to_owned()))
        .and_then(Weak::upgrade)
    else {
        return false;
    };
    token.cancel();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_actor_and_request_scoped_and_survives_spawn_handoff() {
        let request = uuid::Uuid::new_v4().to_string();
        let dispatched = register(1, &request);
        let other = register(2, &request);
        assert!(!cancel(3, &request));
        assert!(cancel(1, &request));
        let running = register(1, &request);
        assert!(running.is_cancelled());
        assert!(!other.is_cancelled());
        drop(dispatched);
        drop(running);
        assert!(!cancel(1, &request));
        assert!(!register(1, &request).is_cancelled());
    }
}
