pub mod session;

use crate::model::settings::Args;
use log::{error, info};

pub fn run_session_worker(args: Args, pipe_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    info!("SessionWorker starting, pipe_name={}", pipe_name);

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
