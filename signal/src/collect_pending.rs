//! Pending remote-collection store for the OSS signal central brain.
//!
//! When a browser asks the signal server to diagnose a device, signal — not the
//! edge — drives the orchestration: it pushes a `CollectRequest` to the target
//! edge, reassembles the chunked `CollectResponse`, then runs the model phase.
//! This module holds the **security-relevant** half of that flow: the pending
//! store that makes a forged or mis-routed response impossible to accept.
//!
//! It mirrors the manager's `CollectPendingStore` (its own implementation, not a
//! shared extraction): the request is keyed by a central-generated `request_id`
//! (one-shot registration rejects a replayed id), every accepted chunk must
//! arrive from the connection the request was pushed to
//! (`target_connection_id == source`), and the entry is consumed on completion so
//! a late or duplicate chunk finds no pending state. The signal flavor is simpler
//! than the manager's — single-account, browser-stream only (no fleet `Await`
//! fan-out), no org/device/actor attribution carried here (the
//! `ControlFrameAuthorizer` stamps those separately). The portable signal server
//! is single-node, so the store is process-local in-memory state.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use desk_agent_protocol::diagnose::{CollectResponseChunk, DiagnoseRequestData};
use desk_agent_protocol::evidence::EvidenceSnapshot;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::chunk::SnapshotReassembler;

/// How long the central orchestrator keeps a pending collection before reaping
/// it. Covers only the collect round-trip; the subsequent model dial has its own
/// timeout. Doubles as the replay window: a `request_id` cannot be reused while
/// its entry is live, and the entry lives at most this long.
pub const COLLECT_TIMEOUT: Duration = Duration::from_secs(60);

/// The trusted routing of one in-flight remote collection. Every field is set by
/// signal from validated state when the collection starts; the edge can never
/// change them, so the response routing is unforgeable.
#[derive(Debug, Clone)]
pub struct CollectContext {
    /// The diagnosis / collection request id (central-generated, globally
    /// unique). The browser keyed its diagnosis on this, so the streamed
    /// `DiagnoseEvent` frames carry it back.
    pub request_id: String,
    /// Signaling connection id of the target edge — the only connection allowed
    /// to answer this request.
    pub target_connection_id: String,
    /// Signaling connection id of the browser that started the diagnosis (where
    /// the orchestrated result streams back).
    pub browser_connection_id: String,
    /// The original collection intent (question + context kinds + locale), reused
    /// to build the model prompt once evidence arrives.
    pub request: DiagnoseRequestData,
}

/// One pending collection: its context plus the chunk reassembler and the
/// registration instant (for TTL reaping).
struct PendingCollect {
    ctx: CollectContext,
    reassembler: SnapshotReassembler,
    registered_at: Instant,
}

/// What accepting a response chunk (or error) produced.
pub enum AcceptOutcome {
    /// More chunks are expected; nothing to do yet.
    NeedMore,
    /// The snapshot is complete. The pending entry has been removed (consumed).
    Complete {
        ctx: CollectContext,
        snapshot: Box<EvidenceSnapshot>,
    },
    /// The collection failed (edge error, or a reassembly/validation failure).
    /// The pending entry has been removed.
    Failed {
        ctx: CollectContext,
        error: AgentError,
    },
    /// The chunk did not match a live pending entry, or came from the wrong
    /// connection. Dropped without touching any pending state; the `&str` is the
    /// drop reason. This is the security gate: a response from a connection other
    /// than the one the request was pushed to is rejected.
    Rejected(&'static str),
}

/// In-flight remote collections keyed by the collect `request_id`.
#[derive(Default)]
pub struct CollectPendingStore {
    inner: Mutex<HashMap<String, PendingCollect>>,
}

impl CollectPendingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pending collection. Returns `false` if `request_id` is already
    /// in flight (the caller must reject the duplicate / replay).
    pub fn register(&self, ctx: CollectContext) -> bool {
        let mut map = self.inner.lock().unwrap();
        if map.contains_key(&ctx.request_id) {
            return false;
        }
        map.insert(
            ctx.request_id.clone(),
            PendingCollect {
                ctx,
                reassembler: SnapshotReassembler::new(),
                registered_at: Instant::now(),
            },
        );
        true
    }

    /// Number of in-flight collections (test/diagnostics).
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Accept a chunk for `chunk.request_id` arriving from `source_connection_id`.
    ///
    /// Dropped (`Rejected`) unless a pending entry exists **and** the source
    /// connection matches the one the request was pushed to — the per-request
    /// source binding that stops another connection forging or hijacking the
    /// response. On the final chunk the snapshot is verified (length + sha256)
    /// and the entry removed.
    pub fn accept_chunk(
        &self,
        source_connection_id: &str,
        chunk: &CollectResponseChunk,
    ) -> AcceptOutcome {
        let mut map = self.inner.lock().unwrap();
        let Some(pending) = map.get_mut(&chunk.request_id) else {
            return AcceptOutcome::Rejected("no pending collection for request_id");
        };
        if pending.ctx.target_connection_id != source_connection_id {
            return AcceptOutcome::Rejected("response source connection does not match target");
        }
        if let Err(e) = pending.reassembler.push(chunk) {
            let pending = map.remove(&chunk.request_id).expect("just matched");
            return AcceptOutcome::Failed {
                ctx: pending.ctx,
                error: transport_error(format!("chunk rejected: {e}")),
            };
        }
        if !chunk.last {
            return AcceptOutcome::NeedMore;
        }
        let pending = map.remove(&chunk.request_id).expect("just matched");
        match pending.reassembler.finish() {
            Ok(snapshot) => AcceptOutcome::Complete {
                ctx: pending.ctx,
                snapshot: Box::new(snapshot),
            },
            Err(e) => AcceptOutcome::Failed {
                ctx: pending.ctx,
                error: transport_error(format!("snapshot reassembly failed: {e}")),
            },
        }
    }

    /// Fail and remove a pending collection (the edge reported a wholesale
    /// error). Same source binding as [`accept_chunk`].
    pub fn fail(
        &self,
        source_connection_id: &str,
        request_id: &str,
        error: AgentError,
    ) -> AcceptOutcome {
        let mut map = self.inner.lock().unwrap();
        let Some(pending) = map.get(request_id) else {
            return AcceptOutcome::Rejected("no pending collection for request_id");
        };
        if pending.ctx.target_connection_id != source_connection_id {
            return AcceptOutcome::Rejected("error source connection does not match target");
        }
        let pending = map.remove(request_id).expect("just matched");
        AcceptOutcome::Failed {
            ctx: pending.ctx,
            error,
        }
    }

    /// Drop a pending collection without producing an outcome (e.g. the browser
    /// cancelled). Returns its context if it existed.
    pub fn cancel(&self, request_id: &str) -> Option<CollectContext> {
        self.inner.lock().unwrap().remove(request_id).map(|p| p.ctx)
    }

    /// Drop every pending collection whose target **or** browser is
    /// `connection_id` (that connection closed). Returns their contexts so the
    /// caller can fail the affected streams.
    pub fn drain_for_connection(&self, connection_id: &str) -> Vec<CollectContext> {
        let mut map = self.inner.lock().unwrap();
        let ids: Vec<String> = map
            .iter()
            .filter(|(_, p)| {
                p.ctx.target_connection_id == connection_id
                    || p.ctx.browser_connection_id == connection_id
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| map.remove(&id))
            .map(|p| p.ctx)
            .collect()
    }

    /// Remove and return every pending collection older than [`COLLECT_TIMEOUT`].
    /// The caller fails the corresponding browser streams; afterward the
    /// `request_id` is free for reuse.
    pub fn reap_expired(&self) -> Vec<CollectContext> {
        let mut map = self.inner.lock().unwrap();
        let now = Instant::now();
        let ids: Vec<String> = map
            .iter()
            .filter(|(_, p)| now.duration_since(p.registered_at) >= COLLECT_TIMEOUT)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| map.remove(&id))
            .map(|p| p.ctx)
            .collect()
    }
}

fn transport_error(message: String) -> AgentError {
    AgentError {
        kind: AgentErrorKind::TransportError,
        message,
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::chunk::chunk_snapshot;

    fn ctx(request_id: &str, target: &str) -> CollectContext {
        CollectContext {
            request_id: request_id.to_string(),
            target_connection_id: target.to_string(),
            browser_connection_id: "browser-1".to_string(),
            request: DiagnoseRequestData::default(),
        }
    }

    fn snapshot() -> EvidenceSnapshot {
        EvidenceSnapshot::record("test", "test snapshot", "2026-01-01T00:00:00Z", vec![])
    }

    /// A second registration of the same request_id is rejected (replay / dup).
    #[test]
    fn register_is_one_shot_per_request_id() {
        let store = CollectPendingStore::new();
        assert!(store.register(ctx("r1", "edge-1")));
        assert!(!store.register(ctx("r1", "edge-1")));
        assert_eq!(store.len(), 1);
    }

    /// A chunk from a connection other than the request target is rejected
    /// without disturbing the pending entry.
    #[test]
    fn chunk_from_wrong_connection_is_rejected() {
        let store = CollectPendingStore::new();
        store.register(ctx("r1", "edge-1"));
        let chunks = chunk_snapshot("r1", &snapshot(), 64 * 1024).unwrap();
        match store.accept_chunk("attacker-conn", &chunks[0]) {
            AcceptOutcome::Rejected(_) => {}
            _ => panic!("expected rejection from wrong source connection"),
        }
        // The pending entry is untouched.
        assert_eq!(store.len(), 1);
    }

    /// A chunk for an unknown request_id is rejected.
    #[test]
    fn chunk_for_unknown_request_is_rejected() {
        let store = CollectPendingStore::new();
        let chunks = chunk_snapshot("ghost", &snapshot(), 64 * 1024).unwrap();
        assert!(matches!(
            store.accept_chunk("edge-1", &chunks[0]),
            AcceptOutcome::Rejected(_)
        ));
    }

    /// A full chunk sequence from the bound connection completes and consumes the
    /// entry; a replayed final chunk afterward finds no pending state.
    #[test]
    fn complete_consumes_entry_and_rejects_replay() {
        let store = CollectPendingStore::new();
        store.register(ctx("r1", "edge-1"));
        let chunks = chunk_snapshot("r1", &snapshot(), 64 * 1024).unwrap();
        let last = chunks.len() - 1;
        for c in &chunks[..last] {
            assert!(matches!(
                store.accept_chunk("edge-1", c),
                AcceptOutcome::NeedMore
            ));
        }
        assert!(matches!(
            store.accept_chunk("edge-1", &chunks[last]),
            AcceptOutcome::Complete { .. }
        ));
        assert!(store.is_empty(), "entry consumed on completion");
        // A late/duplicate chunk now finds no pending entry.
        assert!(matches!(
            store.accept_chunk("edge-1", &chunks[last]),
            AcceptOutcome::Rejected(_)
        ));
    }

    /// `fail` honours the same source binding and removes the entry.
    #[test]
    fn fail_requires_matching_source_and_removes_entry() {
        let store = CollectPendingStore::new();
        store.register(ctx("r1", "edge-1"));
        let err = transport_error("boom".into());
        assert!(matches!(
            store.fail("attacker-conn", "r1", err.clone()),
            AcceptOutcome::Rejected(_)
        ));
        assert_eq!(store.len(), 1);
        assert!(matches!(
            store.fail("edge-1", "r1", err),
            AcceptOutcome::Failed { .. }
        ));
        assert!(store.is_empty());
    }

    /// Draining by connection removes entries whose target or browser matches.
    #[test]
    fn drain_for_connection_removes_target_and_browser_matches() {
        let store = CollectPendingStore::new();
        store.register(ctx("r1", "edge-1"));
        store.register(ctx("r2", "edge-2"));
        let drained = store.drain_for_connection("edge-1");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].request_id, "r1");
        // The browser connection is shared by both; draining it clears the rest.
        let drained = store.drain_for_connection("browser-1");
        assert_eq!(drained.len(), 1);
        assert!(store.is_empty());
    }
}
