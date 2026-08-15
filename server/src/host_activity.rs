use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use desk_ipc_protocol::message::FileTransferDirection;
use desk_signal_facade::model::request_remote_authz::ActorSummary;
use tokio::sync::broadcast;

use crate::host_control::{
    HostAccessSession, HostAccessSnapshot, HostControlMessage, HostFileTransferDirection,
    HostFileTransferSummary, HostRemoteAccessStatus,
};

const MAX_VISIBLE_SESSIONS: usize = 64;
const MAX_DISPLAY_FIELD_CHARS: usize = 160;

#[derive(Clone)]
pub struct HostActivityRegistry {
    inner: Arc<RwLock<HostActivityState>>,
    publisher: broadcast::Sender<HostControlMessage>,
}

struct HostActivityState {
    epoch: String,
    revision: u64,
    indicator_enabled: bool,
    remote_access: HostRemoteAccessStatus,
    sessions: BTreeMap<String, SessionState>,
}

struct SessionState {
    actor: ActorSummary,
    started_at: String,
    pc_connected: bool,
    pc_handoff: bool,
    video_negotiated: bool,
    system_audio_capture: bool,
    remote_control: bool,
    terminal_count: u32,
    file_manager: bool,
    transfers: BTreeMap<String, HostFileTransferSummary>,
}

impl SessionState {
    fn new(actor: ActorSummary) -> Self {
        Self {
            actor: sanitize_actor(actor),
            started_at: chrono::Utc::now().to_rfc3339(),
            pc_connected: false,
            pc_handoff: false,
            video_negotiated: false,
            system_audio_capture: false,
            remote_control: false,
            terminal_count: 0,
            file_manager: false,
            transfers: BTreeMap::new(),
        }
    }

    fn public(&self, connection_id: &str) -> HostAccessSession {
        HostAccessSession {
            connection_id: connection_id.to_string(),
            actor: self.actor.clone(),
            started_at: self.started_at.clone(),
            desktop_view: (self.pc_connected || self.pc_handoff) && self.video_negotiated,
            system_audio_capture: self.system_audio_capture,
            remote_control: self.remote_control,
            terminal_count: self.terminal_count,
            file_manager: self.file_manager,
            transfers: self.transfers.values().cloned().collect(),
        }
    }

    fn has_recorded_state(&self) -> bool {
        self.pc_connected
            || self.pc_handoff
            || self.video_negotiated
            || self.system_audio_capture
            || self.remote_control
            || self.terminal_count != 0
            || self.file_manager
            || !self.transfers.is_empty()
    }
}

impl HostActivityRegistry {
    pub fn new(publisher: broadcast::Sender<HostControlMessage>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HostActivityState {
                epoch: uuid::Uuid::new_v4().to_string(),
                revision: 0,
                indicator_enabled: true,
                remote_access: HostRemoteAccessStatus::default(),
                sessions: BTreeMap::new(),
            })),
            publisher,
        }
    }

    pub fn snapshot(&self) -> HostAccessSnapshot {
        snapshot_from(&self.inner.read().unwrap())
    }

    pub fn set_indicator_enabled(&self, enabled: bool) {
        self.mutate(|state| {
            if state.indicator_enabled == enabled {
                return false;
            }
            state.indicator_enabled = enabled;
            true
        });
    }

    pub fn set_remote_access_status(&self, status: HostRemoteAccessStatus) {
        self.mutate(|state| {
            if state.remote_access == status {
                return false;
            }
            state.remote_access = status;
            true
        });
    }

    pub fn ensure_session(&self, connection_id: &str, actor: ActorSummary) {
        self.mutate(|state| match state.sessions.get_mut(connection_id) {
            Some(session) if session.actor == ActorSummary::unknown() => {
                let actor = sanitize_actor(actor);
                if session.actor == actor {
                    false
                } else {
                    session.actor = actor;
                    true
                }
            }
            Some(_) => false,
            None => {
                state
                    .sessions
                    .insert(connection_id.to_string(), SessionState::new(actor));
                false
            }
        });
    }

    pub fn set_pc_connected(&self, connection_id: &str, connected: bool) {
        self.mutate_session(connection_id, |session| {
            if session.pc_connected == connected && (!connected || !session.pc_handoff) {
                false
            } else {
                session.pc_connected = connected;
                if connected {
                    session.pc_handoff = false;
                }
                true
            }
        });
    }

    pub fn begin_pc_handoff(&self, connection_id: &str) {
        self.mutate_session(connection_id, |session| {
            let keep_desktop = session.pc_connected && session.video_negotiated;
            let changed = session.pc_connected
                || session.pc_handoff != keep_desktop
                || session.system_audio_capture
                || session.remote_control;
            session.pc_connected = false;
            session.pc_handoff = keep_desktop;
            session.system_audio_capture = false;
            session.remote_control = false;
            changed
        });
    }

    pub fn mark_video_negotiated(&self, connection_id: &str) {
        self.mutate_session(connection_id, |session| {
            if session.video_negotiated {
                false
            } else {
                session.video_negotiated = true;
                true
            }
        });
    }

    pub fn set_system_audio_capture(&self, connection_id: &str, active: bool) {
        self.mutate_session(connection_id, |session| {
            if session.system_audio_capture == active {
                false
            } else {
                session.system_audio_capture = active;
                true
            }
        });
    }

    pub fn set_remote_control(&self, connection_id: &str, active: bool) {
        self.mutate_session(connection_id, |session| {
            if session.remote_control == active {
                false
            } else {
                session.remote_control = active;
                true
            }
        });
    }

    pub fn terminal_started(&self, connection_id: &str) {
        self.mutate_session(connection_id, |session| {
            if session.terminal_count == 1 {
                false
            } else {
                session.terminal_count = 1;
                true
            }
        });
    }

    pub fn terminal_closed(&self, connection_id: &str) {
        self.mutate(|state| {
            let Some(session) = state.sessions.get_mut(connection_id) else {
                return false;
            };
            let changed = session.terminal_count != 0;
            session.terminal_count = 0;
            let remove = !session.has_recorded_state();
            if remove {
                state.sessions.remove(connection_id);
            }
            changed || remove
        });
    }

    pub fn file_manager_opened(&self, connection_id: &str) {
        self.mutate_session(connection_id, |session| {
            if session.file_manager {
                false
            } else {
                session.file_manager = true;
                true
            }
        });
    }

    pub fn file_transfer_started(
        &self,
        connection_id: &str,
        transfer_id: &str,
        direction: FileTransferDirection,
        file_name: &str,
        total_bytes: u64,
    ) {
        let summary = HostFileTransferSummary {
            transfer_id: transfer_id.to_string(),
            direction: match direction {
                FileTransferDirection::Upload => HostFileTransferDirection::Upload,
                FileTransferDirection::Download => HostFileTransferDirection::Download,
            },
            file_name: sanitize_file_name(file_name),
            transferred_bytes: 0,
            total_bytes,
        };
        self.mutate_session(connection_id, |session| {
            if session.transfers.get(transfer_id) == Some(&summary) {
                false
            } else {
                session.transfers.insert(transfer_id.to_string(), summary);
                true
            }
        });
    }

    pub fn file_transfer_finished(&self, connection_id: &str, transfer_id: &str) {
        self.mutate_session(connection_id, |session| {
            session.transfers.remove(transfer_id).is_some()
        });
    }

    pub fn remove_connection(&self, connection_id: &str) {
        self.mutate(|state| state.sessions.remove(connection_id).is_some());
    }

    pub fn clear_worker_owned(&self) {
        self.mutate(|state| {
            let mut changed = false;
            for session in state.sessions.values_mut() {
                changed |= session.terminal_count != 0
                    || session.file_manager
                    || session.system_audio_capture
                    || !session.transfers.is_empty();
                session.terminal_count = 0;
                session.file_manager = false;
                session.system_audio_capture = false;
                session.transfers.clear();
            }
            state
                .sessions
                .retain(|_, session| session.has_recorded_state());
            changed
        });
    }

    fn mutate_session(&self, connection_id: &str, f: impl FnOnce(&mut SessionState) -> bool) {
        self.mutate(|state| {
            let session = state
                .sessions
                .entry(connection_id.to_string())
                .or_insert_with(|| SessionState::new(ActorSummary::unknown()));
            f(session)
        });
    }

    fn mutate(&self, f: impl FnOnce(&mut HostActivityState) -> bool) {
        let snapshot = {
            let mut state = self.inner.write().unwrap();
            let before = snapshot_from(&state);
            if !f(&mut state) {
                return;
            }
            let after_without_revision = snapshot_from(&state);
            if before.indicator_enabled == after_without_revision.indicator_enabled
                && before.remote_access == after_without_revision.remote_access
                && before.sessions == after_without_revision.sessions
            {
                return;
            }
            state.revision = state.revision.saturating_add(1);
            snapshot_from(&state)
        };
        let _ = self
            .publisher
            .send(HostControlMessage::HostAccessSnapshot { snapshot });
    }
}

fn snapshot_from(state: &HostActivityState) -> HostAccessSnapshot {
    let mut sessions: Vec<_> = state
        .sessions
        .iter()
        .map(|(connection_id, session)| session.public(connection_id))
        .filter(HostAccessSession::is_active)
        .collect();
    let total_session_count = sessions.len().min(u32::MAX as usize) as u32;
    sessions.truncate(MAX_VISIBLE_SESSIONS);
    HostAccessSnapshot {
        epoch: state.epoch.clone(),
        revision: state.revision,
        indicator_enabled: state.indicator_enabled,
        total_session_count,
        sessions,
        remote_access: state.remote_access.clone(),
    }
}

fn sanitize_actor(mut actor: ActorSummary) -> ActorSummary {
    actor.display_name = actor.display_name.and_then(|value| {
        let cleaned: String = value
            .chars()
            .filter(|ch| !ch.is_control())
            .take(MAX_DISPLAY_FIELD_CHARS)
            .collect();
        let cleaned = cleaned.trim().to_string();
        (!cleaned.is_empty()).then_some(cleaned)
    });
    actor
}

fn sanitize_file_name(file_name: &str) -> String {
    let basename = file_name.rsplit(['/', '\\']).next().unwrap_or_default();
    basename
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_DISPLAY_FIELD_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> HostActivityRegistry {
        let (tx, _) = broadcast::channel(16);
        HostActivityRegistry::new(tx)
    }

    #[test]
    fn desktop_requires_video_and_connected_in_any_order() {
        let registry = registry();
        registry.mark_video_negotiated("c1");
        assert!(registry.snapshot().sessions.is_empty());
        registry.set_pc_connected("c1", true);
        assert!(registry.snapshot().sessions[0].desktop_view);
        let revision = registry.snapshot().revision;
        registry.set_pc_connected("c1", true);
        assert_eq!(registry.snapshot().revision, revision);
    }

    #[test]
    fn control_release_preserves_desktop_view() {
        let registry = registry();
        registry.mark_video_negotiated("c1");
        registry.set_pc_connected("c1", true);
        registry.set_remote_control("c1", true);
        registry.set_remote_control("c1", false);
        let session = &registry.snapshot().sessions[0];
        assert!(session.desktop_view);
        assert!(!session.remote_control);
    }

    #[test]
    fn peer_connection_handoff_preserves_view_without_preserving_grants() {
        let registry = registry();
        registry.mark_video_negotiated("c1");
        registry.set_pc_connected("c1", true);
        registry.set_remote_control("c1", true);
        registry.set_system_audio_capture("c1", true);
        let started_at = registry.snapshot().sessions[0].started_at.clone();

        registry.begin_pc_handoff("c1");
        let handoff = registry.snapshot();
        assert_eq!(handoff.total_session_count, 1);
        assert!(handoff.sessions[0].desktop_view);
        assert!(!handoff.sessions[0].remote_control);
        assert!(!handoff.sessions[0].system_audio_capture);

        registry.set_pc_connected("c1", true);
        let reconnected = registry.snapshot();
        assert_eq!(reconnected.sessions[0].started_at, started_at);
        assert!(reconnected.sessions[0].desktop_view);
    }

    #[test]
    fn indicator_toggle_preserves_sessions_and_increments_revision() {
        let registry = registry();
        registry.terminal_started("terminal-1");
        let before = registry.snapshot();
        registry.set_indicator_enabled(false);
        let after = registry.snapshot();
        assert!(!after.indicator_enabled);
        assert_eq!(after.sessions, before.sessions);
        assert!(after.revision > before.revision);
    }

    #[test]
    fn remote_access_change_publishes_and_increments_revision() {
        let registry = registry();
        let mut receiver = registry.publisher.subscribe();
        let before = registry.snapshot();
        let status = HostRemoteAccessStatus {
            mode: crate::host_control::HostRemoteAccessMode::Locked,
            state_version: 2,
            locked_at: Some("2026-07-22T12:00:00Z".to_string()),
            durable: true,
            central_sync: crate::host_control::CentralSyncState::Pending,
        };

        registry.set_remote_access_status(status.clone());

        let after = registry.snapshot();
        assert!(after.revision > before.revision);
        assert_eq!(after.remote_access, status);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HostControlMessage::HostAccessSnapshot { snapshot }
                if snapshot.remote_access.mode
                    == crate::host_control::HostRemoteAccessMode::Locked
        ));
    }

    #[test]
    fn transfer_keys_are_scoped_by_connection() {
        let registry = registry();
        registry.file_transfer_started(
            "c1",
            "same",
            FileTransferDirection::Upload,
            "/secret/a.txt",
            10,
        );
        registry.file_transfer_started(
            "c2",
            "same",
            FileTransferDirection::Download,
            "C:\\private\\b.txt",
            20,
        );
        registry.file_transfer_finished("c1", "same");
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].connection_id, "c2");
        assert_eq!(snapshot.sessions[0].transfers[0].file_name, "b.txt");
    }

    #[test]
    fn worker_disconnect_keeps_daemon_owned_desktop() {
        let registry = registry();
        registry.mark_video_negotiated("c1");
        registry.set_pc_connected("c1", true);
        registry.terminal_started("terminal-1");
        registry.file_manager_opened("file-1");
        registry.clear_worker_owned();
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].connection_id, "c1");
        assert!(snapshot.sessions[0].desktop_view);
    }

    #[test]
    fn duplicate_terminal_events_are_idempotent() {
        let registry = registry();
        registry.terminal_started("terminal-1");
        let revision = registry.snapshot().revision;
        registry.terminal_started("terminal-1");
        assert_eq!(registry.snapshot().revision, revision);
        assert_eq!(registry.snapshot().sessions[0].terminal_count, 1);
    }

    #[test]
    fn terminal_close_removes_inactive_entry_without_creating_unknown_entry() {
        let registry = registry();
        registry.terminal_started("terminal-1");
        registry.terminal_closed("terminal-1");
        assert!(registry.snapshot().sessions.is_empty());
        assert!(registry.inner.read().unwrap().sessions.is_empty());

        registry.terminal_closed("unknown-terminal");
        assert!(registry.inner.read().unwrap().sessions.is_empty());
    }

    #[test]
    fn video_negotiation_is_monotonic_until_connection_removal() {
        let registry = registry();
        registry.mark_video_negotiated("c1");
        let revision = registry.snapshot().revision;
        registry.mark_video_negotiated("c1");
        assert_eq!(registry.snapshot().revision, revision);
        assert!(
            registry
                .inner
                .read()
                .unwrap()
                .sessions
                .get("c1")
                .unwrap()
                .video_negotiated
        );
        registry.remove_connection("c1");
        assert!(registry.inner.read().unwrap().sessions.is_empty());
    }

    #[test]
    fn snapshot_caps_details_but_preserves_total_session_count() {
        let registry = registry();
        for index in 0..(MAX_VISIBLE_SESSIONS + 3) {
            registry.terminal_started(&format!("terminal-{index:03}"));
        }
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.sessions.len(), MAX_VISIBLE_SESSIONS);
        assert_eq!(
            snapshot.total_session_count,
            (MAX_VISIBLE_SESSIONS + 3) as u32
        );
    }
}
