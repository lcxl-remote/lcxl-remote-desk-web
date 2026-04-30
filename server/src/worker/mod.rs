pub mod session;

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

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
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
