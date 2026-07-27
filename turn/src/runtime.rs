//! Live view of this process's TURN runtime, for everything that must follow it
//! instead of freezing whatever existed at startup.
//!
//! The supervisor rebuilds the runtime whenever the configuration changes, so a
//! consumer holding a `TurnApiState` from process start would, after the first
//! reconfiguration, be reading a server that no longer exists — issuing ICE
//! credentials the running relay rejects, or reporting the uptime of a socket
//! that is closed. [`TurnRuntimeView`] resolves the current runtime per call.
//!
//! It also carries the *intent*, which the supervisor deliberately does not
//! model: to the supervisor, "no runtime" is one state, but to an operator
//! "switched off", "nothing configured", "this mode never relays" and "tried and
//! failed" are four different answers.

use std::sync::Arc;

use async_trait::async_trait;
use desk_signal_facade::model::signal::{LcxlRTCIceServer, TurnProvider};
use tokio::sync::watch;

use crate::model::{SOFTWARE, TurnApiState, TurnRuntimeInfo, TurnRuntimeState};
use crate::supervisor::TurnSupervisorHandle;

/// What the host means to do about TURN, independent of whether it succeeded.
///
/// Derived from the host's own settings and startup mode, so it is the piece
/// the supervisor cannot know: the supervisor is told *what* to run, never
/// *why* it was told nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnIntent {
    /// A runtime should be up.
    Run,
    /// The operator switched TURN off.
    Disabled,
    /// This startup mode never hosts a TURN runtime.
    Unsupported,
    /// Switched on, but no interface is configured to serve on.
    NotConfigured,
}

/// Read side of the TURN runtime: the live server plus why it is (not) there.
///
/// Cheap to clone; every reader gets its own handle onto the same channels.
#[derive(Clone)]
pub struct TurnRuntimeView {
    /// `None` on a host that never spawned a supervisor, which is exactly the
    /// hosts whose intent is [`TurnIntent::Unsupported`].
    supervisor: Option<TurnSupervisorHandle>,
    intent: watch::Receiver<TurnIntent>,
}

impl TurnRuntimeView {
    pub fn new(supervisor: TurnSupervisorHandle, intent: watch::Receiver<TurnIntent>) -> Self {
        Self {
            supervisor: Some(supervisor),
            intent,
        }
    }

    /// A view for a process that hosts no TURN runtime at all.
    ///
    /// The runtime endpoints are registered everywhere (their availability is a
    /// runtime property, not a route-table one), so a process with no supervisor
    /// still has to answer them — with "this mode does not relay" rather than an
    /// error about a missing extractor.
    pub fn unsupported() -> Self {
        let (tx, intent) = watch::channel(TurnIntent::Unsupported);
        // The sender is dropped on purpose: the value never changes, and a
        // receiver keeps reading the last value after its sender is gone.
        drop(tx);
        Self {
            supervisor: None,
            intent,
        }
    }

    /// The runtime serving right now, if any.
    pub fn runtime(&self) -> Option<Arc<TurnApiState>> {
        self.supervisor.as_ref()?.runtime()
    }

    /// Follow runtime changes (restarts included) rather than sampling once.
    pub fn subscribe(&self) -> Option<watch::Receiver<Option<Arc<TurnApiState>>>> {
        self.supervisor.as_ref().map(|s| s.subscribe_runtime())
    }

    /// Current runtime status, answering "is this host relaying, and if not,
    /// why" in one document.
    pub async fn info(&self) -> TurnRuntimeInfo {
        if let Some(state) = self.runtime() {
            return TurnRuntimeInfo {
                state: TurnRuntimeState::Running,
                software: SOFTWARE.to_string(),
                interfaces: state.settings.interfaces.clone(),
                uptime_secs: Some(state.uptime.elapsed().as_secs()),
                last_error: None,
            };
        }
        // Copy out before any await: a `watch::Ref` is not Send.
        let intent = *self.intent.borrow();
        let (state, last_error) = match intent {
            TurnIntent::Disabled => (TurnRuntimeState::Disabled, None),
            TurnIntent::Unsupported => (TurnRuntimeState::Unsupported, None),
            TurnIntent::NotConfigured => (TurnRuntimeState::NotConfigured, None),
            // Meant to run and is not: the supervisor is mid-retry, and its last
            // error is the only actionable thing to report.
            TurnIntent::Run => {
                let last_error = match &self.supervisor {
                    Some(s) => s.status().await.last_error,
                    None => None,
                };
                (TurnRuntimeState::Failed, last_error)
            }
        };
        TurnRuntimeInfo {
            state,
            software: SOFTWARE.to_string(),
            interfaces: Vec::new(),
            uptime_secs: None,
            last_error,
        }
    }
}

/// A [`TurnProvider`] that issues credentials for the runtime that is serving
/// *now*.
///
/// Reading the settings instead would be wrong in both directions: after the
/// service is switched off it would keep advertising a relay that no longer
/// listens, and during a secret rotation it would sign credentials with a secret
/// the running server has not adopted yet. The running runtime is the only
/// self-consistent source.
pub struct LiveTurnProvider {
    view: TurnRuntimeView,
}

impl LiveTurnProvider {
    pub fn new(view: TurnRuntimeView) -> Self {
        Self { view }
    }
}

#[async_trait]
impl TurnProvider for LiveTurnProvider {
    async fn get_ice_servers(&self, username: &str, credential: &str) -> LcxlRTCIceServer {
        match self.view.runtime() {
            Some(state) => state.settings.get_ice_servers(username, credential),
            // An entry with no URLs; callers already skip it rather than hand a
            // peer a relay it cannot reach.
            None => LcxlRTCIceServer::default(),
        }
    }

    async fn get_rest_ice_servers(&self, name: &str, ttl_secs: u64) -> Option<LcxlRTCIceServer> {
        self.view
            .runtime()?
            .settings
            .get_rest_ice_servers(name, ttl_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::{
        BackoffConfig, DesiredState, StartedRuntime, TurnRuntimeDriver, spawn,
    };
    use crate::test_support::{EXTERNAL, loopback_params, loopback_supervisor, wait_for_runtime};
    use crate::{model::TurnRuntimeState, supervisor::TurnRuntimeParams};
    use std::time::Duration;

    /// The three "not running for a reason" answers must stay distinguishable:
    /// an operator who switched TURN off, one who never gave it an address, and
    /// a mode that cannot relay take different actions, and a single
    /// "unavailable" would hide which one happened.
    #[tokio::test]
    async fn a_host_that_is_not_relaying_says_why() {
        for (intent, expected) in [
            (TurnIntent::Disabled, TurnRuntimeState::Disabled),
            (TurnIntent::Unsupported, TurnRuntimeState::Unsupported),
            (TurnIntent::NotConfigured, TurnRuntimeState::NotConfigured),
        ] {
            let (supervisor, view, _intent_tx) = loopback_supervisor(
                intent,
                DesiredState {
                    revision: 1,
                    params: None,
                },
            );
            let info = view.info().await;
            assert_eq!(info.state, expected);
            assert!(info.interfaces.is_empty(), "nothing is being served");
            assert!(info.uptime_secs.is_none());
            assert!(info.last_error.is_none(), "{intent:?} is not an error");
            supervisor.shutdown().await;
        }
    }

    /// A host with no supervisor at all still answers, rather than failing on a
    /// missing extractor.
    #[tokio::test]
    async fn a_process_without_a_supervisor_reports_unsupported() {
        let info = TurnRuntimeView::unsupported().info().await;
        assert_eq!(info.state, TurnRuntimeState::Unsupported);
        assert!(!info.software.is_empty());
    }

    /// Meant to run but not running is the one case that carries a cause.
    #[tokio::test]
    async fn a_failed_start_surfaces_the_supervisor_error() {
        struct AlwaysFails;
        #[async_trait]
        impl TurnRuntimeDriver for AlwaysFails {
            async fn start(&self, _params: &TurnRuntimeParams) -> Result<StartedRuntime, String> {
                Err("no socket for you".into())
            }
        }
        let (_tx, rx) = watch::channel(TurnIntent::Run);
        let supervisor = spawn(
            Arc::new(AlwaysFails),
            DesiredState {
                revision: 1,
                params: Some(loopback_params("s")),
            },
            BackoffConfig {
                min: Duration::from_millis(5),
                max: Duration::from_millis(20),
            },
        );
        let view = TurnRuntimeView::new(supervisor, rx);
        for _ in 0..200 {
            let info = view.info().await;
            if info.last_error.is_some() {
                assert_eq!(info.state, TurnRuntimeState::Failed);
                assert!(info.last_error.unwrap().contains("no socket for you"));
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("a failing start never surfaced an error");
    }

    /// A running runtime reports what it is actually serving.
    #[tokio::test]
    async fn a_running_runtime_reports_its_own_interfaces() {
        let (supervisor, view, _intent_tx) = loopback_supervisor(
            TurnIntent::Run,
            DesiredState {
                revision: 1,
                params: Some(loopback_params("s")),
            },
        );
        wait_for_runtime(&view, true).await;

        let info = view.info().await;
        assert_eq!(info.state, TurnRuntimeState::Running);
        assert_eq!(
            info.interfaces
                .iter()
                .map(|i| i.external.as_str())
                .collect::<Vec<_>>(),
            vec![EXTERNAL]
        );
        assert!(info.uptime_secs.is_some());
        assert!(info.last_error.is_none());
        supervisor.shutdown().await;
    }

    /// The provider must track restarts: after a secret rotation it has to sign
    /// with the secret the *running* server validates against, and after a
    /// shutdown it must stop advertising the relay entirely.
    #[tokio::test]
    async fn the_provider_follows_the_running_runtime() {
        let (supervisor, view, _intent_tx) = loopback_supervisor(
            TurnIntent::Run,
            DesiredState {
                revision: 1,
                params: Some(loopback_params("first-secret")),
            },
        );
        wait_for_runtime(&view, true).await;
        let provider = LiveTurnProvider::new(view.clone());

        let before = provider
            .get_rest_ice_servers("peer-1", 600)
            .await
            .expect("a running runtime with a secret issues credentials");
        assert_eq!(before.urls, vec![format!("turn:{EXTERNAL}?transport=udp")]);

        // Rotate the secret: the runtime restarts, and the credential must change
        // with it (a credential signed with the retired secret is rejected).
        supervisor.apply(DesiredState {
            revision: 2,
            params: Some(loopback_params("second-secret")),
        });
        let mut after = None;
        for _ in 0..200 {
            if let Some(candidate) = provider.get_rest_ice_servers("peer-1", 600).await
                && candidate.credential != before.credential
            {
                after = Some(candidate);
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let after = after.expect("the rotated secret never reached the provider");
        assert_eq!(after.urls, before.urls, "the endpoint itself is unchanged");

        // Switched off: nothing to advertise, and the ICE entry carries no URLs
        // so callers skip it.
        supervisor.apply(DesiredState {
            revision: 3,
            params: None,
        });
        wait_for_runtime(&view, false).await;
        assert!(provider.get_rest_ice_servers("peer-1", 600).await.is_none());
        assert!(
            provider
                .get_ice_servers("peer-1", "client-1")
                .await
                .urls
                .is_empty(),
            "a host with no relay must advertise no relay"
        );
        supervisor.shutdown().await;
    }
}
