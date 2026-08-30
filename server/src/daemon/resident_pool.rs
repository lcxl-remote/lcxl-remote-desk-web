//! Pure resident-worker pool state and route fencing contract.

use desk_ipc_protocol::message::{
    DesktopTarget, SessionKey, WorkerIdentity, WorkerKey, WorkerProfile,
};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Starting,
    ReadyStandby,
    ActiveInteractive { route_epoch: u64 },
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteClass {
    Interactive,
    SessionResource,
    BroadcastPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTicket {
    pub worker: WorkerIdentity,
    pub route_class: RouteClass,
    /// Only interactive routes use a mutable desktop epoch. Session resources
    /// stay bound to their session-user worker while UAC changes the desktop.
    pub route_epoch: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolStateError {
    InvalidProfile,
    DuplicateWorker,
    WorkerUnknown,
    WorkerNotReady,
    WrongSession,
    RouteUnavailable,
}

impl fmt::Display for PoolStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

struct WorkerRecord {
    identity: WorkerIdentity,
    state: WorkerState,
}

#[derive(Default)]
pub struct ResidentWorkerPoolState {
    workers: HashMap<WorkerKey, WorkerRecord>,
    active_interactive: HashMap<SessionKey, WorkerKey>,
    route_epoch: HashMap<SessionKey, u64>,
}

impl ResidentWorkerPoolState {
    pub fn register(&mut self, identity: WorkerIdentity) -> Result<(), PoolStateError> {
        if !profile_matches_target(identity.profile, identity.key.desktop) {
            return Err(PoolStateError::InvalidProfile);
        }
        if self.workers.contains_key(&identity.key) {
            return Err(PoolStateError::DuplicateWorker);
        }
        self.workers.insert(
            identity.key.clone(),
            WorkerRecord {
                identity,
                state: WorkerState::Starting,
            },
        );
        Ok(())
    }

    pub fn mark_ready(&mut self, key: &WorkerKey, incarnation: u64) -> Result<(), PoolStateError> {
        let record = self
            .workers
            .get_mut(key)
            .ok_or(PoolStateError::WorkerUnknown)?;
        if record.identity.incarnation != incarnation {
            return Err(PoolStateError::WorkerUnknown);
        }
        record.state = WorkerState::ReadyStandby;
        Ok(())
    }

    pub fn replace(&mut self, identity: WorkerIdentity) -> Result<(), PoolStateError> {
        if !profile_matches_target(identity.profile, identity.key.desktop) {
            return Err(PoolStateError::InvalidProfile);
        }
        if self
            .active_interactive
            .get(&identity.key.session)
            .is_some_and(|active| active == &identity.key)
        {
            self.active_interactive.remove(&identity.key.session);
            self.bump_route_epoch(&identity.key.session);
        }
        self.workers.insert(
            identity.key.clone(),
            WorkerRecord {
                identity,
                state: WorkerState::Starting,
            },
        );
        Ok(())
    }

    pub fn retire(&mut self, key: &WorkerKey, incarnation: u64) -> bool {
        if !self
            .workers
            .get(key)
            .is_some_and(|record| record.identity.incarnation == incarnation)
        {
            return false;
        }
        self.workers.remove(key);
        if self
            .active_interactive
            .get(&key.session)
            .is_some_and(|active| active == key)
        {
            self.active_interactive.remove(&key.session);
            self.bump_route_epoch(&key.session);
        }
        true
    }

    pub fn retire_session(&mut self, session: &SessionKey) -> usize {
        let before = self.workers.len();
        self.workers.retain(|key, _| &key.session != session);
        self.active_interactive.remove(session);
        self.bump_route_epoch(session);
        before - self.workers.len()
    }

    pub fn activate_interactive(
        &mut self,
        session: &SessionKey,
        key: &WorkerKey,
    ) -> Result<RouteTicket, PoolStateError> {
        if &key.session != session {
            return Err(PoolStateError::WrongSession);
        }
        let record = self.workers.get(key).ok_or(PoolStateError::WorkerUnknown)?;
        if !matches!(
            record.state,
            WorkerState::ReadyStandby | WorkerState::ActiveInteractive { .. }
        ) {
            return Err(PoolStateError::WorkerNotReady);
        }

        if let Some(previous) = self.active_interactive.get(session)
            && previous != key
            && let Some(previous_record) = self.workers.get_mut(previous)
        {
            previous_record.state = WorkerState::ReadyStandby;
        }
        let epoch = self
            .route_epoch
            .entry(session.clone())
            .and_modify(|epoch| *epoch = epoch.saturating_add(1))
            .or_insert(1);
        let epoch = *epoch;
        let record = self.workers.get_mut(key).expect("worker was checked above");
        record.state = WorkerState::ActiveInteractive { route_epoch: epoch };
        self.active_interactive.insert(session.clone(), key.clone());
        Ok(RouteTicket {
            worker: record.identity.clone(),
            route_class: RouteClass::Interactive,
            route_epoch: Some(epoch),
        })
    }

    pub fn route(
        &self,
        session: &SessionKey,
        route_class: RouteClass,
    ) -> Result<RouteTicket, PoolStateError> {
        let key = match route_class {
            RouteClass::Interactive => self
                .active_interactive
                .get(session)
                .ok_or(PoolStateError::RouteUnavailable)?,
            RouteClass::SessionResource => self
                .workers
                .keys()
                .find(|key| {
                    &key.session == session
                        && matches!(
                            key.desktop,
                            DesktopTarget::WindowsDefault | DesktopTarget::LinuxSession
                        )
                })
                .ok_or(PoolStateError::RouteUnavailable)?,
            RouteClass::BroadcastPolicy => return Err(PoolStateError::WrongSession),
        };
        let record = self.workers.get(key).ok_or(PoolStateError::WorkerUnknown)?;
        let route_epoch = match (route_class, record.state) {
            (RouteClass::Interactive, WorkerState::ActiveInteractive { route_epoch }) => {
                Some(route_epoch)
            }
            (RouteClass::SessionResource, WorkerState::ReadyStandby)
            | (RouteClass::SessionResource, WorkerState::ActiveInteractive { .. }) => None,
            _ => return Err(PoolStateError::WorkerNotReady),
        };
        Ok(RouteTicket {
            worker: record.identity.clone(),
            route_class,
            route_epoch,
        })
    }

    pub fn broadcast_policy(&self) -> Vec<RouteTicket> {
        self.workers
            .values()
            .filter(|record| {
                matches!(
                    record.state,
                    WorkerState::ReadyStandby | WorkerState::ActiveInteractive { .. }
                )
            })
            .map(|record| RouteTicket {
                worker: record.identity.clone(),
                route_class: RouteClass::BroadcastPolicy,
                route_epoch: None,
            })
            .collect()
    }

    pub fn accepts(&self, ticket: &RouteTicket) -> bool {
        let Some(record) = self.workers.get(&ticket.worker.key) else {
            return false;
        };
        if record.identity.incarnation != ticket.worker.incarnation {
            return false;
        }
        match (ticket.route_class, record.state) {
            (RouteClass::Interactive, WorkerState::ActiveInteractive { route_epoch }) => {
                ticket.route_epoch == Some(route_epoch)
            }
            (
                RouteClass::SessionResource | RouteClass::BroadcastPolicy,
                WorkerState::ReadyStandby | WorkerState::ActiveInteractive { .. },
            ) => ticket.route_epoch.is_none(),
            _ => false,
        }
    }

    fn bump_route_epoch(&mut self, session: &SessionKey) -> u64 {
        let epoch = self.route_epoch.entry(session.clone()).or_default();
        *epoch = epoch.saturating_add(1);
        *epoch
    }
}

fn profile_matches_target(profile: WorkerProfile, target: DesktopTarget) -> bool {
    matches!(
        (profile, target),
        (WorkerProfile::SessionUser, DesktopTarget::WindowsDefault)
            | (WorkerProfile::SessionUser, DesktopTarget::LinuxSession)
            | (
                WorkerProfile::RestrictedDesktop,
                DesktopTarget::WindowsWinlogon
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> SessionKey {
        SessionKey {
            platform_session_id: name.to_string(),
            session_generation: 1,
        }
    }

    fn worker(
        session: SessionKey,
        desktop: DesktopTarget,
        profile: WorkerProfile,
        incarnation: u64,
    ) -> WorkerIdentity {
        WorkerIdentity {
            key: WorkerKey { session, desktop },
            profile,
            incarnation,
        }
    }

    fn ready(pool: &mut ResidentWorkerPoolState, identity: WorkerIdentity) {
        pool.register(identity.clone()).unwrap();
        pool.mark_ready(&identity.key, identity.incarnation)
            .unwrap();
    }

    #[test]
    fn multiple_sessions_can_each_have_an_active_desktop() {
        let mut pool = ResidentWorkerPoolState::default();
        let first = worker(
            session("wts-1"),
            DesktopTarget::WindowsDefault,
            WorkerProfile::SessionUser,
            1,
        );
        let second = worker(
            session("wts-2"),
            DesktopTarget::WindowsDefault,
            WorkerProfile::SessionUser,
            2,
        );
        ready(&mut pool, first.clone());
        ready(&mut pool, second.clone());

        let first_route = pool
            .activate_interactive(&first.key.session, &first.key)
            .unwrap();
        let second_route = pool
            .activate_interactive(&second.key.session, &second.key)
            .unwrap();
        assert!(pool.accepts(&first_route));
        assert!(pool.accepts(&second_route));
    }

    #[test]
    fn uac_switch_does_not_move_session_resources_to_restricted_worker() {
        let mut pool = ResidentWorkerPoolState::default();
        let user = worker(
            session("wts-1"),
            DesktopTarget::WindowsDefault,
            WorkerProfile::SessionUser,
            1,
        );
        let winlogon = worker(
            user.key.session.clone(),
            DesktopTarget::WindowsWinlogon,
            WorkerProfile::RestrictedDesktop,
            2,
        );
        ready(&mut pool, user.clone());
        ready(&mut pool, winlogon.clone());
        pool.activate_interactive(&user.key.session, &winlogon.key)
            .unwrap();

        let terminal = pool
            .route(&user.key.session, RouteClass::SessionResource)
            .unwrap();
        assert_eq!(terminal.worker.key, user.key);
        assert_eq!(terminal.worker.profile, WorkerProfile::SessionUser);
    }

    #[test]
    fn old_interactive_epoch_and_incarnation_are_fenced() {
        let mut pool = ResidentWorkerPoolState::default();
        let user = worker(
            session("wts-1"),
            DesktopTarget::WindowsDefault,
            WorkerProfile::SessionUser,
            1,
        );
        let winlogon = worker(
            user.key.session.clone(),
            DesktopTarget::WindowsWinlogon,
            WorkerProfile::RestrictedDesktop,
            2,
        );
        ready(&mut pool, user.clone());
        ready(&mut pool, winlogon.clone());
        let old_route = pool
            .activate_interactive(&user.key.session, &user.key)
            .unwrap();
        pool.activate_interactive(&user.key.session, &winlogon.key)
            .unwrap();
        assert!(!pool.accepts(&old_route));

        let mut stale_incarnation = pool
            .route(&user.key.session, RouteClass::SessionResource)
            .unwrap();
        stale_incarnation.worker.incarnation += 100;
        assert!(!pool.accepts(&stale_incarnation));
    }

    #[test]
    fn restricted_profile_is_valid_only_for_winlogon() {
        let mut pool = ResidentWorkerPoolState::default();
        let invalid = worker(
            session("wts-1"),
            DesktopTarget::WindowsDefault,
            WorkerProfile::RestrictedDesktop,
            1,
        );
        assert_eq!(pool.register(invalid), Err(PoolStateError::InvalidProfile));
    }

    #[test]
    fn replacement_and_logout_revoke_old_tickets() {
        let mut pool = ResidentWorkerPoolState::default();
        let original = worker(
            session("linux-a"),
            DesktopTarget::LinuxSession,
            WorkerProfile::SessionUser,
            1,
        );
        ready(&mut pool, original.clone());
        let old_ticket = pool
            .activate_interactive(&original.key.session, &original.key)
            .unwrap();

        let replacement = worker(
            original.key.session.clone(),
            DesktopTarget::LinuxSession,
            WorkerProfile::SessionUser,
            2,
        );
        pool.replace(replacement.clone()).unwrap();
        assert!(!pool.accepts(&old_ticket));
        pool.mark_ready(&replacement.key, replacement.incarnation)
            .unwrap();
        let replacement_ticket = pool
            .activate_interactive(&replacement.key.session, &replacement.key)
            .unwrap();
        assert!(pool.accepts(&replacement_ticket));

        assert_eq!(pool.retire_session(&replacement.key.session), 1);
        assert!(!pool.accepts(&replacement_ticket));
    }
}
