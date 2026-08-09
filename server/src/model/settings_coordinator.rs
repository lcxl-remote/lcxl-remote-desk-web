//! The host's single durable commit path for the security policy and the
//! locale.
//!
//! Everything else about the settings — encoder knobs, TURN, telemetry consent —
//! stays with whichever handler owns it: those values are read where they are
//! written and never have to agree with a second process. The two settled here
//! do: a session worker enforces the security policy and renders text in the
//! host locale, so both have to reach it, and both have a process-wide effect
//! that must not happen before the value is durable.
//!
//! A commit runs as one transaction — build a candidate, persist it, and only
//! then let anything observe the change. A caller that sees an error can treat
//! the host as untouched.

use std::sync::{OnceLock, RwLock as StdRwLock};

use desk_ipc_protocol::message::{ServiceToWorker, SetLocalePayload};
use desk_signal_facade::model::policy_snapshot::{CapabilityState, PolicySnapshot};
use desk_signal_facade::model::security_settings::{SecurityPermissionType, SecuritySettings};
use desk_utils::error::DeskErrorCode;
use tokio::sync::Mutex as TokioMutex;

use crate::daemon::worker_manager::WorkerManager;
use crate::error::DeskError;
use crate::model::settings::{Settings, SharedSettings};

/// How long the daemon waits for a worker to confirm a published policy before
/// saying so. Generous relative to the round trip (an unbounded mpsc send plus
/// one match arm on the worker's reader task) but short enough that a stuck
/// worker is reported while the operator is still looking at the console.
const POLICY_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What a successful commit changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    /// The locale now in force process-wide, when this commit moved it.
    pub locale_changed_to: Option<String>,
    /// The security policy sequence after the commit. Unchanged commits report
    /// the sequence that was already current.
    pub seq: u64,
    /// Whether the security policy itself moved.
    pub policy_changed: bool,
}

/// What became of a remembered approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RememberOutcome {
    /// The capability now carries the remembered answer.
    Committed,
    /// The capability moved while the user was being asked, so the answer
    /// belongs to a policy that no longer exists. The operator's newer decision
    /// stands.
    Superseded,
    /// The answer could not be persisted. The capability keeps its previous
    /// value; the request the user answered was still honored.
    Failed(String),
}

impl RememberOutcome {
    /// Say what happened to a remembered answer.
    ///
    /// Worth logging even when nothing went wrong on the host: the user ticked
    /// a box and, in two of the three cases, the setting they expected did not
    /// move. Without this the only visible symptom is being asked again later.
    pub fn report(&self, capability: SecurityPermissionType) {
        match self {
            RememberOutcome::Committed => {}
            RememberOutcome::Superseded => log::warn!(
                "[security] not storing the remembered answer for {capability:?}: the capability \
                 changed while the user was deciding, and the newer setting stands"
            ),
            RememberOutcome::Failed(error) => log::error!(
                "[security] failed to store the remembered answer for {capability:?}: {error}"
            ),
        }
    }
}

/// The authoritative holder of the host security policy and the only place the
/// locale is committed.
pub struct SettingsCoordinator {
    settings: std::sync::Arc<SharedSettings>,
    /// The policy as published. Kept beside the settings rather than inside
    /// them because it carries the sequence and per-capability stamps that make
    /// distribution safe, none of which belong in the configuration file.
    ///
    /// A synchronous lock, so a permission gate can read the policy without
    /// awaiting anything — gates run on the hot path of every remote command.
    policy: StdRwLock<PolicySnapshot>,
    /// Where a committed policy is published. Bound once the worker manager
    /// exists; before that (and in a host that never runs one) a commit is
    /// durable and live but has nobody to notify.
    worker_manager: OnceLock<WorkerManager>,
    /// Serializes commits so two of them cannot each read the settings, build a
    /// candidate from it, and write back over the other.
    commit_lock: TokioMutex<()>,
}

impl std::fmt::Debug for SettingsCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsCoordinator")
            .field("seq", &self.seq())
            .finish_non_exhaustive()
    }
}

impl SettingsCoordinator {
    pub fn new(settings: std::sync::Arc<SharedSettings>, initial: SecuritySettings) -> Self {
        Self {
            settings,
            policy: StdRwLock::new(PolicySnapshot::new(initial)),
            worker_manager: OnceLock::new(),
            commit_lock: TokioMutex::new(()),
        }
    }

    /// Build one from the settings it will own, reading the starting policy out
    /// of them.
    pub async fn from_settings(settings: std::sync::Arc<SharedSettings>) -> Self {
        let initial = settings.read().await.security.clone();
        Self::new(settings, initial)
    }

    /// Name the worker manager that published policies go to. Called once, as
    /// soon as the daemon has one.
    pub fn bind_worker_manager(&self, manager: WorkerManager) {
        let _ = self.worker_manager.set(manager);
    }

    /// Return the currently bound desktop worker manager for local host actions.
    pub fn worker_manager(&self) -> Option<WorkerManager> {
        self.worker_manager.get().cloned()
    }

    pub fn snapshot(&self) -> PolicySnapshot {
        self.policy.read().expect("settings coordinator").clone()
    }

    pub fn seq(&self) -> u64 {
        self.policy.read().expect("settings coordinator").seq()
    }

    /// One capability's setting and its stamp, taken under a single read so a
    /// concurrent commit cannot land between them.
    pub fn capability(&self, capability: SecurityPermissionType) -> CapabilityState {
        self.policy
            .read()
            .expect("settings coordinator")
            .capability(capability)
    }

    pub fn permission(&self, capability: SecurityPermissionType) -> Option<bool> {
        self.capability(capability).permission
    }

    pub fn changed_at(&self, capability: SecurityPermissionType) -> u64 {
        self.capability(capability).generation
    }

    pub fn approval_timeout(&self) -> Option<u32> {
        self.policy
            .read()
            .expect("settings coordinator")
            .security()
            .approval_timeout
    }

    pub fn security(&self) -> SecuritySettings {
        self.policy
            .read()
            .expect("settings coordinator")
            .security()
            .clone()
    }

    /// Commit a change to the settings.
    ///
    /// `change` receives a candidate copy: it may edit anything, and returning
    /// an error abandons the whole commit with the host untouched. The
    /// candidate is persisted before it becomes the live settings, so a failed
    /// write leaves both the file and the process on the previous values.
    ///
    /// The locale is canonicalized here rather than trusted from the caller,
    /// and the process-wide locale only moves once the file is on disk.
    pub async fn commit<F>(&self, change: F) -> Result<CommitOutcome, DeskError>
    where
        F: FnOnce(&mut Settings) -> Result<(), DeskError>,
    {
        self.commit_with_effect(change, |_| {}).await
    }

    /// Commit a change and run `effect` against the new settings before anyone
    /// else can read them.
    ///
    /// For the state that has to move in lockstep with the settings — the
    /// manager-link gate is the one that matters: a reconnect loop that saw new
    /// settings while the gate still held the old answer would dial the manager
    /// the operator has just disabled. Running the effect after the write lock
    /// is released would leave exactly that gap; running it inside `change`
    /// would apply it even when the commit is abandoned.
    pub async fn commit_with_effect<F, E>(
        &self,
        change: F,
        effect: E,
    ) -> Result<CommitOutcome, DeskError>
    where
        F: FnOnce(&mut Settings) -> Result<(), DeskError>,
        E: FnOnce(&Settings),
    {
        let _serial = self.commit_lock.lock().await;
        self.commit_locked(change, effect).await
    }

    /// Make a remembered approval the standing answer for one capability.
    ///
    /// `expected_generation` is the capability's stamp as it stood when the
    /// prompt went out. A mismatch means the operator changed that capability
    /// while the user was deciding, so the remembered answer is dropped rather
    /// than allowed to undo the newer decision. Comparing stamps rather than
    /// values is what makes a revoke-then-restore round trip visible: the value
    /// is back where it started but the stamp has moved.
    pub async fn remember(
        &self,
        capability: SecurityPermissionType,
        approved: bool,
        expected_generation: u64,
    ) -> RememberOutcome {
        let _serial = self.commit_lock.lock().await;
        if self.changed_at(capability) != expected_generation {
            return RememberOutcome::Superseded;
        }
        let result = self
            .commit_locked(
                |settings| {
                    capability.write(&mut settings.security, Some(approved));
                    Ok(())
                },
                |_| {},
            )
            .await;
        match result {
            Ok(_) => RememberOutcome::Committed,
            Err(error) => RememberOutcome::Failed(error.to_string()),
        }
    }

    /// Publish the policy the daemon currently holds. Used when a worker
    /// appears: it starts from whatever policy was serialized into its Init
    /// payload, which is the right values but sequence zero, so the daemon has
    /// to re-state the current one for the worker to be comparable again.
    pub async fn republish(&self) {
        let snapshot = self.snapshot();
        self.publish(snapshot).await;
    }

    async fn commit_locked<F, E>(&self, change: F, effect: E) -> Result<CommitOutcome, DeskError>
    where
        F: FnOnce(&mut Settings) -> Result<(), DeskError>,
        E: FnOnce(&Settings),
    {
        let (outcome, published) = {
            let mut live = self.settings.write().await;
            let mut candidate = live.clone();
            change(&mut candidate)?;

            // Normalize before persisting so the file never carries a locale
            // spelling or an approval timeout that would reload as something
            // else.
            candidate.system.locale = match candidate.system.locale.as_deref() {
                Some(locale) => match crate::locale::canonicalize(locale) {
                    Some(canonical) => Some(canonical.to_string()),
                    None => {
                        return DeskError::custom_error(
                            DeskErrorCode::INVALID_PARAMS,
                            "unsupported locale",
                        );
                    }
                },
                None => None,
            };
            candidate.security.normalize();

            // The one durable write. Everything below this line assumes the
            // file already holds the candidate.
            candidate.save()?;

            let locale_moved = candidate.system.locale != live.system.locale;
            let applied_locale = candidate
                .system
                .locale
                .clone()
                .unwrap_or_else(|| crate::locale::DEFAULT_LOCALE.to_string());
            let security = candidate.security.clone();
            *live = candidate;

            let (policy_changed, seq, snapshot) = {
                let mut policy = self.policy.write().expect("settings coordinator");
                let changed = policy.set(security);
                (changed, policy.seq(), policy.clone())
            };

            if locale_moved {
                crate::locale::set_global_locale(&applied_locale)
                    .expect("the candidate locale was canonicalized before persisting");
            }
            effect(&live);

            (
                CommitOutcome {
                    locale_changed_to: locale_moved.then_some(applied_locale),
                    seq,
                    policy_changed,
                },
                policy_changed.then_some(snapshot),
            )
            // The settings write guard drops here, before anything is sent to
            // the worker. `WorkerManager::start_worker` takes its own lock and
            // then the settings lock; publishing under the settings lock would
            // take them in the opposite order and the two could deadlock.
        };

        if let Some(snapshot) = published {
            self.publish(snapshot).await;
        }
        if let Some(locale) = outcome.locale_changed_to.as_deref() {
            self.send_locale(locale).await;
        }
        Ok(outcome)
    }

    async fn publish(&self, snapshot: PolicySnapshot) {
        let Some(manager) = self.worker_manager.get() else {
            return;
        };
        manager
            .publish_security_policy(snapshot, POLICY_ACK_TIMEOUT)
            .await;
    }

    /// Tell the worker which locale the host now runs in. Every entry point
    /// that changes the locale goes through a commit, so this is the one place
    /// the instruction is sent and no caller can forget it.
    async fn send_locale(&self, locale: &str) {
        let Some(manager) = self.worker_manager.get() else {
            return;
        };
        if let Err(error) = manager
            .send_to_worker(ServiceToWorker::SetLocale(SetLocalePayload {
                operation_id: uuid::Uuid::new_v4().to_string(),
                locale: locale.to_string(),
            }))
            .await
        {
            log::debug!(
                "[settings] no live worker to tell about the locale change to {locale}: {error}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn coordinator_at(path: &std::path::Path) -> SettingsCoordinator {
        let mut settings = Settings::for_test_config(path);
        settings.system.locale = Some("zh-CN".to_string());
        let security = settings.security.clone();
        SettingsCoordinator::new(Arc::new(SharedSettings::from(settings)), security)
    }

    /// The point of the whole type: a capability the operator sets is visible
    /// to every reader, stamped, and on disk.
    #[tokio::test]
    async fn a_committed_capability_is_live_stamped_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));

        let outcome = coordinator
            .commit(|settings| {
                settings.security.allow_file_delete = Some(false);
                Ok(())
            })
            .await
            .unwrap();

        assert!(outcome.policy_changed);
        assert_eq!(
            coordinator.permission(SecurityPermissionType::FileDelete),
            Some(false)
        );
        assert_eq!(
            coordinator.changed_at(SecurityPermissionType::FileDelete),
            outcome.seq
        );
        let args = coordinator.settings.read().await.args.clone();
        assert_eq!(
            Settings::load_readonly(&args)
                .unwrap()
                .security
                .allow_file_delete,
            Some(false)
        );
    }

    /// A setting outside the security policy moves nothing the workers track.
    ///
    /// The sequence numbers a policy, not the settings file, so a change the
    /// policy cannot see is not a change to it: neither the sequence nor any
    /// capability stamp moves. That matters because a moved stamp is how a
    /// worker is told an approval the user already gave is no longer valid —
    /// turning the log level up must not make anyone answer a prompt again.
    #[tokio::test]
    async fn a_change_outside_the_policy_disturbs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));
        coordinator
            .commit(|settings| {
                settings.security.allow_terminal = Some(true);
                Ok(())
            })
            .await
            .unwrap();
        let seq = coordinator.seq();
        let stamps: Vec<u64> = SecurityPermissionType::ALL
            .iter()
            .map(|c| coordinator.changed_at(*c))
            .collect();

        coordinator
            .commit(|settings| {
                settings.log.log_level = "debug".to_string();
                Ok(())
            })
            .await
            .unwrap();

        let after: Vec<u64> = SecurityPermissionType::ALL
            .iter()
            .map(|c| coordinator.changed_at(*c))
            .collect();
        assert_eq!(stamps, after);
        assert_eq!(
            coordinator.seq(),
            seq,
            "a setting the policy does not contain is not a policy change",
        );
    }

    /// A security setting no capability reads — the timeout an unanswered
    /// prompt gets — is a policy change, and the sequence has to say so: copies
    /// need to see the new value. No stamp moves, because no capability's value
    /// did, and the approvals already given are still the right answers.
    #[tokio::test]
    async fn a_policy_change_no_capability_reads_still_advances_the_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));
        let seq = coordinator.seq();
        let stamps: Vec<u64> = SecurityPermissionType::ALL
            .iter()
            .map(|c| coordinator.changed_at(*c))
            .collect();

        coordinator
            .commit(|settings| {
                settings.security.approval_timeout = Some(45);
                Ok(())
            })
            .await
            .unwrap();

        assert!(coordinator.seq() > seq, "copies have to be told");
        let after: Vec<u64> = SecurityPermissionType::ALL
            .iter()
            .map(|c| coordinator.changed_at(*c))
            .collect();
        assert_eq!(
            stamps, after,
            "no capability changed value, so no approval is invalidated",
        );
    }

    /// A rejected change must not be half-applied: the caller reports an error
    /// and the host has to still be on the previous values.
    #[tokio::test]
    async fn an_abandoned_change_leaves_the_settings_alone() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));

        let result = coordinator
            .commit(|settings| {
                settings.security.allow_whiteboard = Some(false);
                DeskError::custom_error(DeskErrorCode::INVALID_PARAMS, "no")
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            coordinator.permission(SecurityPermissionType::Whiteboard),
            None
        );
        assert_eq!(
            coordinator.settings.read().await.security.allow_whiteboard,
            None
        );
    }

    /// An unsupported locale is a caller error, not something to persist.
    #[tokio::test]
    async fn an_unknown_locale_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));

        let result = coordinator
            .commit(|settings| {
                settings.system.locale = Some("fr-FR".to_string());
                Ok(())
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            coordinator.settings.read().await.system.locale.as_deref(),
            Some("zh-CN")
        );
    }

    /// A locale spelled the way a browser sends it is stored canonically and
    /// takes effect process-wide.
    #[tokio::test]
    async fn a_locale_commit_normalizes_and_applies() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));

        let outcome = coordinator
            .commit(|settings| {
                settings.system.locale = Some("en_US".to_string());
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(outcome.locale_changed_to.as_deref(), Some("en-US"));
        assert_eq!(crate::locale::current_locale(), "en-US");
        assert_eq!(
            coordinator.settings.read().await.system.locale.as_deref(),
            Some("en-US")
        );
        let _ = crate::locale::set_global_locale("zh-CN");
    }

    /// A remembered answer is exactly a commit of one capability, so it has to
    /// land the same way.
    #[tokio::test]
    async fn a_remembered_answer_becomes_the_standing_policy() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));
        let generation = coordinator.changed_at(SecurityPermissionType::Terminal);

        let outcome = coordinator
            .remember(SecurityPermissionType::Terminal, true, generation)
            .await;

        assert_eq!(outcome, RememberOutcome::Committed);
        assert_eq!(
            coordinator.permission(SecurityPermissionType::Terminal),
            Some(true)
        );
    }

    /// The operator's later decision wins over an answer that was given
    /// against the older policy.
    #[tokio::test]
    async fn a_remembered_answer_from_an_older_policy_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));
        let stale = coordinator.changed_at(SecurityPermissionType::Terminal);
        coordinator
            .commit(|settings| {
                settings.security.allow_terminal = Some(false);
                Ok(())
            })
            .await
            .unwrap();

        let outcome = coordinator
            .remember(SecurityPermissionType::Terminal, true, stale)
            .await;

        assert_eq!(outcome, RememberOutcome::Superseded);
        assert_eq!(
            coordinator.permission(SecurityPermissionType::Terminal),
            Some(false),
            "the operator's deny must survive the late approval"
        );
    }

    /// The revoke-and-restore case that comparing values cannot see: the
    /// capability reads the same as when the prompt went out, but it did move
    /// in between, so the answer is stale.
    #[tokio::test]
    async fn a_capability_that_returned_to_its_old_value_still_supersedes() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));
        let generation = coordinator.changed_at(SecurityPermissionType::FileBrowse);
        assert_eq!(
            coordinator.permission(SecurityPermissionType::FileBrowse),
            None
        );
        for value in [Some(false), None] {
            coordinator
                .commit(|settings| {
                    settings.security.allow_file_browse = value;
                    Ok(())
                })
                .await
                .unwrap();
        }
        assert_eq!(
            coordinator.permission(SecurityPermissionType::FileBrowse),
            None,
            "the value is back where it started"
        );

        let outcome = coordinator
            .remember(SecurityPermissionType::FileBrowse, true, generation)
            .await;

        assert_eq!(outcome, RememberOutcome::Superseded);
    }

    /// Changing one capability must not invalidate a remembered answer for a
    /// different one — that is the whole reason the stamps are per-capability.
    #[tokio::test]
    async fn an_unrelated_change_leaves_a_remembered_answer_valid() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));
        let generation = coordinator.changed_at(SecurityPermissionType::FileTransfer);
        coordinator
            .commit(|settings| {
                settings.security.allow_whiteboard = Some(false);
                Ok(())
            })
            .await
            .unwrap();

        let outcome = coordinator
            .remember(SecurityPermissionType::FileTransfer, true, generation)
            .await;

        assert_eq!(outcome, RememberOutcome::Committed);
    }

    /// Several fields changed together are one write, so a reader can never
    /// catch the file holding half of them.
    #[tokio::test]
    async fn a_multi_field_change_is_one_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let coordinator = coordinator_at(&path);

        coordinator
            .commit(|settings| {
                settings.system.locale = Some("en-US".to_string());
                settings.system.port = 9099;
                settings.security.allow_terminal = Some(false);
                Ok(())
            })
            .await
            .unwrap();

        let args = coordinator.settings.read().await.args.clone();
        let saved = Settings::load_readonly(&args).unwrap();
        assert_eq!(saved.system.locale.as_deref(), Some("en-US"));
        assert_eq!(saved.system.port, 9099);
        assert_eq!(saved.security.allow_terminal, Some(false));
        let _ = crate::locale::set_global_locale("zh-CN");
    }

    /// A worker with the policy in hand plus the channel the daemon publishes on.
    async fn coordinator_with_worker(
        path: &std::path::Path,
    ) -> (
        SettingsCoordinator,
        tokio::sync::mpsc::UnboundedReceiver<ServiceToWorker>,
    ) {
        let coordinator = coordinator_at(path);
        let (worker_manager, _worker_rx) = crate::daemon::worker_manager::WorkerManager::new(
            actix_web::web::Data::from(Arc::clone(&coordinator.settings)),
            crate::daemon::pc_manager::PcRegistry::new(),
        );
        let (ipc_tx, ipc_rx) = tokio::sync::mpsc::unbounded_channel();
        worker_manager.install_active_for_test(ipc_tx).await;
        coordinator.bind_worker_manager(worker_manager);
        (coordinator, ipc_rx)
    }

    /// The point of publishing: a worker enforcing the policy is told the moment
    /// it changes, rather than finding out when it is next restarted.
    #[tokio::test]
    async fn a_committed_policy_reaches_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, mut ipc_rx) = coordinator_with_worker(&dir.path().join("config")).await;

        coordinator
            .commit(|settings| {
                settings.security.allow_file_delete = Some(false);
                Ok(())
            })
            .await
            .unwrap();

        match ipc_rx.try_recv().expect("a published policy") {
            ServiceToWorker::UpdateSecurityPolicy(payload) => {
                assert_eq!(payload.snapshot.seq(), coordinator.seq());
                assert_eq!(payload.snapshot.security().allow_file_delete, Some(false));
                assert_eq!(
                    payload
                        .snapshot
                        .changed_at(SecurityPermissionType::FileDelete),
                    coordinator.seq(),
                    "the worker needs the stamp, not just the value"
                );
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// A change that leaves the policy alone must not publish: a worker that
    /// received it would treat every unrelated settings edit as a reason to
    /// re-examine its cached approvals.
    #[tokio::test]
    async fn a_change_outside_the_policy_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, mut ipc_rx) = coordinator_with_worker(&dir.path().join("config")).await;

        coordinator
            .commit(|settings| {
                settings.log.log_level = "debug".to_string();
                Ok(())
            })
            .await
            .unwrap();

        assert!(ipc_rx.try_recv().is_err());
    }

    /// A worker starts from the values in its Init payload but at sequence zero,
    /// which is not comparable with what the daemon has been counting. Restating
    /// the current policy is what puts them back on one numbering.
    #[tokio::test]
    async fn a_republish_restates_the_current_policy() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, mut ipc_rx) = coordinator_with_worker(&dir.path().join("config")).await;
        coordinator
            .commit(|settings| {
                settings.security.allow_terminal = Some(true);
                Ok(())
            })
            .await
            .unwrap();
        let _ = ipc_rx.try_recv();

        coordinator.republish().await;

        match ipc_rx.try_recv().expect("the current policy, restated") {
            ServiceToWorker::UpdateSecurityPolicy(payload) => {
                assert_eq!(payload.snapshot.seq(), coordinator.seq());
                assert_eq!(payload.snapshot.security().allow_terminal, Some(true));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// A locale change reaches the worker too, so its messages are rendered in
    /// the language the host is set to.
    #[tokio::test]
    async fn a_committed_locale_reaches_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, mut ipc_rx) = coordinator_with_worker(&dir.path().join("config")).await;

        coordinator
            .commit(|settings| {
                settings.system.locale = Some("en-US".to_string());
                Ok(())
            })
            .await
            .unwrap();

        match ipc_rx.try_recv().expect("a locale instruction") {
            ServiceToWorker::SetLocale(payload) => assert_eq!(payload.locale, "en-US"),
            other => panic!("unexpected message: {other:?}"),
        }
        let _ = crate::locale::set_global_locale("zh-CN");
    }

    /// With no worker to publish to, a commit still succeeds — the settings are
    /// durable, and a worker that starts later reads them from its Init
    /// payload.
    #[tokio::test]
    async fn a_commit_succeeds_with_nobody_to_publish_to() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = coordinator_at(&dir.path().join("config"));

        let outcome = coordinator
            .commit(|settings| {
                settings.security.allow_clipboard_sync = Some(true);
                Ok(())
            })
            .await
            .unwrap();

        assert!(outcome.policy_changed);
    }
}
