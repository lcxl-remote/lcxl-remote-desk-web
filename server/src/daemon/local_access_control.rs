use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::daemon::remote_access::RemoteAccessCoordinator;
use crate::host_control::HostRemoteAccessStatus;

const DEFAULT_CHALLENGE_TTL: Duration = Duration::from_secs(60);
const MAX_PENDING_CHALLENGES: usize = 128;
const MAX_CACHED_RESULTS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostAccessControlAction {
    DisconnectConnection { connection_id: String },
    LockAll,
    Unlock { expected_version: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAccessAuthChallengeRequest {
    pub request_id: String,
    pub action: HostAccessControlAction,
    pub expected_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAccessAuthChallenge {
    pub request_id: String,
    pub nonce: Vec<u8>,
    pub action_digest: [u8; 32],
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAuthProof {
    pub nonce: Vec<u8>,
    pub action_digest: [u8; 32],
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAccessControlRequest {
    pub request_id: String,
    pub action: HostAccessControlAction,
    pub auth_proof: Option<LocalAuthProof>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalAccessControlPhase {
    Authenticating,
    Applying,
    Disconnecting,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAccessControlProgress {
    pub request_id: String,
    pub phase: LocalAccessControlPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostAccessControlOutcome {
    Disconnected { connection_id: String },
    AlreadyDisconnected { connection_id: String },
    Locked { status: HostRemoteAccessStatus },
    Unlocked { status: HostRemoteAccessStatus },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAccessControlResult {
    pub request_id: String,
    pub outcome: HostAccessControlOutcome,
}

/// Identity produced by a native transport after OS-level peer inspection.
/// There is deliberately no deserialization path or public constructor: wire
/// clients cannot promote a self-reported pid/user into a verified identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedLocalPeer {
    pid: u32,
    user_id: String,
    executable_path: PathBuf,
    elevated: bool,
    protected_executable: bool,
}

impl VerifiedLocalPeer {
    pub(crate) fn from_native_transport(
        pid: u32,
        user_id: String,
        executable_path: PathBuf,
        elevated: bool,
        protected_executable: bool,
    ) -> Self {
        Self {
            pid,
            user_id,
            executable_path,
            elevated,
            protected_executable,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn executable_path(&self) -> &Path {
        &self.executable_path
    }

    pub fn is_elevated(&self) -> bool {
        self.elevated
    }

    pub fn has_protected_executable(&self) -> bool {
        self.protected_executable
    }
}

#[async_trait]
pub trait LocalUserAuthenticator: Send + Sync {
    async fn verify(
        &self,
        peer: &VerifiedLocalPeer,
        challenge: &LocalAccessAuthChallenge,
        proof: &LocalAuthProof,
    ) -> Result<()>;
}

/// Native elevated-peer authenticator used by the headless CLI and as the
/// cross-platform recovery path. The native transport establishes the elevated
/// token/root credential and protected executable path; the proof still binds
/// that fact to one nonce/action/version and is consumed by the service once.
pub struct ElevatedPeerAuthenticator;

#[async_trait]
impl LocalUserAuthenticator for ElevatedPeerAuthenticator {
    async fn verify(
        &self,
        peer: &VerifiedLocalPeer,
        challenge: &LocalAccessAuthChallenge,
        proof: &LocalAuthProof,
    ) -> Result<()> {
        if !peer.is_elevated() || !peer.has_protected_executable() {
            bail!("unlock requires an elevated client from the protected installation");
        }
        if proof.signature != elevated_proof_signature(challenge) {
            bail!("invalid elevated local authentication proof");
        }
        Ok(())
    }
}

pub fn elevated_proof_signature(challenge: &LocalAccessAuthChallenge) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(b"lcxl-local-access-elevated-proof-v1");
    digest.update(&challenge.nonce);
    digest.update(challenge.action_digest);
    digest.update(challenge.expires_at.timestamp_millis().to_le_bytes());
    digest.finalize().to_vec()
}

#[derive(Clone)]
struct PendingChallenge {
    public: LocalAccessAuthChallenge,
    action: HostAccessControlAction,
    expected_version: u64,
    deadline: Instant,
}

#[derive(Default)]
struct ChallengeCache {
    entries: HashMap<String, PendingChallenge>,
    order: VecDeque<String>,
}

#[derive(Clone)]
struct CachedResult {
    action_digest: [u8; 32],
    result: LocalAccessControlResult,
}

#[derive(Default)]
struct ResultCache {
    entries: HashMap<String, CachedResult>,
    order: VecDeque<String>,
}

pub struct LocalAccessControlService {
    coordinator: Arc<RemoteAccessCoordinator>,
    authenticator: Arc<dyn LocalUserAuthenticator>,
    challenge_ttl: Duration,
    challenges: Mutex<ChallengeCache>,
    results: Mutex<ResultCache>,
    execution: tokio::sync::Mutex<()>,
}

impl LocalAccessControlService {
    pub fn new(
        coordinator: Arc<RemoteAccessCoordinator>,
        authenticator: Arc<dyn LocalUserAuthenticator>,
    ) -> Self {
        Self::with_challenge_ttl(coordinator, authenticator, DEFAULT_CHALLENGE_TTL)
    }

    fn with_challenge_ttl(
        coordinator: Arc<RemoteAccessCoordinator>,
        authenticator: Arc<dyn LocalUserAuthenticator>,
        challenge_ttl: Duration,
    ) -> Self {
        Self {
            coordinator,
            authenticator,
            challenge_ttl,
            challenges: Mutex::new(ChallengeCache::default()),
            results: Mutex::new(ResultCache::default()),
            execution: tokio::sync::Mutex::new(()),
        }
    }

    pub fn issue_challenge(
        &self,
        peer: &VerifiedLocalPeer,
        request: LocalAccessAuthChallengeRequest,
    ) -> Result<LocalAccessAuthChallenge> {
        // The challenge itself is an authorization-bearing capability: only an
        // OS-elevated, installation-verified process may receive it. This keeps
        // an unprivileged same-user terminal from manufacturing the otherwise
        // deterministic binding proof or racing the elevated helper.
        if !peer.is_elevated() || !peer.has_protected_executable() {
            bail!("unlock challenge requires local OS elevation");
        }
        let HostAccessControlAction::Unlock { expected_version } = request.action.clone() else {
            bail!("authentication challenges are only issued for unlock");
        };
        if expected_version != request.expected_version {
            bail!("challenge expected_version does not match unlock action");
        }
        let current = self.coordinator.snapshot();
        if current.state_version != expected_version {
            bail!(
                "stale remote-access state: expected version {expected_version}, current version {}",
                current.state_version
            );
        }
        if !current.is_locked() {
            bail!("remote access is already unlocked");
        }

        let action_digest = digest_action(&request.action, request.expected_version)?;
        let nonce = uuid::Uuid::new_v4().as_bytes().to_vec();
        let expires_at = Utc::now()
            + chrono::Duration::from_std(self.challenge_ttl)
                .unwrap_or_else(|_| chrono::Duration::seconds(60));
        let public = LocalAccessAuthChallenge {
            request_id: request.request_id.clone(),
            nonce,
            action_digest,
            expires_at,
        };
        let pending = PendingChallenge {
            public: public.clone(),
            action: request.action,
            expected_version: request.expected_version,
            deadline: Instant::now() + self.challenge_ttl,
        };

        let mut cache = self.challenges.lock().unwrap();
        prune_expired_challenges(&mut cache);
        if cache.entries.contains_key(&request.request_id) {
            bail!("a live challenge already exists for request_id");
        }
        insert_bounded_challenge(&mut cache, request.request_id, pending);
        Ok(public)
    }

    pub fn status(&self) -> HostRemoteAccessStatus {
        HostRemoteAccessStatus::from(&self.coordinator.snapshot())
    }

    pub async fn execute(
        &self,
        peer: &VerifiedLocalPeer,
        request: LocalAccessControlRequest,
    ) -> Result<LocalAccessControlResult> {
        if !peer.has_protected_executable() {
            bail!("local access control requires a client from the protected installation");
        }
        let _execution = self.execution.lock().await;
        let expected_version = match request.action {
            HostAccessControlAction::Unlock { expected_version } => expected_version,
            _ => 0,
        };
        let action_digest = digest_action(&request.action, expected_version)?;
        if let Some(cached) = self
            .results
            .lock()
            .unwrap()
            .entries
            .get(&request.request_id)
        {
            if cached.action_digest != action_digest {
                bail!("request_id was already used for a different action");
            }
            return Ok(cached.result.clone());
        }

        let outcome = match &request.action {
            HostAccessControlAction::DisconnectConnection { connection_id } => {
                let outcome = self
                    .coordinator
                    .disconnect_connection(connection_id)
                    .await?;
                if outcome.already_disconnected {
                    HostAccessControlOutcome::AlreadyDisconnected {
                        connection_id: outcome.connection_id,
                    }
                } else {
                    HostAccessControlOutcome::Disconnected {
                        connection_id: outcome.connection_id,
                    }
                }
            }
            HostAccessControlAction::LockAll => {
                let state = self.coordinator.lock().await?;
                HostAccessControlOutcome::Locked {
                    status: HostRemoteAccessStatus::from(&state),
                }
            }
            HostAccessControlAction::Unlock { expected_version } => {
                let proof = request
                    .auth_proof
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("unlock requires an authentication proof"))?;
                let challenge = {
                    let mut cache = self.challenges.lock().unwrap();
                    prune_expired_challenges(&mut cache);
                    cache
                        .order
                        .retain(|request_id| request_id != &request.request_id);
                    cache.entries.remove(&request.request_id)
                }
                .ok_or_else(|| {
                    anyhow::anyhow!("unlock challenge is missing, expired, or consumed")
                })?;
                if challenge.deadline <= Instant::now() {
                    bail!("unlock challenge expired");
                }
                if challenge.action != request.action
                    || challenge.expected_version != *expected_version
                {
                    bail!("unlock action does not match challenge");
                }
                let current = self.coordinator.snapshot();
                if current.state_version != *expected_version {
                    bail!(
                        "stale remote-access state: expected version {expected_version}, current version {}",
                        current.state_version
                    );
                }
                if proof.nonce != challenge.public.nonce
                    || proof.action_digest != challenge.public.action_digest
                {
                    bail!("authentication proof is not bound to this challenge");
                }
                self.authenticator
                    .verify(peer, &challenge.public, proof)
                    .await?;
                let state = self.coordinator.unlock(*expected_version).await?;
                HostAccessControlOutcome::Unlocked {
                    status: HostRemoteAccessStatus::from(&state),
                }
            }
        };

        let result = LocalAccessControlResult {
            request_id: request.request_id.clone(),
            outcome,
        };
        let mut cache = self.results.lock().unwrap();
        insert_bounded_result(
            &mut cache,
            request.request_id,
            CachedResult {
                action_digest,
                result: result.clone(),
            },
        );
        Ok(result)
    }
}

fn digest_action(action: &HostAccessControlAction, expected_version: u64) -> Result<[u8; 32]> {
    let encoded = serde_json::to_vec(&(action, expected_version))?;
    Ok(Sha256::digest(encoded).into())
}

fn prune_expired_challenges(cache: &mut ChallengeCache) {
    let now = Instant::now();
    cache
        .entries
        .retain(|_, challenge| challenge.deadline > now);
    cache
        .order
        .retain(|request_id| cache.entries.contains_key(request_id));
}

fn insert_bounded_challenge(
    cache: &mut ChallengeCache,
    request_id: String,
    challenge: PendingChallenge,
) {
    while cache.entries.len() >= MAX_PENDING_CHALLENGES {
        if let Some(oldest) = cache.order.pop_front() {
            cache.entries.remove(&oldest);
        } else {
            break;
        }
    }
    cache.order.push_back(request_id.clone());
    cache.entries.insert(request_id, challenge);
}

fn insert_bounded_result(cache: &mut ResultCache, request_id: String, result: CachedResult) {
    while cache.entries.len() >= MAX_CACHED_RESULTS {
        if let Some(oldest) = cache.order.pop_front() {
            cache.entries.remove(&oldest);
        } else {
            break;
        }
    }
    cache.order.push_back(request_id.clone());
    cache.entries.insert(request_id, result);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::remote_access::{
        RemoteAccessGate, RemoteAccessState, RemoteAccessStateStore,
    };
    use crate::host_control::HostControlHub;

    struct MockAuthenticator;

    #[async_trait]
    impl LocalUserAuthenticator for MockAuthenticator {
        async fn verify(
            &self,
            _peer: &VerifiedLocalPeer,
            _challenge: &LocalAccessAuthChallenge,
            proof: &LocalAuthProof,
        ) -> Result<()> {
            if proof.signature == b"valid" {
                Ok(())
            } else {
                bail!("invalid local authentication signature")
            }
        }
    }

    fn peer() -> VerifiedLocalPeer {
        VerifiedLocalPeer::from_native_transport(
            7,
            "test-user".into(),
            "test-client".into(),
            true,
            true,
        )
    }

    fn untrusted_peer() -> VerifiedLocalPeer {
        VerifiedLocalPeer::from_native_transport(
            8,
            "test-user".into(),
            "untrusted-client".into(),
            false,
            false,
        )
    }

    fn service(
        ttl: Duration,
    ) -> (
        tempfile::TempDir,
        Arc<RemoteAccessCoordinator>,
        LocalAccessControlService,
    ) {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = RemoteAccessStateStore::new(temp.path().join("remote-access-state.toml"));
        let initial = RemoteAccessState::unlocked(1);
        store.persist(&initial).expect("initial state");
        let gate = RemoteAccessGate::new(initial);
        let hub = HostControlHub::new_local();
        let coordinator = Arc::new(RemoteAccessCoordinator::new(
            store,
            gate,
            hub.host_activity(),
        ));
        let service = LocalAccessControlService::with_challenge_ttl(
            coordinator.clone(),
            Arc::new(MockAuthenticator),
            ttl,
        );
        (temp, coordinator, service)
    }

    async fn lock_and_challenge(
        service: &LocalAccessControlService,
        coordinator: &RemoteAccessCoordinator,
        request_id: &str,
    ) -> (u64, LocalAccessAuthChallenge) {
        let locked = coordinator.lock().await.expect("lock");
        let expected_version = locked.state_version;
        let challenge = service
            .issue_challenge(
                &peer(),
                LocalAccessAuthChallengeRequest {
                    request_id: request_id.to_string(),
                    action: HostAccessControlAction::Unlock { expected_version },
                    expected_version,
                },
            )
            .expect("challenge");
        (expected_version, challenge)
    }

    fn proof(challenge: &LocalAccessAuthChallenge, signature: &[u8]) -> LocalAuthProof {
        LocalAuthProof {
            nonce: challenge.nonce.clone(),
            action_digest: challenge.action_digest,
            signature: signature.to_vec(),
        }
    }

    #[tokio::test]
    async fn unlock_requires_bound_valid_single_use_proof() {
        let (_temp, coordinator, service) = service(Duration::from_secs(10));
        let (expected_version, challenge) =
            lock_and_challenge(&service, &coordinator, "unlock-1").await;
        let request = LocalAccessControlRequest {
            request_id: "unlock-1".into(),
            action: HostAccessControlAction::Unlock { expected_version },
            auth_proof: Some(proof(&challenge, b"valid")),
        };

        let result = service
            .execute(&peer(), request.clone())
            .await
            .expect("unlock");
        assert!(matches!(
            result.outcome,
            HostAccessControlOutcome::Unlocked { .. }
        ));
        assert_eq!(
            service.execute(&peer(), request).await.expect("cached"),
            result
        );
        assert!(!coordinator.snapshot().is_locked());
    }

    #[tokio::test]
    async fn lock_needs_verified_peer_type_but_not_unlock_proof_and_is_idempotent() {
        let (_temp, coordinator, service) = service(Duration::from_secs(10));
        let request = LocalAccessControlRequest {
            request_id: "lock-1".into(),
            action: HostAccessControlAction::LockAll,
            auth_proof: None,
        };

        let first = service
            .execute(&peer(), request.clone())
            .await
            .expect("lock");
        let second = service
            .execute(&peer(), request)
            .await
            .expect("cached lock");
        assert_eq!(first, second);
        assert!(coordinator.snapshot().is_locked());

        let reused = LocalAccessControlRequest {
            request_id: "lock-1".into(),
            action: HostAccessControlAction::Unlock {
                expected_version: coordinator.snapshot().state_version,
            },
            auth_proof: None,
        };
        assert!(service.execute(&peer(), reused).await.is_err());
    }

    #[tokio::test]
    async fn untrusted_native_peer_cannot_lock_or_obtain_unlock_challenge() {
        let (_temp, coordinator, service) = service(Duration::from_secs(10));
        let request = LocalAccessControlRequest {
            request_id: "untrusted-lock".into(),
            action: HostAccessControlAction::LockAll,
            auth_proof: None,
        };
        assert!(service.execute(&untrusted_peer(), request).await.is_err());

        let locked = coordinator.lock().await.unwrap();
        assert!(
            service
                .issue_challenge(
                    &untrusted_peer(),
                    LocalAccessAuthChallengeRequest {
                        request_id: "untrusted-unlock".into(),
                        action: HostAccessControlAction::Unlock {
                            expected_version: locked.state_version,
                        },
                        expected_version: locked.state_version,
                    },
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn invalid_signature_consumes_challenge() {
        let (_temp, coordinator, service) = service(Duration::from_secs(10));
        let (expected_version, challenge) =
            lock_and_challenge(&service, &coordinator, "unlock-2").await;
        let request = LocalAccessControlRequest {
            request_id: "unlock-2".into(),
            action: HostAccessControlAction::Unlock { expected_version },
            auth_proof: Some(proof(&challenge, b"invalid")),
        };
        assert!(service.execute(&peer(), request.clone()).await.is_err());
        assert!(service.execute(&peer(), request).await.is_err());
        assert!(coordinator.snapshot().is_locked());
    }

    #[tokio::test]
    async fn proof_for_another_challenge_is_rejected() {
        let (_temp, coordinator, service) = service(Duration::from_secs(10));
        let (expected_version, first) =
            lock_and_challenge(&service, &coordinator, "unlock-a").await;
        let second = service
            .issue_challenge(
                &peer(),
                LocalAccessAuthChallengeRequest {
                    request_id: "unlock-b".into(),
                    action: HostAccessControlAction::Unlock { expected_version },
                    expected_version,
                },
            )
            .expect("second challenge");
        let request = LocalAccessControlRequest {
            request_id: "unlock-b".into(),
            action: HostAccessControlAction::Unlock { expected_version },
            auth_proof: Some(proof(&first, b"valid")),
        };
        assert!(service.execute(&peer(), request).await.is_err());
        assert_ne!(first.nonce, second.nonce);
        assert!(coordinator.snapshot().is_locked());
    }

    #[tokio::test]
    async fn expired_challenge_cannot_unlock() {
        let (_temp, coordinator, service) = service(Duration::ZERO);
        let locked = coordinator.lock().await.expect("lock");
        let expected_version = locked.state_version;
        let challenge = service
            .issue_challenge(
                &peer(),
                LocalAccessAuthChallengeRequest {
                    request_id: "expired".into(),
                    action: HostAccessControlAction::Unlock { expected_version },
                    expected_version,
                },
            )
            .expect("challenge");
        let request = LocalAccessControlRequest {
            request_id: "expired".into(),
            action: HostAccessControlAction::Unlock { expected_version },
            auth_proof: Some(proof(&challenge, b"valid")),
        };
        assert!(service.execute(&peer(), request).await.is_err());
        assert!(coordinator.snapshot().is_locked());
    }

    #[tokio::test]
    async fn stale_challenge_cannot_unlock_newer_state() {
        let (_temp, coordinator, service) = service(Duration::from_secs(10));
        let (expected_version, challenge) =
            lock_and_challenge(&service, &coordinator, "stale").await;
        coordinator
            .unlock(expected_version)
            .await
            .expect("intervening unlock");
        let request = LocalAccessControlRequest {
            request_id: "stale".into(),
            action: HostAccessControlAction::Unlock { expected_version },
            auth_proof: Some(proof(&challenge, b"valid")),
        };
        assert!(service.execute(&peer(), request).await.is_err());
    }
}
