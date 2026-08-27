//! Connection-scoped dynamic Computer Use readiness for the OSS signal server.
//! The cache intentionally has no Redis dependency: OSS is single-process, and
//! the authenticated signaling connection itself is the presence fence.

use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use chrono::{DateTime, Duration, Utc};
use desk_agent_protocol::computer_use::ComputerUseReadiness;
use desk_server_version::supports_computer_use;
use desk_signal_facade::{
    model::{
        connection::ConnectionState,
        signal::{RemoteDeskTypeEnum, SignalingModel},
    },
    service::ComputerUseReadinessObserver,
};

const MAX_REPORTED_VALIDITY_SECS: i64 = 300;
const MAX_OBSERVED_FUTURE_SKEW_SECS: i64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedComputerUseReadiness {
    pub connection_id: String,
    pub readiness: ComputerUseReadiness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheUpdateOutcome {
    Stored,
    StaleRevision,
}

#[derive(Default)]
pub struct ComputerUseReadinessCache {
    entries: Mutex<HashMap<String, CachedComputerUseReadiness>>,
}

impl ComputerUseReadinessCache {
    pub fn update(
        &self,
        connection_id: &str,
        readiness: ComputerUseReadiness,
        now: DateTime<Utc>,
    ) -> Result<CacheUpdateOutcome, String> {
        readiness.validate().map_err(|e| e.to_string())?;
        validate_readiness_time(&readiness, now)?;
        let mut entries = self.lock();
        if let Some(existing) = entries.get(connection_id) {
            if existing.readiness.revision > readiness.revision {
                return Ok(CacheUpdateOutcome::StaleRevision);
            }
            if existing.readiness.revision == readiness.revision {
                if !same_material_readiness(&existing.readiness, &readiness) {
                    return Err(
                        "readiness changed material state without advancing revision".into(),
                    );
                }
                let existing_observed =
                    DateTime::parse_from_rfc3339(&existing.readiness.observed_at)
                        .map_err(|_| "existing readiness observed_at is invalid")?;
                let incoming_observed = DateTime::parse_from_rfc3339(&readiness.observed_at)
                    .map_err(|_| "readiness observed_at is invalid")?;
                if incoming_observed <= existing_observed {
                    return Ok(CacheUpdateOutcome::StaleRevision);
                }
            }
        }
        let first_inventory_for_connection = !entries.contains_key(connection_id);
        if first_inventory_for_connection {
            let summary = readiness
                .capabilities
                .iter()
                .map(|entry| {
                    format!(
                        "{:?}:ready={}:reason={:?}",
                        entry.capability, entry.ready, entry.reason
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            if readiness.capabilities.iter().any(|entry| !entry.ready) {
                log::warn!(
                    "[capability-inventory] connection={} revision={} has blocked capabilities=[{}]",
                    connection_id,
                    readiness.revision,
                    summary
                );
            } else {
                log::info!(
                    "[capability-inventory] connection={} revision={} capabilities=[{}]",
                    connection_id,
                    readiness.revision,
                    summary
                );
            }
        }
        entries.insert(
            connection_id.to_string(),
            CachedComputerUseReadiness {
                connection_id: connection_id.to_string(),
                readiness,
            },
        );
        Ok(CacheUpdateOutcome::Stored)
    }

    /// A caller must still resolve `connection_id` through the live connection
    /// map. This method provides TTL/revision state, not connection liveness.
    pub fn get_fresh(
        &self,
        connection_id: &str,
        now: DateTime<Utc>,
    ) -> Option<CachedComputerUseReadiness> {
        let mut entries = self.lock();
        let entry = entries.get(connection_id)?.clone();
        if validate_readiness_time(&entry.readiness, now).is_err() {
            entries.remove(connection_id);
            None
        } else {
            Some(entry)
        }
    }

    pub fn remove_connection(&self, connection_id: &str) {
        self.lock().remove(connection_id);
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, CachedComputerUseReadiness>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn same_material_readiness(left: &ComputerUseReadiness, right: &ComputerUseReadiness) -> bool {
    left.schema_version == right.schema_version
        && left.revision == right.revision
        && left.server_api_version == right.server_api_version
        && left.os == right.os
        && left.interactive_session_incarnation == right.interactive_session_incarnation
        && left.local_ceiling_revision == right.local_ceiling_revision
        && left.capabilities == right.capabilities
        && left
            .context_references
            .iter()
            .map(|reference| (reference.capability, reference.object_ref.object_kind))
            .eq(right
                .context_references
                .iter()
                .map(|reference| (reference.capability, reference.object_ref.object_kind)))
}

pub fn global_computer_use_readiness_cache() -> Arc<ComputerUseReadinessCache> {
    static CACHE: OnceLock<Arc<ComputerUseReadinessCache>> = OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(ComputerUseReadinessCache::default()))
        .clone()
}

pub struct SignalComputerUseReadinessObserver {
    cache: Arc<ComputerUseReadinessCache>,
}

impl SignalComputerUseReadinessObserver {
    pub fn new(cache: Arc<ComputerUseReadinessCache>) -> Self {
        Self { cache }
    }
}

impl ComputerUseReadinessObserver for SignalComputerUseReadinessObserver {
    fn on_computer_use_readiness<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if source.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
                || source.model.version_info.remote_desk_type != RemoteDeskTypeEnum::Server
            {
                log::warn!("dropping Computer Use readiness from a non-server connection");
                return;
            }
            let readiness = match model.get_data::<ComputerUseReadiness>() {
                Ok(readiness) => readiness,
                Err(e) => {
                    log::warn!("dropping malformed Computer Use readiness: {e}");
                    return;
                }
            };
            if !supports_computer_use(source.model.version_info.api_version)
                || readiness.server_api_version != source.model.version_info.api_version
            {
                log::warn!("dropping Computer Use readiness with incompatible server API version");
                return;
            }
            match self
                .cache
                .update(&source.model.connection_id, readiness, Utc::now())
            {
                Ok(CacheUpdateOutcome::Stored) => {}
                Ok(CacheUpdateOutcome::StaleRevision) => log::debug!(
                    "dropping stale Computer Use readiness from {}",
                    source.model.connection_id
                ),
                Err(e) => log::warn!("dropping invalid Computer Use readiness: {e}"),
            }
        })
    }
}

fn validate_readiness_time(
    readiness: &ComputerUseReadiness,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let observed = DateTime::parse_from_rfc3339(&readiness.observed_at)
        .map_err(|_| "observed_at is not RFC3339".to_string())?
        .with_timezone(&Utc);
    let expires = DateTime::parse_from_rfc3339(&readiness.expires_at)
        .map_err(|_| "expires_at is not RFC3339".to_string())?
        .with_timezone(&Utc);
    if expires <= now || expires <= observed {
        return Err("readiness is expired or has a non-positive validity window".to_string());
    }
    if expires - observed > Duration::seconds(MAX_REPORTED_VALIDITY_SECS) {
        return Err("readiness validity window is too long".to_string());
    }
    if observed > now + Duration::seconds(MAX_OBSERVED_FUTURE_SKEW_SECS) {
        return Err("observed_at is too far in the future".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{
        Capability, capability_provider::CapabilityBlockedReason,
        computer_use::ComputerUseReadinessReason,
    };
    use desk_diagnose_core::device_assistant::{
        DESKTOP_UI_CAPABILITY_ID, DESKTOP_UI_PROVIDER_ID, WINDOWS_UIA_ADAPTER_ID,
        provider_readiness_reports,
    };

    fn readiness(revision: u64, incarnation: &str) -> ComputerUseReadiness {
        ComputerUseReadiness {
            schema_version: desk_agent_protocol::computer_use::COMPUTER_USE_SCHEMA_VERSION,
            revision,
            observed_at: "2026-08-23T11:59:55Z".to_string(),
            expires_at: "2026-08-23T12:00:30Z".to_string(),
            server_api_version: desk_server_version::SERVER_API_VERSION,
            os: "windows".to_string(),
            interactive_session_incarnation: incarnation.to_string(),
            local_ceiling_revision: 1,
            capabilities: Vec::new(),
            context_references: Vec::new(),
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn revisions_are_monotonic_across_worker_incarnations() {
        let cache = ComputerUseReadinessCache::default();
        assert_eq!(
            cache.update("connection", readiness(10, "worker-a"), now()),
            Ok(CacheUpdateOutcome::Stored)
        );
        assert_eq!(
            cache.update("connection", readiness(11, "worker-b"), now()),
            Ok(CacheUpdateOutcome::Stored)
        );
        assert_eq!(
            cache.update("connection", readiness(10, "worker-a"), now()),
            Ok(CacheUpdateOutcome::StaleRevision)
        );
        assert_eq!(
            cache
                .get_fresh("connection", now())
                .unwrap()
                .readiness
                .interactive_session_incarnation,
            "worker-b"
        );
    }

    #[test]
    fn equivalent_same_revision_heartbeat_refreshes_expiry() {
        let cache = ComputerUseReadinessCache::default();
        cache
            .update("connection", readiness(10, "worker-a"), now())
            .unwrap();
        let mut heartbeat = readiness(10, "worker-a");
        heartbeat.observed_at = "2026-08-23T12:00:05Z".into();
        heartbeat.expires_at = "2026-08-23T12:00:40Z".into();
        let heartbeat_now = DateTime::parse_from_rfc3339("2026-08-23T12:00:10Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            cache.update("connection", heartbeat, heartbeat_now),
            Ok(CacheUpdateOutcome::Stored)
        );
        let after_original_expiry = DateTime::parse_from_rfc3339("2026-08-23T12:00:35Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(
            cache
                .get_fresh("connection", after_original_expiry)
                .is_some()
        );
    }

    #[test]
    fn same_revision_cannot_change_material_readiness() {
        let cache = ComputerUseReadinessCache::default();
        cache
            .update("connection", readiness(10, "worker-a"), now())
            .unwrap();
        let mut changed = readiness(10, "worker-b");
        changed.observed_at = "2026-08-23T12:00:05Z".into();
        changed.expires_at = "2026-08-23T12:00:40Z".into();
        let heartbeat_now = DateTime::parse_from_rfc3339("2026-08-23T12:00:10Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(
            cache
                .update("connection", changed, heartbeat_now)
                .unwrap_err()
                .contains("without advancing revision")
        );
    }

    #[test]
    fn cache_is_connection_scoped_and_expires() {
        let cache = ComputerUseReadinessCache::default();
        cache
            .update("old-connection", readiness(99, "worker-a"), now())
            .unwrap();
        cache
            .update("new-connection", readiness(1, "worker-b"), now())
            .unwrap();
        assert!(cache.get_fresh("new-connection", now()).is_some());
        assert!(
            cache
                .get_fresh(
                    "old-connection",
                    DateTime::parse_from_rfc3339("2026-08-23T12:01:00Z")
                        .unwrap()
                        .with_timezone(&Utc)
                )
                .is_none()
        );
    }

    #[test]
    fn disconnect_removes_only_its_entry() {
        let cache = ComputerUseReadinessCache::default();
        cache
            .update("one", readiness(1, "worker-a"), now())
            .unwrap();
        cache
            .update("two", readiness(1, "worker-b"), now())
            .unwrap();
        cache.remove_connection("one");
        assert!(cache.get_fresh("one", now()).is_none());
        assert!(cache.get_fresh("two", now()).is_some());
    }

    #[test]
    fn existing_readiness_projects_to_generic_provider_identity() {
        let mut source = readiness(7, "worker-a");
        source.capabilities = vec![
            desk_agent_protocol::computer_use::ComputerUseCapabilityReadiness {
                capability: Capability::DesktopUiInspect,
                adapter: desk_agent_protocol::computer_use::ComputerUseAdapterRef {
                    kind: desk_agent_protocol::computer_use::ComputerUseAdapterKind::WindowsUia,
                    version: "a4-windows-uia-read/v1".into(),
                },
                supported: true,
                ready: false,
                reason: Some(ComputerUseReadinessReason::PermissionMissing),
            },
        ];
        let reports = provider_readiness_reports(&source).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].provider_id, DESKTOP_UI_PROVIDER_ID);
        assert_eq!(reports[0].capability_id, DESKTOP_UI_CAPABILITY_ID);
        assert_eq!(
            reports[0].adapter_id.as_deref(),
            Some(WINDOWS_UIA_ADAPTER_ID)
        );
        assert_eq!(
            reports[0].reason,
            Some(CapabilityBlockedReason::PermissionMissing)
        );
        assert!(!reports[0].ready);
    }
}
