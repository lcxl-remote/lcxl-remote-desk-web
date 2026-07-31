//! Host-local manager credential proof lease and admission fencing.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use sha2::{Digest as _, Sha256};
use tokio::{
    sync::{RwLock, broadcast},
    time::{Instant, sleep_until},
};

const CREDENTIAL_LEASE: Duration = Duration::from_secs(120);

static NEXT_LINK_INCARNATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CredentialFingerprint(String);

impl CredentialFingerprint {
    pub fn from_token(token: &str) -> Self {
        let digest = Sha256::digest(token.as_bytes());
        Self(digest.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialScopeState {
    AwaitingProof,
    Active,
    Suspended,
    Revoked,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemberState {
    Pending,
    Active,
}

#[derive(Debug)]
struct CredentialScope {
    generation: u64,
    lease_generation: u64,
    active_incarnation: u64,
    deadline: Option<Instant>,
    state: CredentialScopeState,
    members: HashMap<String, MemberState>,
}

struct RegistryState {
    scopes: HashMap<CredentialFingerprint, CredentialScope>,
}

impl Default for RegistryState {
    fn default() -> Self {
        Self {
            scopes: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CredentialExpiry {
    pub members: Vec<String>,
    fingerprint: CredentialFingerprint,
    incarnation: u64,
}

impl CredentialExpiry {
    pub fn belongs_to(&self, link: &ManagerCredentialLink) -> bool {
        self.fingerprint == link.fingerprint && self.incarnation == link.incarnation
    }
}

#[derive(Clone)]
pub struct ManagerCredentialScopeRegistry {
    state: Arc<RwLock<RegistryState>>,
    expiry_tx: broadcast::Sender<CredentialExpiry>,
}

impl Default for ManagerCredentialScopeRegistry {
    fn default() -> Self {
        let (expiry_tx, _) = broadcast::channel(64);
        Self {
            state: Arc::new(RwLock::new(RegistryState::default())),
            expiry_tx,
        }
    }
}

#[derive(Clone)]
pub struct ManagerCredentialLink {
    registry: ManagerCredentialScopeRegistry,
    fingerprint: CredentialFingerprint,
    incarnation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionRejection {
    AwaitingProof,
    Terminal,
}

pub struct ManagerAdmissionPermit {
    link: ManagerCredentialLink,
    connection_id: String,
    generation: u64,
    committed: bool,
}

impl ManagerCredentialScopeRegistry {
    pub async fn begin_link(&self, token: &str) -> ManagerCredentialLink {
        let fingerprint = CredentialFingerprint::from_token(token);
        let incarnation = NEXT_LINK_INCARNATION.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.write().await;
        let scope = state
            .scopes
            .entry(fingerprint.clone())
            .or_insert_with(|| CredentialScope {
                generation: 0,
                lease_generation: 0,
                active_incarnation: incarnation,
                deadline: None,
                state: CredentialScopeState::AwaitingProof,
                members: HashMap::new(),
            });
        scope.generation = scope.generation.wrapping_add(1);
        scope.active_incarnation = incarnation;
        scope.state = CredentialScopeState::AwaitingProof;
        let inherited_timer = scope
            .deadline
            .map(|deadline| (scope.lease_generation, deadline));
        let link = ManagerCredentialLink {
            registry: self.clone(),
            fingerprint,
            incarnation,
        };
        drop(state);
        if let Some((lease_generation, deadline)) = inherited_timer {
            link.spawn_expiry(lease_generation, deadline);
        }
        link
    }

    async fn remove_pending(
        &self,
        link: &ManagerCredentialLink,
        connection_id: &str,
        generation: u64,
    ) {
        let mut state = self.state.write().await;
        if let Some(scope) = state.scopes.get_mut(&link.fingerprint)
            && scope.generation == generation
            && scope.active_incarnation == link.incarnation
            && scope.members.get(connection_id) == Some(&MemberState::Pending)
        {
            scope.members.remove(connection_id);
        }
    }

    pub async fn remove_member(&self, fingerprint: &CredentialFingerprint, connection_id: &str) {
        let mut state = self.state.write().await;
        if let Some(scope) = state.scopes.get_mut(fingerprint) {
            scope.members.remove(connection_id);
        }
        state.scopes.retain(|_, scope| {
            !scope.members.is_empty()
                || matches!(
                    scope.state,
                    CredentialScopeState::AwaitingProof
                        | CredentialScopeState::Active
                        | CredentialScopeState::Suspended
                )
        });
    }

    pub fn subscribe_expirations(&self) -> broadcast::Receiver<CredentialExpiry> {
        self.expiry_tx.subscribe()
    }
}

impl ManagerCredentialLink {
    pub fn fingerprint(&self) -> CredentialFingerprint {
        self.fingerprint.clone()
    }

    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    pub async fn begin_admission(
        &self,
        connection_id: &str,
    ) -> Result<ManagerAdmissionPermit, AdmissionRejection> {
        let mut state = self.registry.state.write().await;
        let Some(scope) = state.scopes.get_mut(&self.fingerprint) else {
            return Err(AdmissionRejection::Terminal);
        };
        if scope.active_incarnation != self.incarnation {
            return Err(AdmissionRejection::Terminal);
        }
        if scope
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            scope.state = CredentialScopeState::Expired;
            return Err(AdmissionRejection::Terminal);
        }
        match scope.state {
            CredentialScopeState::Active => {}
            CredentialScopeState::AwaitingProof => {
                return Err(AdmissionRejection::AwaitingProof);
            }
            CredentialScopeState::Suspended
            | CredentialScopeState::Revoked
            | CredentialScopeState::Expired => return Err(AdmissionRejection::Terminal),
        }
        scope
            .members
            .insert(connection_id.to_string(), MemberState::Pending);
        Ok(ManagerAdmissionPermit {
            link: self.clone(),
            connection_id: connection_id.to_string(),
            generation: scope.generation,
            committed: false,
        })
    }

    pub async fn accept_proof(&self) -> bool {
        let mut state = self.registry.state.write().await;
        let Some(scope) = state.scopes.get_mut(&self.fingerprint) else {
            return false;
        };
        if scope.active_incarnation != self.incarnation
            || matches!(scope.state, CredentialScopeState::Revoked)
        {
            return false;
        }
        if scope
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
            && !scope.members.is_empty()
        {
            scope.state = CredentialScopeState::Expired;
            return false;
        }
        let deadline = Instant::now() + CREDENTIAL_LEASE;
        scope.lease_generation = scope.lease_generation.wrapping_add(1);
        let lease_generation = scope.lease_generation;
        scope.deadline = Some(deadline);
        scope.state = CredentialScopeState::Active;
        drop(state);
        self.spawn_expiry(lease_generation, deadline);
        true
    }

    pub async fn proof_unavailable(&self) {
        let mut state = self.registry.state.write().await;
        if let Some(scope) = state.scopes.get_mut(&self.fingerprint)
            && scope.active_incarnation == self.incarnation
            && scope.state == CredentialScopeState::Active
        {
            scope.state = CredentialScopeState::AwaitingProof;
        }
    }

    pub async fn invalidate(&self, state_value: CredentialScopeState) -> Vec<String> {
        let mut state = self.registry.state.write().await;
        let Some(scope) = state.scopes.get_mut(&self.fingerprint) else {
            return Vec::new();
        };
        if scope.active_incarnation != self.incarnation {
            return Vec::new();
        }
        scope.generation = scope.generation.wrapping_add(1);
        scope.lease_generation = scope.lease_generation.wrapping_add(1);
        scope.state = state_value;
        scope.deadline = None;
        let members = scope.members.keys().cloned().collect();
        scope.members.clear();
        members
    }

    fn spawn_expiry(&self, lease_generation: u64, deadline: Instant) {
        let link = self.clone();
        tokio::spawn(async move {
            sleep_until(deadline).await;
            let mut state = link.registry.state.write().await;
            let Some(scope) = state.scopes.get_mut(&link.fingerprint) else {
                return;
            };
            if scope.active_incarnation != link.incarnation {
                return;
            }
            if scope.lease_generation != lease_generation || scope.deadline != Some(deadline) {
                return;
            }
            if Instant::now() < deadline {
                return;
            }
            scope.generation = scope.generation.wrapping_add(1);
            scope.lease_generation = scope.lease_generation.wrapping_add(1);
            scope.state = CredentialScopeState::Expired;
            scope.deadline = None;
            let members = scope.members.keys().cloned().collect();
            scope.members.clear();
            drop(state);
            let _ = link.registry.expiry_tx.send(CredentialExpiry {
                members,
                fingerprint: link.fingerprint.clone(),
                incarnation: link.incarnation,
            });
        });
    }

    pub async fn state(&self) -> Option<CredentialScopeState> {
        self.registry
            .state
            .read()
            .await
            .scopes
            .get(&self.fingerprint)
            .filter(|scope| scope.active_incarnation == self.incarnation)
            .map(|scope| scope.state)
    }

    pub async fn has_credential_deadline(&self) -> bool {
        self.registry
            .state
            .read()
            .await
            .scopes
            .get(&self.fingerprint)
            .is_some_and(|scope| {
                scope.active_incarnation == self.incarnation && scope.deadline.is_some()
            })
    }

    pub async fn members(&self) -> HashSet<String> {
        self.registry
            .state
            .read()
            .await
            .scopes
            .get(&self.fingerprint)
            .map(|scope| scope.members.keys().cloned().collect())
            .unwrap_or_default()
    }
}

impl ManagerAdmissionPermit {
    pub fn fingerprint(&self) -> CredentialFingerprint {
        self.link.fingerprint()
    }

    pub fn incarnation(&self) -> u64 {
        self.link.incarnation()
    }

    pub async fn commit(mut self) -> bool {
        let mut state = self.link.registry.state.write().await;
        let Some(scope) = state.scopes.get_mut(&self.link.fingerprint) else {
            return false;
        };
        if scope.generation != self.generation
            || scope.active_incarnation != self.link.incarnation
            || scope.state != CredentialScopeState::Active
            || scope
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            || scope.members.get(&self.connection_id) != Some(&MemberState::Pending)
        {
            return false;
        }
        scope
            .members
            .insert(self.connection_id.clone(), MemberState::Active);
        self.committed = true;
        true
    }
}

impl Drop for ManagerAdmissionPermit {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let registry = self.link.registry.clone();
        let link = self.link.clone();
        let connection_id = self.connection_id.clone();
        let generation = self.generation;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                registry
                    .remove_pending(&link, &connection_id, generation)
                    .await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn proof_is_required_before_admission() {
        let registry = ManagerCredentialScopeRegistry::default();
        let link = registry.begin_link("secret").await;
        assert!(matches!(
            link.begin_admission("c1").await,
            Err(AdmissionRejection::AwaitingProof)
        ));
        assert!(link.accept_proof().await);
        let permit = link.begin_admission("c1").await.unwrap();
        assert!(permit.commit().await);
        assert!(link.members().await.contains("c1"));
    }

    #[tokio::test]
    async fn old_incarnation_cannot_renew_or_admit() {
        let registry = ManagerCredentialScopeRegistry::default();
        let old = registry.begin_link("secret").await;
        assert!(old.accept_proof().await);
        let new = registry.begin_link("secret").await;
        assert!(!old.accept_proof().await);
        assert!(matches!(
            old.begin_admission("old").await,
            Err(AdmissionRejection::Terminal)
        ));
        assert!(new.accept_proof().await);
    }

    #[tokio::test]
    async fn invalidation_fences_pending_commit() {
        let registry = ManagerCredentialScopeRegistry::default();
        let link = registry.begin_link("secret").await;
        assert!(link.accept_proof().await);
        let permit = link.begin_admission("c1").await.unwrap();
        assert_eq!(
            link.invalidate(CredentialScopeState::Revoked).await,
            vec!["c1"]
        );
        assert!(!permit.commit().await);
    }

    #[tokio::test]
    async fn reconnect_inherits_the_existing_credential_deadline() {
        let registry = ManagerCredentialScopeRegistry::default();
        let old = registry.begin_link("secret").await;
        assert!(old.accept_proof().await);
        let permit = old.begin_admission("c1").await.unwrap();
        assert!(permit.commit().await);

        let current = registry.begin_link("secret").await;

        assert!(current.has_credential_deadline().await);
        assert_eq!(
            current.state().await,
            Some(CredentialScopeState::AwaitingProof)
        );
        assert!(current.members().await.contains("c1"));
    }

    #[tokio::test]
    async fn independent_deadline_task_expires_members_without_a_connection_loop() {
        let registry = ManagerCredentialScopeRegistry::default();
        let mut expiry_rx = registry.subscribe_expirations();
        let link = registry.begin_link("secret").await;
        assert!(link.accept_proof().await);
        let permit = link.begin_admission("c1").await.unwrap();
        assert!(permit.commit().await);
        let deadline = Instant::now() + Duration::from_millis(10);
        let lease_generation = {
            let mut state = registry.state.write().await;
            let scope = state.scopes.get_mut(&link.fingerprint).unwrap();
            scope.lease_generation = scope.lease_generation.wrapping_add(1);
            scope.deadline = Some(deadline);
            scope.lease_generation
        };
        link.spawn_expiry(lease_generation, deadline);

        let expiry = tokio::time::timeout(Duration::from_secs(1), expiry_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert!(expiry.belongs_to(&link));
        assert_eq!(expiry.members, vec!["c1"]);
        assert_eq!(link.state().await, Some(CredentialScopeState::Expired));
    }
}
