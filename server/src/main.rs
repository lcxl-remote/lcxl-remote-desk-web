use clap::Parser as _;
use lcxl_remote_desk_server::model::settings::{Args, StartupMode};
use log::{error, info};

/// Extra flags for install/uninstall — parsed before the main startup-mode dispatch.
#[derive(clap::Parser, Debug, Default)]
#[command(ignore_errors = true)]
struct ServerArgs {
    /// Install the Windows Service (requires elevation)
    #[cfg(target_os = "windows")]
    #[arg(long)]
    install_service: bool,

    /// Uninstall the Windows Service (requires elevation)
    #[cfg(target_os = "windows")]
    #[arg(long)]
    uninstall_service: bool,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // Handle one-shot service management flags before entering any startup mode.
    #[cfg(target_os = "windows")]
    {
        let server_args = ServerArgs::parse();
        if server_args.install_service {
            use lcxl_remote_desk_server::daemon::windows_service::install_service;
            if let Err(e) = install_service() {
                error!("Failed to install service: {e}");
                std::process::exit(1);
            }
            return;
        }
        if server_args.uninstall_service {
            use lcxl_remote_desk_server::daemon::windows_service::uninstall_service;
            if let Err(e) = uninstall_service() {
                error!("Failed to uninstall service: {e}");
                std::process::exit(1);
            }
            return;
        }
    }

    let args = Args::parse();

    info!(
        "lcxl-remote-desk-server starting, mode={:?}, pipe={:?}",
        args.startup_mode, args.pipe
    );

    match args.startup_mode {
        StartupMode::Default | StartupMode::Signaling | StartupMode::DeskServer => {
            info!("Starting in Portable mode");
            let system = actix_web::rt::System::new();
            let exit_code = system.block_on(async {
                match lcxl_remote_desk_server::run().await {
                    Ok(server) => {
                        info!("Server started successfully");
                        if let Err(e) = server.await {
                            error!("Server error: {e}");
                            1
                        } else {
                            0
                        }
                    }
                    Err(e) => {
                        error!("Failed to start server: {e}");
                        1
                    }
                }
            });
            std::process::exit(exit_code);
        }
        StartupMode::ServiceDaemon => {
            info!("Starting in ServiceDaemon mode");
            if let Err(e) = lcxl_remote_desk_server::daemon::run_service_daemon(args) {
                error!("ServiceDaemon failed: {e}");
                std::process::exit(1);
            }
        }
        StartupMode::SessionWorker => {
            info!("Starting in SessionWorker mode");
            let pipe_name = args.pipe.clone().unwrap_or_else(|| {
                error!("--pipe argument is required for SessionWorker mode");
                std::process::exit(1);
            });
            if let Err(e) = lcxl_remote_desk_server::worker::run_session_worker(args, &pipe_name) {
                error!("SessionWorker failed: {e}");
                std::process::exit(1);
            }
        }
    }
}
