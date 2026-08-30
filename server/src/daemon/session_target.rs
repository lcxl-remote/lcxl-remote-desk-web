//! Daemon-owned session target catalog for user-visible session selection.

use desk_ipc_protocol::message::SessionKey;
pub use desk_signal_facade::model::signal::SessionTargetDescriptor;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCandidate {
    pub session: SessionKey,
    pub display_name: String,
    pub session_type: Option<String>,
    pub seat: Option<String>,
    pub foreground: bool,
    pub remote_desktop_ready: bool,
    pub terminal_ready: bool,
    pub file_ready: bool,
    pub assistant_ready: bool,
}

impl SessionCandidate {
    fn supports(&self, capability: SessionCapability) -> bool {
        match capability {
            SessionCapability::RemoteDesktop => self.remote_desktop_ready,
            SessionCapability::Terminal => self.terminal_ready,
            SessionCapability::FileManager => self.file_ready,
            SessionCapability::Assistant => self.assistant_ready,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionCapability {
    RemoteDesktop,
    Terminal,
    FileManager,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTargetSelectionError {
    Unavailable,
    SelectionRequired,
    Stale,
}

impl fmt::Display for SessionTargetSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("SESSION_UNAVAILABLE"),
            Self::SelectionRequired => f.write_str("SESSION_SELECTION_REQUIRED"),
            Self::Stale => f.write_str("SESSION_TARGET_STALE"),
        }
    }
}

struct CatalogEntry {
    id: Uuid,
    candidate: SessionCandidate,
}

struct CatalogInner {
    revision: u64,
    by_session: HashMap<SessionKey, CatalogEntry>,
    session_by_id: HashMap<Uuid, SessionKey>,
}

#[derive(Clone)]
pub struct SessionTargetCatalog {
    daemon_generation: Uuid,
    inner: Arc<RwLock<CatalogInner>>,
}

impl Default for SessionTargetCatalog {
    fn default() -> Self {
        Self {
            daemon_generation: Uuid::new_v4(),
            inner: Arc::new(RwLock::new(CatalogInner {
                revision: 0,
                by_session: HashMap::new(),
                session_by_id: HashMap::new(),
            })),
        }
    }
}

impl SessionTargetCatalog {
    pub fn daemon_generation(&self) -> Uuid {
        self.daemon_generation
    }

    pub fn upsert(&self, candidate: SessionCandidate) -> String {
        let mut inner = self.inner.write().unwrap();
        if let Some(entry) = inner.by_session.get_mut(&candidate.session) {
            entry.candidate = candidate;
            let id = entry.id.to_string();
            inner.revision = inner.revision.saturating_add(1);
            return id;
        }

        let id = Uuid::new_v4();
        inner.session_by_id.insert(id, candidate.session.clone());
        inner
            .by_session
            .insert(candidate.session.clone(), CatalogEntry { id, candidate });
        inner.revision = inner.revision.saturating_add(1);
        id.to_string()
    }

    pub fn remove(&self, session: &SessionKey) -> bool {
        let mut inner = self.inner.write().unwrap();
        let Some(entry) = inner.by_session.remove(session) else {
            return false;
        };
        inner.session_by_id.remove(&entry.id);
        inner.revision = inner.revision.saturating_add(1);
        true
    }

    /// Update only worker-derived readiness while preserving the opaque target
    /// identity and platform display metadata for this session generation.
    /// Returns false when the registration was already revoked.
    pub fn set_readiness(
        &self,
        session: &SessionKey,
        remote_desktop_ready: bool,
        terminal_ready: bool,
        file_ready: bool,
        assistant_ready: bool,
    ) -> bool {
        let mut inner = self.inner.write().unwrap();
        let Some(entry) = inner.by_session.get_mut(session) else {
            return false;
        };
        entry.candidate.remote_desktop_ready = remote_desktop_ready;
        entry.candidate.terminal_ready = terminal_ready;
        entry.candidate.file_ready = file_ready;
        entry.candidate.assistant_ready = assistant_ready;
        inner.revision = inner.revision.saturating_add(1);
        true
    }

    /// Update only interactive desktop readiness. Desktop switches briefly
    /// fence new remote-desktop admissions while terminal, file, and assistant
    /// operations remain bound to the same session-user worker.
    pub fn set_remote_desktop_readiness(
        &self,
        session: &SessionKey,
        remote_desktop_ready: bool,
    ) -> bool {
        let mut inner = self.inner.write().unwrap();
        let Some(entry) = inner.by_session.get_mut(session) else {
            return false;
        };
        entry.candidate.remote_desktop_ready = remote_desktop_ready;
        inner.revision = inner.revision.saturating_add(1);
        true
    }

    /// Reconciles the catalog with an authoritative platform snapshot.
    /// Existing identities survive when the full session key (including its
    /// generation) is unchanged; missing and generation-reused sessions are
    /// removed before their target IDs could be selected again.
    pub fn replace_all(&self, candidates: Vec<SessionCandidate>) {
        let expected: HashSet<_> = candidates
            .iter()
            .map(|candidate| candidate.session.clone())
            .collect();
        let mut inner = self.inner.write().unwrap();

        let removed_ids: Vec<_> = inner
            .by_session
            .iter()
            .filter(|(session, _)| !expected.contains(*session))
            .map(|(_, entry)| entry.id)
            .collect();
        inner
            .by_session
            .retain(|session, _| expected.contains(session));
        for id in removed_ids {
            inner.session_by_id.remove(&id);
        }

        for candidate in candidates {
            if let Some(entry) = inner.by_session.get_mut(&candidate.session) {
                entry.candidate = candidate;
            } else {
                let id = Uuid::new_v4();
                inner.session_by_id.insert(id, candidate.session.clone());
                inner
                    .by_session
                    .insert(candidate.session.clone(), CatalogEntry { id, candidate });
            }
        }
        inner.revision = inner.revision.saturating_add(1);
    }

    pub fn list_for(&self, capability: SessionCapability) -> (u64, Vec<SessionTargetDescriptor>) {
        let inner = self.inner.read().unwrap();
        let mut targets: Vec<_> = inner
            .by_session
            .values()
            .filter(|entry| entry.candidate.supports(capability))
            .map(|entry| SessionTargetDescriptor {
                target_id: entry.id.to_string(),
                display_name: entry.candidate.display_name.clone(),
                session_type: entry.candidate.session_type.clone(),
                seat: entry.candidate.seat.clone(),
                foreground: entry.candidate.foreground,
                remote_desktop_ready: entry.candidate.remote_desktop_ready,
                terminal_ready: entry.candidate.terminal_ready,
                file_ready: entry.candidate.file_ready,
                assistant_ready: entry.candidate.assistant_ready,
            })
            .collect();
        targets.sort_by(|left, right| {
            right
                .foreground
                .cmp(&left.foreground)
                .then_with(|| left.display_name.cmp(&right.display_name))
                .then_with(|| left.target_id.cmp(&right.target_id))
        });
        (inner.revision, targets)
    }

    pub fn select(
        &self,
        capability: SessionCapability,
        requested_target_id: Option<&str>,
    ) -> Result<SessionKey, SessionTargetSelectionError> {
        let inner = self.inner.read().unwrap();
        if let Some(requested) = requested_target_id {
            let id = Uuid::parse_str(requested).map_err(|_| SessionTargetSelectionError::Stale)?;
            let session = inner
                .session_by_id
                .get(&id)
                .ok_or(SessionTargetSelectionError::Stale)?;
            let entry = inner
                .by_session
                .get(session)
                .ok_or(SessionTargetSelectionError::Stale)?;
            if !entry.candidate.supports(capability) {
                return Err(SessionTargetSelectionError::Unavailable);
            }
            return Ok(session.clone());
        }

        let mut candidates = inner
            .by_session
            .values()
            .filter(|entry| entry.candidate.supports(capability));
        let first = candidates
            .next()
            .ok_or(SessionTargetSelectionError::Unavailable)?;
        if candidates.next().is_some() {
            return Err(SessionTargetSelectionError::SelectionRequired);
        }
        Ok(first.candidate.session.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, generation: u64) -> SessionCandidate {
        SessionCandidate {
            session: SessionKey {
                platform_session_id: name.to_string(),
                session_generation: generation,
            },
            display_name: name.to_string(),
            session_type: Some("wayland".to_string()),
            seat: Some("seat0".to_string()),
            foreground: false,
            remote_desktop_ready: true,
            terminal_ready: true,
            file_ready: true,
            assistant_ready: true,
        }
    }

    #[test]
    fn zero_one_many_selection_is_fail_closed() {
        let catalog = SessionTargetCatalog::default();
        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, None),
            Err(SessionTargetSelectionError::Unavailable)
        );

        let first = candidate("session-a", 1);
        catalog.upsert(first.clone());
        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, None),
            Ok(first.session.clone())
        );

        catalog.upsert(candidate("session-b", 1));
        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, None),
            Err(SessionTargetSelectionError::SelectionRequired)
        );
    }

    #[test]
    fn explicit_target_is_immutable_and_stale_after_logout() {
        let catalog = SessionTargetCatalog::default();
        let first = candidate("session-a", 1);
        let target_id = catalog.upsert(first.clone());
        catalog.upsert(candidate("session-b", 1));

        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, Some(&target_id)),
            Ok(first.session.clone())
        );
        assert!(catalog.remove(&first.session));
        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, Some(&target_id)),
            Err(SessionTargetSelectionError::Stale)
        );

        let reused = candidate("session-a", 2);
        let replacement_id = catalog.upsert(reused.clone());
        assert_ne!(target_id, replacement_id);
        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, Some(&target_id)),
            Err(SessionTargetSelectionError::Stale)
        );
        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, Some(&replacement_id)),
            Ok(reused.session)
        );
    }

    #[test]
    fn registered_but_not_ready_session_is_not_selectable() {
        let catalog = SessionTargetCatalog::default();
        let mut pending = candidate("session-a", 1);
        pending.remote_desktop_ready = false;
        pending.terminal_ready = false;
        pending.file_ready = false;
        pending.assistant_ready = false;
        let target_id = catalog.upsert(pending);

        assert!(
            catalog
                .list_for(SessionCapability::RemoteDesktop)
                .1
                .is_empty()
        );
        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, Some(&target_id)),
            Err(SessionTargetSelectionError::Unavailable)
        );
    }

    #[test]
    fn updating_readiness_preserves_target_id_within_one_session_generation() {
        let catalog = SessionTargetCatalog::default();
        let mut pending = candidate("session-a", 1);
        pending.terminal_ready = false;
        let first_id = catalog.upsert(pending.clone());
        pending.terminal_ready = true;
        let second_id = catalog.upsert(pending);

        assert_eq!(first_id, second_id);
        assert_eq!(
            catalog.list_for(SessionCapability::RemoteDesktop).1.len(),
            1
        );
    }

    #[test]
    fn readiness_update_preserves_target_and_never_resurrects_revoked_session() {
        let catalog = SessionTargetCatalog::default();
        let mut pending = candidate("session-a", 1);
        pending.remote_desktop_ready = false;
        pending.terminal_ready = false;
        pending.file_ready = false;
        pending.assistant_ready = false;
        let target_id = catalog.upsert(pending.clone());

        assert!(catalog.set_readiness(&pending.session, true, true, true, true));
        assert_eq!(
            catalog.select(SessionCapability::Terminal, Some(&target_id)),
            Ok(pending.session.clone())
        );

        assert!(catalog.remove(&pending.session));
        assert!(!catalog.set_readiness(&pending.session, true, true, true, true));
        assert_eq!(
            catalog.select(SessionCapability::Terminal, Some(&target_id)),
            Err(SessionTargetSelectionError::Stale)
        );
    }

    #[test]
    fn selection_is_scoped_to_the_requested_capability() {
        let catalog = SessionTargetCatalog::default();
        let mut desktop_only = candidate("desktop-only", 1);
        desktop_only.terminal_ready = false;
        let desktop_target = catalog.upsert(desktop_only.clone());

        let mut terminal_only = candidate("terminal-only", 1);
        terminal_only.remote_desktop_ready = false;
        let terminal_target = catalog.upsert(terminal_only.clone());

        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, None),
            Ok(desktop_only.session)
        );
        assert_eq!(
            catalog.select(SessionCapability::Terminal, None),
            Ok(terminal_only.session)
        );
        assert_eq!(
            catalog.select(SessionCapability::Terminal, Some(&desktop_target)),
            Err(SessionTargetSelectionError::Unavailable)
        );
        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, Some(&terminal_target)),
            Err(SessionTargetSelectionError::Unavailable)
        );
    }

    #[test]
    fn catalog_identity_is_daemon_generation_bound() {
        let first = SessionTargetCatalog::default();
        let second = SessionTargetCatalog::default();
        assert_ne!(first.daemon_generation(), second.daemon_generation());
    }

    #[test]
    fn authoritative_reconcile_preserves_live_ids_and_revokes_missing_ids() {
        let catalog = SessionTargetCatalog::default();
        let first = candidate("session-a", 1);
        let first_id = catalog.upsert(first.clone());
        let removed = candidate("session-b", 1);
        let removed_id = catalog.upsert(removed);

        catalog.replace_all(vec![first.clone(), candidate("session-c", 1)]);

        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, Some(&first_id)),
            Ok(first.session)
        );
        assert_eq!(
            catalog.select(SessionCapability::RemoteDesktop, Some(&removed_id)),
            Err(SessionTargetSelectionError::Stale)
        );
    }
}
