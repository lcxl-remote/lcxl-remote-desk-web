pub mod agent;
pub mod clipboard_dispatcher;
pub mod connection_ceiling;
pub mod desktop_monitor;
pub mod exec;
pub mod exec_containment;
pub mod exec_pty;
pub mod exec_registry;
pub mod file_transfer_dispatcher;
pub mod input_dispatcher;
pub mod media_producer;
pub mod policy_mirror;
pub mod session;
pub mod shared_capture;
pub mod virtual_display;
pub mod whiteboard_dispatcher;

use crate::model::settings::Args;
use log::{error, info};

pub fn run_session_worker(args: Args, pipe_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    info!("SessionWorker starting, pipe_name={}", pipe_name);

    // Install default crypto provider for rustls. webrtc-dtls (used by webrtc-rs)
    // depends on rustls under the hood; without a registered provider the DTLS
    // handshake silently fails after ICE connects, leaving the peer connection
    // stuck at dtlsState=connecting. The Default/DeskServer/Signaling and
    // ServiceDaemon entry points already do this; SessionWorker must too.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Use actix-rt's System (the same runtime flavour used by every other
    // startup mode). It installs a `tokio::task::LocalSet`, which is required
    // by `actix_web::rt::spawn` / `awc::Client` calls reachable from the
    // signaling and host-control upstream paths. Spawning the worker on a
    // plain `tokio::runtime::Builder::new_multi_thread()` runtime panics those
    // call sites with "spawn_local called from outside of a `task::LocalSet`",
    // which aborts the whole worker process before the telemetry guard can
    // flush — leaving daemon-side restart loops with no diagnostic in the
    // worker log.
    let system = actix_web::rt::System::new();
    system.block_on(async {
        match session::WorkerSession::run(args, pipe_name).await {
            Ok(()) => {
                info!("SessionWorker exited normally");
                Ok(())
            }
            Err(e) => {
                error!("SessionWorker error: {}", e);
                Err(e)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    /// Regression: the worker runtime must run inside a `tokio::task::LocalSet`
    /// so that `actix_web::rt::spawn` (used by the host-control upstream task
    /// and by signaling-side request handling) does not panic with
    /// "spawn_local called from outside of a `task::LocalSet`". Building a
    /// plain `tokio::runtime::Builder::new_multi_thread()` runtime, as the
    /// worker did before this fix, would crash with that exact message and
    /// abort the whole process before the telemetry guard could flush.
    #[test]
    fn worker_runtime_supports_actix_local_spawn() {
        let system = actix_web::rt::System::new();
        let outcome = system.block_on(async {
            let (tx, rx) = tokio::sync::oneshot::channel::<u32>();
            actix_web::rt::spawn(async move {
                let _ = tx.send(42);
            });
            rx.await.expect("spawn_local task must run to completion")
        });
        assert_eq!(outcome, 42);
    }
}
