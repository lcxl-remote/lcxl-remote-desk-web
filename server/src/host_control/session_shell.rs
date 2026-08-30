//! Linux Tauri session registration and byte-safe environment validation.

use super::UpstreamSessionId;
use super::protocol::{
    SESSION_SHELL_PROTOCOL_VERSION, SessionShellInfo, SessionShellRegistrationError,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use uuid::Uuid;

pub const MAX_SESSION_ENVIRONMENT_ENTRIES: usize = 4_096;
pub const MAX_SESSION_ENVIRONMENT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SESSION_ENVIRONMENT_ENCODED_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_SESSION_ENVIRONMENT_ENTRY_BYTES: usize = 128 * 1024;
pub const MAX_SESSION_CWD_BYTES: usize = 64 * 1024;
/// Hard host-local bound for concurrently trusted desktop-session shells. One
/// logical session already has a single-leader constraint below; this protects
/// the daemon and resident-worker pool across many distinct sessions/seats.
pub const MAX_SESSION_SHELL_REGISTRATIONS: usize = 32;
pub const MAX_HOST_CONTROL_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_SESSION_LABEL_BYTES: usize = 256;
const MAX_APP_VERSION_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogicalSessionKey {
    pub uid: u32,
    pub session_id: Option<String>,
    pub seat: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixProcessIdentity {
    pub uid: u32,
    pub gid: u32,
    pub supplementary_groups: Vec<u32>,
    pub start_ticks: u64,
}

#[derive(Clone)]
pub struct RegisteredSessionShell {
    pub registration_id: Uuid,
    pub registration_generation: u64,
    pub websocket_session_id: UpstreamSessionId,
    pub logical_session: LogicalSessionKey,
    pub app_version: String,
    pub pid: u32,
    pub process_identity: UnixProcessIdentity,
    pub session_type: Option<String>,
    pub cwd: PathBuf,
    pub umask: u32,
    pub environment: Vec<(OsString, OsString)>,
}

impl fmt::Debug for RegisteredSessionShell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredSessionShell")
            .field("registration_id", &self.registration_id)
            .field("registration_generation", &self.registration_generation)
            .field("websocket_session_id", &self.websocket_session_id)
            .field("logical_session", &self.logical_session)
            .field("app_version", &self.app_version)
            .field("pid", &self.pid)
            .field("process_identity", &self.process_identity)
            .field("session_type", &self.session_type)
            .field("cwd", &"<redacted>")
            .field("umask", &format_args!("{:04o}", self.umask))
            .field("environment_entries", &self.environment.len())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub enum SessionShellRegistryEvent {
    Registered(Arc<RegisteredSessionShell>),
    Disconnected {
        registration_id: Uuid,
        registration_generation: u64,
        logical_session: LogicalSessionKey,
    },
}

#[derive(Debug)]
pub struct SessionShellRegistrationFailure {
    code: SessionShellRegistrationError,
    detail: String,
}

impl SessionShellRegistrationFailure {
    fn new(code: SessionShellRegistrationError, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub fn code(&self) -> SessionShellRegistrationError {
        self.code
    }
}

impl fmt::Display for SessionShellRegistrationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

struct RegistryInner {
    by_websocket: HashMap<UpstreamSessionId, Arc<RegisteredSessionShell>>,
    leader_by_logical_session: HashMap<LogicalSessionKey, UpstreamSessionId>,
}

#[derive(Clone)]
pub struct SessionShellRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    next_generation: Arc<AtomicU64>,
    events: broadcast::Sender<SessionShellRegistryEvent>,
}

impl Default for SessionShellRegistry {
    fn default() -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                by_websocket: HashMap::new(),
                leader_by_logical_session: HashMap::new(),
            })),
            next_generation: Arc::new(AtomicU64::new(1)),
            events,
        }
    }
}

impl SessionShellRegistry {
    pub fn subscribe(&self) -> broadcast::Receiver<SessionShellRegistryEvent> {
        self.events.subscribe()
    }

    pub fn register(
        &self,
        websocket_session_id: UpstreamSessionId,
        info: SessionShellInfo,
    ) -> Result<Arc<RegisteredSessionShell>, SessionShellRegistrationFailure> {
        let decoded = DecodedSessionShellInfo::decode(info)?;
        let process_identity = read_process_identity(decoded.pid)?;
        if process_identity.uid != decoded.reported_uid {
            return Err(SessionShellRegistrationFailure::new(
                SessionShellRegistrationError::IdentityMismatch,
                "reported uid differs from /proc effective uid",
            ));
        }
        if process_identity.start_ticks != decoded.process_start_ticks {
            return Err(SessionShellRegistrationFailure::new(
                SessionShellRegistrationError::IdentityMismatch,
                "reported process start ticks differ from /proc",
            ));
        }

        let logical_session = LogicalSessionKey {
            uid: process_identity.uid,
            session_id: decoded.session_id.clone(),
            seat: decoded.seat.clone(),
        };

        let mut inner = self.inner.lock().unwrap();
        if let Some(existing_websocket) = inner.leader_by_logical_session.get(&logical_session)
            && *existing_websocket != websocket_session_id
        {
            return Err(SessionShellRegistrationFailure::new(
                SessionShellRegistrationError::SessionConflict,
                "another live Tauri websocket already leads this logical session",
            ));
        }
        if !inner.by_websocket.contains_key(&websocket_session_id)
            && inner.by_websocket.len() >= MAX_SESSION_SHELL_REGISTRATIONS
        {
            return Err(SessionShellRegistrationFailure::new(
                SessionShellRegistrationError::CapacityExceeded,
                "the host reached its resident session-shell limit",
            ));
        }

        let previous = inner.by_websocket.remove(&websocket_session_id);
        if let Some(previous) = previous.as_ref() {
            inner
                .leader_by_logical_session
                .remove(&previous.logical_session);
        }

        let registration_generation = self.next_generation.fetch_add(1, Ordering::AcqRel);
        let registration = Arc::new(RegisteredSessionShell {
            registration_id: Uuid::new_v4(),
            registration_generation,
            websocket_session_id,
            logical_session: logical_session.clone(),
            app_version: decoded.app_version,
            pid: decoded.pid,
            process_identity,
            session_type: decoded.session_type,
            cwd: PathBuf::from(OsString::from_vec(decoded.cwd)),
            umask: decoded.umask,
            environment: decoded
                .environment
                .into_iter()
                .map(|(key, value)| (OsString::from_vec(key), OsString::from_vec(value)))
                .collect(),
        });
        inner
            .leader_by_logical_session
            .insert(logical_session, websocket_session_id);
        inner
            .by_websocket
            .insert(websocket_session_id, Arc::clone(&registration));
        drop(inner);

        if let Some(previous) = previous {
            let _ = self.events.send(SessionShellRegistryEvent::Disconnected {
                registration_id: previous.registration_id,
                registration_generation: previous.registration_generation,
                logical_session: previous.logical_session.clone(),
            });
        }
        let _ = self
            .events
            .send(SessionShellRegistryEvent::Registered(Arc::clone(
                &registration,
            )));
        Ok(registration)
    }

    pub fn unregister_websocket(
        &self,
        websocket_session_id: UpstreamSessionId,
    ) -> Option<Arc<RegisteredSessionShell>> {
        let removed = {
            let mut inner = self.inner.lock().unwrap();
            let removed = inner.by_websocket.remove(&websocket_session_id)?;
            inner
                .leader_by_logical_session
                .remove(&removed.logical_session);
            removed
        };
        let _ = self.events.send(SessionShellRegistryEvent::Disconnected {
            registration_id: removed.registration_id,
            registration_generation: removed.registration_generation,
            logical_session: removed.logical_session.clone(),
        });
        Some(removed)
    }

    pub fn snapshot(&self) -> Vec<Arc<RegisteredSessionShell>> {
        let mut values: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .by_websocket
            .values()
            .cloned()
            .collect();
        values.sort_by_key(|registration| registration.registration_generation);
        values
    }
}

struct DecodedSessionShellInfo {
    app_version: String,
    pid: u32,
    process_start_ticks: u64,
    reported_uid: u32,
    session_id: Option<String>,
    seat: Option<String>,
    session_type: Option<String>,
    cwd: Vec<u8>,
    umask: u32,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
}

impl DecodedSessionShellInfo {
    fn decode(info: SessionShellInfo) -> Result<Self, SessionShellRegistrationFailure> {
        if info.protocol_version != SESSION_SHELL_PROTOCOL_VERSION {
            return Err(invalid_payload(
                "unsupported session-shell protocol version",
            ));
        }
        if info.app_version.is_empty() || info.app_version.len() > MAX_APP_VERSION_BYTES {
            return Err(invalid_payload("invalid app version length"));
        }
        if info.pid == 0 || info.process_start_ticks == 0 {
            return Err(invalid_payload(
                "pid and process start ticks must be non-zero",
            ));
        }
        if info.umask > 0o777 {
            return Err(invalid_payload("umask is outside the Unix permission mask"));
        }
        validate_label("session id", info.session_id.as_deref())?;
        validate_label("seat", info.seat.as_deref())?;
        validate_label("session type", info.session_type.as_deref())?;

        let cwd = decode_bounded("cwd", &info.cwd_base64, MAX_SESSION_CWD_BYTES)?;
        if cwd.is_empty() || cwd.contains(&0) {
            return Err(invalid_payload("cwd is empty or contains NUL"));
        }
        if !PathBuf::from(OsString::from_vec(cwd.clone())).is_absolute() {
            return Err(invalid_payload("cwd is not absolute"));
        }

        if info.environment.len() > MAX_SESSION_ENVIRONMENT_ENTRIES {
            return Err(invalid_payload("environment entry count exceeds limit"));
        }
        let encoded_bytes = info.environment.iter().try_fold(0usize, |total, entry| {
            total
                .checked_add(entry.key_base64.len())
                .and_then(|value| value.checked_add(entry.value_base64.len()))
                .ok_or_else(|| invalid_payload("environment encoded size overflow"))
        })?;
        if encoded_bytes > MAX_SESSION_ENVIRONMENT_ENCODED_BYTES {
            return Err(invalid_payload("environment encoded size exceeds limit"));
        }

        let mut decoded_bytes = 0usize;
        let mut keys = HashSet::with_capacity(info.environment.len());
        let mut environment = Vec::with_capacity(info.environment.len());
        for entry in info.environment {
            let key = decode_bounded(
                "environment key",
                &entry.key_base64,
                MAX_SESSION_ENVIRONMENT_ENTRY_BYTES,
            )?;
            let value = decode_bounded(
                "environment value",
                &entry.value_base64,
                MAX_SESSION_ENVIRONMENT_ENTRY_BYTES,
            )?;
            if key.is_empty() || key.contains(&0) || key.contains(&b'=') {
                return Err(invalid_payload("environment key is invalid"));
            }
            if value.contains(&0) {
                return Err(invalid_payload("environment value contains NUL"));
            }
            if !keys.insert(key.clone()) {
                return Err(invalid_payload("environment contains a duplicate key"));
            }
            decoded_bytes = decoded_bytes
                .checked_add(key.len())
                .and_then(|total| total.checked_add(value.len()))
                .ok_or_else(|| invalid_payload("environment decoded size overflow"))?;
            if decoded_bytes > MAX_SESSION_ENVIRONMENT_BYTES {
                return Err(invalid_payload("environment decoded size exceeds limit"));
            }
            environment.push((key, value));
        }

        Ok(Self {
            app_version: info.app_version,
            pid: info.pid,
            process_start_ticks: info.process_start_ticks,
            reported_uid: info.reported_uid,
            session_id: info.session_id,
            seat: info.seat,
            session_type: info.session_type,
            cwd,
            umask: info.umask,
            environment,
        })
    }
}

fn validate_label(name: &str, value: Option<&str>) -> Result<(), SessionShellRegistrationFailure> {
    if let Some(value) = value
        && (value.is_empty() || value.len() > MAX_SESSION_LABEL_BYTES || value.contains('\0'))
    {
        return Err(invalid_payload(format!("invalid {name}")));
    }
    Ok(())
}

fn decode_bounded(
    name: &str,
    encoded: &str,
    max_decoded_bytes: usize,
) -> Result<Vec<u8>, SessionShellRegistrationFailure> {
    if encoded.len() > max_decoded_bytes.saturating_mul(4).div_ceil(3) + 4 {
        return Err(invalid_payload(format!("{name} exceeds limit")));
    }
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| invalid_payload(format!("{name} is not valid base64")))?;
    if decoded.len() > max_decoded_bytes {
        return Err(invalid_payload(format!("{name} exceeds limit")));
    }
    Ok(decoded)
}

fn invalid_payload(detail: impl Into<String>) -> SessionShellRegistrationFailure {
    SessionShellRegistrationFailure::new(SessionShellRegistrationError::InvalidPayload, detail)
}

pub(crate) fn read_process_identity(
    pid: u32,
) -> Result<UnixProcessIdentity, SessionShellRegistrationFailure> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).map_err(|error| {
        SessionShellRegistrationFailure::new(
            SessionShellRegistrationError::IdentityMismatch,
            format!("cannot read process status: {error}"),
        )
    })?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|error| {
        SessionShellRegistrationFailure::new(
            SessionShellRegistrationError::IdentityMismatch,
            format!("cannot read process stat: {error}"),
        )
    })?;

    let uid = parse_effective_id(&status, "Uid:")?;
    let gid = parse_effective_id(&status, "Gid:")?;
    let mut supplementary_groups = parse_groups(&status)?;
    supplementary_groups.sort_unstable();
    supplementary_groups.dedup();
    let start_ticks = parse_process_start_ticks(&stat)?;
    Ok(UnixProcessIdentity {
        uid,
        gid,
        supplementary_groups,
        start_ticks,
    })
}

fn parse_effective_id(status: &str, prefix: &str) -> Result<u32, SessionShellRegistrationFailure> {
    let line = status
        .lines()
        .find(|line| line.starts_with(prefix))
        .ok_or_else(|| invalid_payload(format!("process status lacks {prefix}")))?;
    line.split_ascii_whitespace()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid_payload(format!("process status has invalid {prefix}")))
}

fn parse_groups(status: &str) -> Result<Vec<u32>, SessionShellRegistrationFailure> {
    let line = status
        .lines()
        .find(|line| line.starts_with("Groups:"))
        .ok_or_else(|| invalid_payload("process status lacks Groups"))?;
    line.split_ascii_whitespace()
        .skip(1)
        .map(|value| {
            value
                .parse()
                .map_err(|_| invalid_payload("process status has invalid Groups"))
        })
        .collect()
}

fn parse_process_start_ticks(stat: &str) -> Result<u64, SessionShellRegistrationFailure> {
    let after_name = stat
        .rfind(") ")
        .and_then(|index| stat.get(index + 2..))
        .ok_or_else(|| invalid_payload("process stat lacks command terminator"))?;
    after_name
        .split_ascii_whitespace()
        .nth(19)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid_payload("process stat has invalid start ticks"))
}

#[cfg(test)]
mod tests {
    use super::super::protocol::EnvironmentEntryBase64;
    use super::*;

    fn encode(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    fn current_info(environment: Vec<(&[u8], &[u8])>) -> SessionShellInfo {
        let identity = read_process_identity(std::process::id()).unwrap();
        SessionShellInfo {
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: SESSION_SHELL_PROTOCOL_VERSION,
            pid: std::process::id(),
            process_start_ticks: identity.start_ticks,
            reported_uid: identity.uid,
            session_id: Some("test-session".to_string()),
            seat: Some("seat-test".to_string()),
            session_type: Some("wayland".to_string()),
            cwd_base64: encode(b"/tmp"),
            umask: 0o022,
            environment: environment
                .into_iter()
                .map(|(key, value)| EnvironmentEntryBase64 {
                    key_base64: encode(key),
                    value_base64: encode(value),
                })
                .collect(),
        }
    }

    #[test]
    fn registration_preserves_non_utf8_environment_bytes() {
        let registry = SessionShellRegistry::default();
        let registration = registry
            .register(7, current_info(vec![(b"BINARY", &[0xff, 0xfe])]))
            .unwrap();

        assert_eq!(registration.environment.len(), 1);
        assert_eq!(registration.environment[0].0.clone().into_vec(), b"BINARY");
        assert_eq!(
            registration.environment[0].1.clone().into_vec(),
            vec![0xff, 0xfe]
        );
        assert_eq!(registry.snapshot().len(), 1);
    }

    #[test]
    fn duplicate_environment_key_is_rejected_atomically() {
        let registry = SessionShellRegistry::default();
        let error = registry
            .register(7, current_info(vec![(b"PATH", b"/a"), (b"PATH", b"/b")]))
            .unwrap_err();

        assert_eq!(error.code(), SessionShellRegistrationError::InvalidPayload);
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn process_start_mismatch_is_rejected() {
        let registry = SessionShellRegistry::default();
        let mut info = current_info(vec![(b"PATH", b"/usr/bin")]);
        info.process_start_ticks += 1;

        let error = registry.register(7, info).unwrap_err();
        assert_eq!(
            error.code(),
            SessionShellRegistrationError::IdentityMismatch
        );
    }

    #[test]
    fn first_live_websocket_leads_a_logical_session() {
        let registry = SessionShellRegistry::default();
        registry
            .register(7, current_info(vec![(b"PATH", b"/usr/bin")]))
            .unwrap();

        let error = registry
            .register(8, current_info(vec![(b"PATH", b"/usr/local/bin")]))
            .unwrap_err();
        assert_eq!(error.code(), SessionShellRegistrationError::SessionConflict);

        registry.unregister_websocket(7).unwrap();
        registry
            .register(8, current_info(vec![(b"PATH", b"/usr/local/bin")]))
            .unwrap();
    }

    #[tokio::test]
    async fn same_websocket_reregistration_revokes_the_previous_identity_first() {
        let registry = SessionShellRegistry::default();
        let mut events = registry.subscribe();
        let first = registry
            .register(7, current_info(vec![(b"PATH", b"/usr/bin")]))
            .unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            SessionShellRegistryEvent::Registered(_)
        ));

        let second = registry
            .register(7, current_info(vec![(b"PATH", b"/usr/local/bin")]))
            .unwrap();
        match events.recv().await.unwrap() {
            SessionShellRegistryEvent::Disconnected {
                registration_id,
                registration_generation,
                logical_session,
            } => {
                assert_eq!(registration_id, first.registration_id);
                assert_eq!(registration_generation, first.registration_generation);
                assert_eq!(logical_session, first.logical_session);
            }
            other => panic!("expected previous registration disconnect, got {other:?}"),
        }
        match events.recv().await.unwrap() {
            SessionShellRegistryEvent::Registered(registration) => {
                assert_eq!(registration.registration_id, second.registration_id);
                assert!(registration.registration_generation > first.registration_generation);
            }
            other => panic!("expected replacement registration, got {other:?}"),
        }
    }

    #[test]
    fn registry_refuses_an_unbounded_number_of_logical_sessions() {
        let registry = SessionShellRegistry::default();
        for index in 0..MAX_SESSION_SHELL_REGISTRATIONS {
            let mut info = current_info(vec![(b"PATH", b"/usr/bin")]);
            info.session_id = Some(format!("bounded-session-{index}"));
            registry.register(index as u64 + 1, info).unwrap();
        }

        let mut overflow = current_info(vec![(b"PATH", b"/usr/bin")]);
        overflow.session_id = Some("bounded-session-overflow".to_string());
        let error = registry
            .register(MAX_SESSION_SHELL_REGISTRATIONS as u64 + 1, overflow)
            .unwrap_err();
        assert_eq!(
            error.code(),
            SessionShellRegistrationError::CapacityExceeded
        );
        assert_eq!(registry.snapshot().len(), MAX_SESSION_SHELL_REGISTRATIONS);

        // Replacing the same websocket does not grow the pool and must remain
        // possible at capacity so a trusted shell can refresh its snapshot.
        let mut replacement = current_info(vec![(b"PATH", b"/usr/local/bin")]);
        replacement.session_id = Some("bounded-session-0".to_string());
        registry.register(1, replacement).unwrap();
        assert_eq!(registry.snapshot().len(), MAX_SESSION_SHELL_REGISTRATIONS);
    }

    #[test]
    fn debug_output_does_not_include_environment_or_cwd() {
        let registry = SessionShellRegistry::default();
        let registration = registry
            .register(
                7,
                current_info(vec![(b"SECRET_NAME", b"secret-value-never-log")]),
            )
            .unwrap();

        let rendered = format!("{registration:?}");
        assert!(!rendered.contains("secret-value-never-log"));
        assert!(!rendered.contains("SECRET_NAME"));
        assert!(!rendered.contains("/tmp"));
        assert!(rendered.contains("environment_entries: 1"));
    }
}
