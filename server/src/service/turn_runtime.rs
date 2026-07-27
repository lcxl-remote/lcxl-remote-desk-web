//! This host's TURN runtime: the driver that starts one, and the control handle
//! that keeps it converged on the saved settings.
//!
//! The driver is deliberately not shared with the managed deployment: both start
//! the same server, but a host authenticates its own signaling connections
//! (`SharedConnectionMap`) while a managed node also enforces decisions its
//! control plane made. Only the supervisor's state machine is common.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use actix_web::web;
use async_trait::async_trait;
use desk_signal::model::SharedConnectionMap;
use desk_turn::model::{Statistics, TurnApiState, TurnSettings};
use desk_turn::runtime::TurnPosture;
use desk_turn::service::startup_turn_server;
use desk_turn::supervisor::{
    StartedRuntime, TurnRuntimeDriver, TurnRuntimeHandle, TurnRuntimeParams, TurnSupervisorHandle,
};
use tokio::sync::watch;

use crate::model::settings::StartupMode;
use crate::model::turn::TurnAuthHandler;
use crate::service::turn_lifecycle::turn_plan;

/// Starts TURN runtimes for this host.
pub struct HostTurnDriver {
    /// Live signaling connections, consulted by the auth handler to turn a TURN
    /// username into a known peer. Shared with the rest of the process, not
    /// per-runtime: connections outlive any single runtime.
    connection_map: web::Data<SharedConnectionMap>,
}

impl HostTurnDriver {
    pub fn new(connection_map: web::Data<SharedConnectionMap>) -> Self {
        Self { connection_map }
    }
}

/// Handle wrapping a running [`TurnApiState`]; `close` shuts the server down.
struct HostHandle {
    state: Arc<TurnApiState>,
}

#[async_trait]
impl TurnRuntimeHandle for HostHandle {
    async fn close(&self) -> Result<(), String> {
        // Propagate failure so the supervisor retains this handle and retries the
        // close instead of leaking the UDP socket.
        self.state.server.close().await.map_err(|e| e.to_string())
    }
}

#[async_trait]
impl TurnRuntimeDriver for HostTurnDriver {
    async fn start(&self, params: &TurnRuntimeParams) -> Result<StartedRuntime, String> {
        let settings = TurnSettings {
            realm: params.realm.clone(),
            interfaces: params.interfaces.clone(),
            static_auth_secret: params.secret.clone(),
            relay_min_port: params.relay_min_port,
            relay_max_port: params.relay_max_port,
            // The supervisor was only asked to start this runtime because the
            // switch was on; the runtime's own copy says so too, so anything
            // reading the running settings (ICE issuance) agrees.
            enable_turn: true,
            ..TurnSettings::default()
        };
        // Fresh accounting per runtime: a restart resets the counters, and the
        // usage collector re-baselines when it sees the new instance rather than
        // reading a decreasing total as a huge negative delta.
        let statistics = Arc::new(RwLock::new(Statistics::default()));
        let auth = Arc::new(TurnAuthHandler::new(
            settings.clone(),
            self.connection_map.clone(),
            statistics.clone(),
        ));
        let api_state = startup_turn_server(settings, auth, statistics)
            .await
            .map_err(|e| e.to_string())?;
        Ok(StartedRuntime {
            handle: Arc::new(HostHandle {
                state: api_state.clone(),
            }),
            api_state,
        })
    }
}

/// Write side of the TURN runtime: turns saved settings into a desired state.
///
/// Every endpoint that persists TURN settings goes through [`Self::apply`]. The
/// alternative — each endpoint saving and hoping something else notices — is how
/// the secret rotation endpoint used to leave the disk and the running server
/// permanently disagreeing.
pub struct TurnRuntimeControl {
    mode: StartupMode,
    supervisor: TurnSupervisorHandle,
    posture_tx: watch::Sender<TurnPosture>,
    /// Monotonic tag for each published desired state. It orders the supervisor's
    /// view of "which save is this"; it is not a settings version.
    revision: AtomicU64,
}

impl TurnRuntimeControl {
    pub fn new(
        mode: StartupMode,
        supervisor: TurnSupervisorHandle,
        posture_tx: watch::Sender<TurnPosture>,
        initial_revision: u64,
    ) -> Self {
        Self {
            mode,
            supervisor,
            posture_tx,
            revision: AtomicU64::new(initial_revision),
        }
    }

    /// Publish the runtime state these settings call for. Returns immediately:
    /// convergence (which may involve tearing a server down and binding sockets)
    /// happens in the supervisor, and a caller that waited for it would hold an
    /// HTTP request open across a restart it cannot influence.
    pub fn apply(&self, settings: &TurnSettings) {
        let revision = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
        let plan = turn_plan(&self.mode, settings, revision);
        self.posture_tx.send_replace(plan.posture);
        self.supervisor.apply(plan.desired);
    }

    /// Stop the runtime and the supervisor, returning once both are done.
    pub async fn shutdown(&self) {
        self.supervisor.shutdown().await;
    }
}

/// Stops this host's TURN runtime when the HTTP server that owns it goes away.
///
/// Held as application data, so actix drops it once the server has stopped —
/// the only moment an *embedded* server can act on, since the process it lives
/// in keeps running afterwards and a relay left listening would hold its UDP
/// port against the next start. `Drop` cannot await, so this only asks; the
/// supervisor performs the teardown on its own task.
pub struct TurnRuntimeStopGuard {
    supervisor: TurnSupervisorHandle,
}

impl TurnRuntimeStopGuard {
    pub fn new(supervisor: TurnSupervisorHandle) -> Self {
        Self { supervisor }
    }
}

impl Drop for TurnRuntimeStopGuard {
    fn drop(&mut self) {
        self.supervisor.request_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_turn::model::{TurnInterface, TurnTransport};
    use desk_turn::runtime::{TurnIntent, TurnRuntimeView};
    use desk_turn::supervisor::{BackoffConfig, DesiredState, spawn};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn settings(interfaces: &[&str], enable: bool) -> TurnSettings {
        TurnSettings {
            enable_turn: enable,
            static_auth_secret: Some("secret".into()),
            interfaces: interfaces
                .iter()
                .map(|listen| TurnInterface {
                    transport: TurnTransport::UDP,
                    listen: (*listen).into(),
                    external: "127.0.0.1:3478".into(),
                })
                .collect(),
            ..TurnSettings::default()
        }
    }

    fn driver() -> Arc<HostTurnDriver> {
        Arc::new(HostTurnDriver::new(web::Data::new(
            SharedConnectionMap::from(BTreeMap::new()),
        )))
    }

    fn control_and_view(mode: StartupMode) -> (TurnRuntimeControl, TurnRuntimeView) {
        let (posture_tx, posture_rx) = watch::channel(TurnPosture::new(TurnIntent::Unsupported));
        // These exercise the control/view surface, not usage accounting, so the
        // retirement queue is dropped rather than drained.
        let (supervisor, _retired) = spawn(
            driver(),
            DesiredState {
                revision: 0,
                params: None,
            },
            BackoffConfig {
                min: Duration::from_millis(5),
                max: Duration::from_millis(20),
            },
        );
        let view = TurnRuntimeView::new(supervisor.clone(), posture_rx);
        (
            TurnRuntimeControl::new(mode, supervisor, posture_tx, 0),
            view,
        )
    }

    async fn wait_for_runtime(view: &TurnRuntimeView, present: bool) -> Option<Arc<TurnApiState>> {
        for _ in 0..200 {
            let runtime = view.runtime();
            if runtime.is_some() == present {
                return runtime;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("runtime presence never became {present}");
    }

    /// The whole point of the control handle: saving settings makes the running
    /// server match them, with no restart of the process.
    #[tokio::test]
    async fn saving_settings_starts_stops_and_restarts_the_runtime() {
        let (control, view) = control_and_view(StartupMode::Default);

        control.apply(&settings(&["127.0.0.1:0"], true));
        let first = wait_for_runtime(&view, true)
            .await
            .expect("an enabled host with an interface relays");

        // A change that matters replaces the runtime...
        let mut rotated = settings(&["127.0.0.1:0"], true);
        rotated.static_auth_secret = Some("rotated".into());
        control.apply(&rotated);
        for _ in 0..200 {
            match view.runtime() {
                Some(current) if !Arc::ptr_eq(&current, &first) => break,
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
        let second = view.runtime().expect("the rotated runtime is serving");
        assert!(
            !Arc::ptr_eq(&first, &second),
            "a rotated secret must reach the running server"
        );
        assert_eq!(
            second.settings.static_auth_secret.as_deref(),
            Some("rotated"),
            "the running server signs with the secret that was saved"
        );

        // ...and switching the service off tears it down, freeing the port.
        control.apply(&settings(&["127.0.0.1:0"], false));
        wait_for_runtime(&view, false).await;
        control.shutdown().await;
    }

    /// Every configured interface is bound, not just the first: proven by
    /// occupying the second one's port beforehand and requiring the start to
    /// fail because of it.
    #[tokio::test]
    async fn all_configured_interfaces_reach_the_runtime() {
        let occupied = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let taken = occupied.local_addr().unwrap().to_string();

        let (control, view) = control_and_view(StartupMode::Default);
        control.apply(&settings(&["127.0.0.1:0", &taken], true));

        // The runtime can never come up while the second address is taken.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            assert!(
                view.runtime().is_none(),
                "a runtime came up without binding every configured interface"
            );
        }
        // Free the port and the same desired state converges, confirming the
        // occupied address was the only thing in the way.
        drop(occupied);
        control.apply(&settings(&["127.0.0.1:0", &taken], true));
        wait_for_runtime(&view, true).await;
        control.shutdown().await;
    }

    /// A mode that never hosts TURN ignores the settings entirely — an operator
    /// switching it on there gets an honest "unsupported", not a relay.
    #[tokio::test]
    async fn a_mode_that_never_relays_starts_nothing() {
        let (control, view) = control_and_view(StartupMode::DeskServer);
        control.apply(&settings(&["127.0.0.1:0"], true));
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            assert!(view.runtime().is_none());
        }
        assert_eq!(
            view.info().await.state,
            desk_turn::model::TurnRuntimeState::Unsupported
        );
        control.shutdown().await;
    }
}
