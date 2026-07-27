//! The one place a permission gate reads the host security policy from.
//!
//! Two roles hold the policy differently. The daemon owns it: it reads the
//! authoritative copy and commits a remembered answer directly. A session
//! worker only mirrors what the daemon publishes — its own `Settings` are a
//! startup copy, so writing a remembered answer through them would push a stale
//! snapshot back over the file. It sends the answer upstream instead.
//!
//! Gates take a [`PolicyAccess`] rather than either of those, so the same gate
//! code is correct in both roles and neither can read the policy from somewhere
//! that does not receive updates.

use std::sync::Arc;

use desk_ipc_protocol::message::{RememberSecurityDecisionPayload, WorkerToService};
use desk_signal_facade::model::policy_snapshot::CapabilityState;
use desk_signal_facade::model::security_settings::{SecurityPermissionType, SecuritySettings};
use tokio::sync::mpsc;

use crate::model::settings_coordinator::SettingsCoordinator;
use crate::worker::policy_mirror::PolicyMirror;

/// A cached permission decision, tagged with the policy it was decided under.
///
/// The tag is what makes invalidation a read-time question. Clearing caches
/// when the policy changes would mean reaching into state the main loop owns
/// while it is parked on an approval prompt — which is exactly when a policy
/// change matters most and exactly when that state cannot be borrowed. A gate
/// compares the tag instead and treats a mismatch as a miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedDecision {
    pub approved: bool,
    /// The capability's stamp when this answer was decided.
    pub decided_at: u64,
}

impl CachedDecision {
    /// Whether this answer still describes the policy in force.
    pub fn is_current(&self, generation: u64) -> bool {
        self.decided_at == generation
    }
}

/// How this process reaches the host security policy.
enum PolicyRole {
    /// The daemon holds it.
    Authoritative(Arc<SettingsCoordinator>),
    /// A session worker mirrors it and sends remembered answers upstream.
    Mirrored {
        mirror: Arc<PolicyMirror>,
        upstream: mpsc::UnboundedSender<WorkerToService>,
    },
}

/// A gate's handle on the host security policy.
pub struct PolicyAccess {
    role: PolicyRole,
}

impl std::fmt::Debug for PolicyAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let role = match &self.role {
            PolicyRole::Authoritative(_) => "authoritative",
            PolicyRole::Mirrored { .. } => "mirrored",
        };
        f.debug_struct("PolicyAccess").field("role", &role).finish()
    }
}

impl PolicyAccess {
    /// For the daemon, which owns the policy.
    pub fn authoritative(coordinator: Arc<SettingsCoordinator>) -> Arc<Self> {
        Arc::new(Self {
            role: PolicyRole::Authoritative(coordinator),
        })
    }

    /// For a session worker, which follows the daemon.
    pub fn mirrored(
        mirror: Arc<PolicyMirror>,
        upstream: mpsc::UnboundedSender<WorkerToService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            role: PolicyRole::Mirrored { mirror, upstream },
        })
    }

    /// A worker-role handle over a policy a test controls directly.
    ///
    /// Returns the mirror so the test can publish a change mid-flight, and the
    /// upstream receiver so a remembered answer is observable.
    #[cfg(test)]
    pub fn for_test(
        security: SecuritySettings,
    ) -> (
        Arc<Self>,
        Arc<PolicyMirror>,
        mpsc::UnboundedReceiver<WorkerToService>,
    ) {
        use desk_signal_facade::model::policy_snapshot::PolicySnapshot;
        let mirror = Arc::new(PolicyMirror::new(PolicySnapshot::new(security)));
        let (tx, rx) = mpsc::unbounded_channel();
        (Self::mirrored(Arc::clone(&mirror), tx), mirror, rx)
    }

    /// The host global for one capability, before any per-connection ceiling,
    /// together with the stamp that value carries.
    ///
    /// There is deliberately no way to ask for one without the other. A gate
    /// that read the setting, waited, and then read the stamp would decide from
    /// a policy that no longer exists and file the answer under the one that
    /// replaced it: an operator's fresh "always deny" would be prompted past,
    /// and a remembered approval would pass the upstream compare-and-set that
    /// exists precisely to reject it.
    pub fn capability(&self, capability: SecurityPermissionType) -> CapabilityState {
        match &self.role {
            PolicyRole::Authoritative(coordinator) => coordinator.capability(capability),
            PolicyRole::Mirrored { mirror, .. } => mirror.capability(capability),
        }
    }

    /// How long the host waits for the user to answer a prompt.
    pub fn approval_timeout(&self) -> Option<u32> {
        match &self.role {
            PolicyRole::Authoritative(coordinator) => coordinator.approval_timeout(),
            PolicyRole::Mirrored { mirror, .. } => mirror.security().approval_timeout,
        }
    }

    /// The whole policy, for the paths that report it rather than gate on one
    /// capability.
    pub fn security(&self) -> SecuritySettings {
        match &self.role {
            PolicyRole::Authoritative(coordinator) => coordinator.security(),
            PolicyRole::Mirrored { mirror, .. } => mirror.security(),
        }
    }

    /// Commit a remembered answer, wherever this role commits it.
    ///
    /// `expected_generation` is the capability's stamp from when the prompt went
    /// out; the authoritative side refuses an answer whose capability has moved
    /// since. A worker forwards the pair and the daemon applies the same rule,
    /// so both roles converge on one decision rather than each keeping half of
    /// the check.
    pub async fn remember(
        &self,
        capability: SecurityPermissionType,
        approved: bool,
        expected_generation: u64,
    ) {
        match &self.role {
            PolicyRole::Authoritative(coordinator) => {
                coordinator
                    .remember(capability, approved, expected_generation)
                    .await
                    .report(capability);
            }
            PolicyRole::Mirrored { upstream, .. } => {
                if upstream
                    .send(WorkerToService::RememberSecurityDecision(
                        RememberSecurityDecisionPayload {
                            capability,
                            approved,
                            expected_generation,
                        },
                    ))
                    .is_err()
                {
                    log::error!(
                        "[security] the remembered answer for {capability:?} could not be sent \
                         to the host; the capability keeps its current setting"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::{Settings, SharedSettings};
    use desk_signal_facade::model::policy_snapshot::PolicySnapshot;

    fn coordinator_at(path: &std::path::Path) -> Arc<SettingsCoordinator> {
        let mut settings = Settings::default();
        settings.args.config_file_path = path.to_string_lossy().into_owned();
        let security = settings.security.clone();
        Arc::new(SettingsCoordinator::new(
            Arc::new(SharedSettings::from(settings)),
            security,
        ))
    }

    /// A cached answer survives changes to other capabilities and only becomes
    /// a miss when its own moves.
    #[test]
    fn a_cached_answer_expires_with_its_own_capability() {
        let cached = CachedDecision {
            approved: true,
            decided_at: 7,
        };
        assert!(cached.is_current(7));
        assert!(!cached.is_current(9));
    }

    /// The daemon reads what it has committed.
    #[tokio::test]
    async fn the_authoritative_role_reads_the_committed_policy() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));
        let access = PolicyAccess::authoritative(Arc::clone(&coordinator));
        coordinator
            .commit(|settings| {
                settings.security.allow_terminal = Some(false);
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(
            access
                .capability(SecurityPermissionType::Terminal)
                .permission,
            Some(false)
        );
        assert_eq!(
            access
                .capability(SecurityPermissionType::Terminal)
                .generation,
            coordinator.seq()
        );
    }

    /// A worker reads what was published to it, not what it started with.
    #[tokio::test]
    async fn the_mirrored_role_reads_the_published_policy() {
        let mirror = Arc::new(PolicyMirror::new(PolicySnapshot::new(
            SecuritySettings::default(),
        )));
        let (tx, _rx) = mpsc::unbounded_channel();
        let access = PolicyAccess::mirrored(Arc::clone(&mirror), tx);
        assert_eq!(
            access
                .capability(SecurityPermissionType::Whiteboard)
                .permission,
            None
        );

        let mut published = PolicySnapshot::new(SecuritySettings::default());
        published.set_capability(SecurityPermissionType::Whiteboard, Some(false));
        mirror.apply(published);

        assert_eq!(
            access
                .capability(SecurityPermissionType::Whiteboard)
                .permission,
            Some(false)
        );
    }

    /// A worker never writes the policy itself — the answer goes to the host,
    /// carrying the stamp the host needs to judge it.
    #[tokio::test]
    async fn a_mirrored_remember_travels_upstream() {
        let mirror = Arc::new(PolicyMirror::new(PolicySnapshot::new(
            SecuritySettings::default(),
        )));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let access = PolicyAccess::mirrored(mirror, tx);

        access
            .remember(SecurityPermissionType::FileDelete, false, 3)
            .await;

        match rx.try_recv().expect("an upstream message") {
            WorkerToService::RememberSecurityDecision(payload) => {
                assert_eq!(payload.capability, SecurityPermissionType::FileDelete);
                assert!(!payload.approved);
                assert_eq!(payload.expected_generation, 3);
            }
            other => panic!("unexpected upstream message: {other:?}"),
        }
    }

    /// The daemon commits its own remembered answers rather than sending them
    /// anywhere.
    #[tokio::test]
    async fn an_authoritative_remember_is_committed_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));
        let access = PolicyAccess::authoritative(Arc::clone(&coordinator));
        let generation = access
            .capability(SecurityPermissionType::ClipboardSync)
            .generation;

        access
            .remember(SecurityPermissionType::ClipboardSync, true, generation)
            .await;

        assert_eq!(
            coordinator.permission(SecurityPermissionType::ClipboardSync),
            Some(true)
        );
    }
}
