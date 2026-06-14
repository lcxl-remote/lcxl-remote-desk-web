use clap::Parser as _;
use lcxl_remote_desk_server::model::settings::{Args, StartupMode};

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

    /// Target installation directory for --install-service
    #[cfg(target_os = "windows")]
    #[arg(long)]
    install_path: Option<String>,

    /// Also stage the LcxlVirtualDisplay IDD driver during
    /// `--install-service`. Ignored unless `--install-service` is set.
    #[cfg(target_os = "windows")]
    #[arg(long)]
    install_idd_driver: bool,
}

fn main() {
    // Handle one-shot service management flags before entering any startup mode.
    // Use plain println!/eprintln! — no logging framework is initialised yet.
    #[cfg(target_os = "windows")]
    {
        let server_args = ServerArgs::parse();
        if server_args.install_service {
            use lcxl_remote_desk_server::daemon::windows_service::{
                default_install_dir, install_service,
            };
            let default_dir = default_install_dir();
            let dir = server_args.install_path.as_deref().unwrap_or(&default_dir);
            if let Err(e) = install_service(dir, server_args.install_idd_driver) {
                eprintln!("Failed to install service: {e}");
                std::process::exit(1);
            }
            println!("Service installed successfully");
            return;
        }
        if server_args.uninstall_service {
            use lcxl_remote_desk_server::daemon::windows_service::uninstall_service;
            if let Err(e) = uninstall_service() {
                eprintln!("Failed to uninstall service: {e}");
                std::process::exit(1);
            }
            println!("Service uninstalled successfully");
            return;
        }
    }

    let args = Args::parse();

    match args.startup_mode {
        StartupMode::Default | StartupMode::Signaling | StartupMode::DeskServer => {
            // telemetry::init_telemetry() inside run() owns all logging for this path.
            // Do NOT initialise any logger here — it would conflict.
            let system = actix_web::rt::System::new();
            let exit_code = system.block_on(async {
                match lcxl_remote_desk_server::run().await {
                    Ok((server, _telemetry_guard)) => {
                        // Hold _telemetry_guard until server.await completes;
                        // dropping earlier closes the non-blocking log writer
                        // thread and silently discards all subsequent lines.
                        if let Err(e) = server.await {
                            eprintln!("Server error: {e}");
                            1
                        } else {
                            0
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to start server: {e}");
                        1
                    }
                }
            });
            std::process::exit(exit_code);
        }
        StartupMode::ServiceDaemon => {
            if let Err(e) = lcxl_remote_desk_server::daemon::run_service_daemon(args) {
                eprintln!("ServiceDaemon failed: {e}");
                std::process::exit(1);
            }
        }
        StartupMode::SessionWorker => {
            let pipe_name = args.pipe.clone().unwrap_or_else(|| {
                eprintln!("--pipe argument is required for SessionWorker mode");
                std::process::exit(1);
            });
            if let Err(e) = lcxl_remote_desk_server::worker::run_session_worker(args, &pipe_name) {
                eprintln!("SessionWorker failed: {e}");
                std::process::exit(1);
            }
        }
        StartupMode::McpStdio => {
            // stdout carries the MCP protocol — never print to it here.
            if let Err(e) = lcxl_remote_desk_server::mcp::run_mcp_stdio(args) {
                eprintln!("McpStdio failed: {e}");
                std::process::exit(1);
            }
        }
    }
}
