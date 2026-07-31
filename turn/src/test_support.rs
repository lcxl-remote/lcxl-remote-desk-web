//! Shared scaffolding for tests that need a real TURN runtime.
//!
//! `turn::Server` refuses to build without at least one bound connection, so
//! there is no socket-free stand-in for [`TurnApiState`]; every test that
//! exercises the published runtime binds an ephemeral loopback port instead.
//! Collected here so the supervisor, the runtime view and the controller tests
//! agree on one honest fake rather than three subtly different ones.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;

use crate::model::{Statistics, TurnApiState, TurnInterface, TurnSettings, TurnTransport};
use crate::runtime::{TurnIntent, TurnPosture, TurnRuntimeView};
use crate::service::startup_turn_server;
use crate::supervisor::{
    BackoffConfig, DesiredState, StartedRuntime, TurnRuntimeDriver, TurnRuntimeHandle,
    TurnRuntimeParams, TurnSupervisorHandle, spawn,
};

/// The advertised endpoint of every runtime built here. Never dialled — the
/// tests care about what is reported, not about relaying through it.
pub const EXTERNAL: &str = "203.0.113.7:3478";

/// Accepts every credential: these tests exercise lifecycle and reporting,
/// never the auth path.
pub struct AllowAll;

impl turn::auth::AuthHandler for AllowAll {
    fn auth_handle(
        &self,
        _username: &str,
        _realm: &str,
        _src_addr: std::net::SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        Ok(Vec::new())
    }
}

/// Settings for a single-interface runtime on an ephemeral loopback port.
pub fn loopback_settings(secret: Option<&str>) -> TurnSettings {
    TurnSettings {
        interfaces: vec![TurnInterface {
            transport: TurnTransport::UDP,
            listen: "127.0.0.1:0".into(),
            external: EXTERNAL.into(),
        }],
        static_auth_secret: secret.map(str::to_owned),
        ..TurnSettings::default()
    }
}

/// Start a real runtime on an ephemeral loopback port.
pub async fn loopback_runtime(settings: TurnSettings) -> Arc<TurnApiState> {
    startup_turn_server(
        settings,
        Arc::new(AllowAll),
        Arc::new(RwLock::new(Statistics::default())),
        Arc::new(crate::service::AllowAllRelayTrafficGate),
    )
    .await
    .expect("a loopback TURN runtime should start")
}

/// Driver that starts loopback runtimes carrying the requested secret, so a
/// test can prove a reader follows the *running* configuration.
pub struct LoopbackDriver;

struct LoopbackHandle {
    state: Arc<TurnApiState>,
}

#[async_trait]
impl TurnRuntimeHandle for LoopbackHandle {
    async fn close(&self) -> Result<(), String> {
        self.state.server.close().await.map_err(|e| e.to_string())
    }
}

#[async_trait]
impl TurnRuntimeDriver for LoopbackDriver {
    async fn start(&self, params: &TurnRuntimeParams) -> Result<StartedRuntime, String> {
        let api_state = loopback_runtime(loopback_settings(params.secret.as_deref())).await;
        Ok(StartedRuntime {
            handle: Arc::new(LoopbackHandle {
                state: api_state.clone(),
            }),
            api_state,
        })
    }
}

/// Runtime parameters that start a loopback runtime signing with `secret`.
pub fn loopback_params(secret: &str) -> TurnRuntimeParams {
    TurnRuntimeParams {
        realm: "localhost".into(),
        secret: Some(secret.into()),
        interfaces: loopback_settings(None).interfaces,
        relay_min_port: 50000,
        relay_max_port: 50050,
        identity: format!("id-{secret}"),
    }
}

/// A supervisor driving loopback runtimes, plus a view onto it. The posture
/// sender is returned so a test can move the host between "meant to run" and
/// the reasons it is not.
pub fn loopback_supervisor(
    intent: TurnIntent,
    initial: DesiredState,
) -> (
    TurnSupervisorHandle,
    TurnRuntimeView,
    watch::Sender<TurnPosture>,
) {
    let (posture_tx, posture_rx) = watch::channel(TurnPosture::new(intent));
    // Nothing here accounts for usage, so the retirement queue is dropped: the
    // supervisor then releases a retired runtime's counters with it instead of
    // holding them for a reader that will never come.
    let (supervisor, _retired) = spawn(
        Arc::new(LoopbackDriver),
        initial,
        BackoffConfig {
            min: Duration::from_millis(5),
            max: Duration::from_millis(20),
        },
    );
    let view = TurnRuntimeView::new(supervisor.clone(), posture_rx);
    (supervisor, view, posture_tx)
}

/// Wait until a runtime is (or is no longer) published.
pub async fn wait_for_runtime(view: &TurnRuntimeView, present: bool) -> Option<Arc<TurnApiState>> {
    for _ in 0..200 {
        let runtime = view.runtime();
        if runtime.is_some() == present {
            return runtime;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("runtime presence never became {present}");
}
