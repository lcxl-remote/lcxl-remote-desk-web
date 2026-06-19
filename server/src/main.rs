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

/// Offline OpenAPI dump command. Parsed only when `argv[1]` is exactly
/// `dump-openapi`, *before* the legacy `ServerArgs` / `Args` parsers, so it
/// connects no infrastructure (no config / DB / Redis / runtime). Strict
/// (no `ignore_errors`): an unknown flag fails via clap as usual.
#[derive(clap::Parser, Debug)]
struct DumpOpenapiCli {
    /// Output path for the generated `openapi.json`.
    #[arg(long, default_value = "openapi.json")]
    out: String,
}

fn main() {
    // Offline OpenAPI dump: handled first, on every platform, before any other
    // argv parsing or startup. Gated on an exact `argv[1] == "dump-openapi"`
    // match so it never interferes with `--startup-mode` / `--pipe` / Windows
    // service flags. `parse_from(args().skip(1))` makes `dump-openapi` occupy
    // clap's ignored bin-name slot, leaving only `--out` to parse.
    if std::env::args().nth(1).as_deref() == Some("dump-openapi") {
        let cli = DumpOpenapiCli::parse_from(std::env::args().skip(1));
        let spec = lcxl_remote_desk_server::build_openapi();
        match spec.to_json() {
            Ok(json) => {
                if let Err(e) = std::fs::write(&cli.out, json) {
                    eprintln!("dump-openapi: failed to write {}: {e}", cli.out);
                    std::process::exit(1);
                }
                eprintln!("Wrote OpenAPI spec to {}", cli.out);
            }
            Err(e) => {
                eprintln!("dump-openapi: failed to serialize spec: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

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

#[cfg(test)]
mod tests {
    use super::DumpOpenapiCli;
    use clap::Parser as _;

    /// Mirrors the gate in `main`: only an exact `dump-openapi` first argument
    /// (after the program name) enters the offline-dump branch.
    fn is_dump_invocation(argv: &[&str]) -> bool {
        argv.get(1).copied() == Some("dump-openapi")
    }

    // `parse_from` treats the first element as the (ignored) bin-name slot, so
    // these mirror the real `parse_from(std::env::args().skip(1))` call.
    #[test]
    fn dump_cli_defaults_out_to_openapi_json() {
        let cli = DumpOpenapiCli::parse_from(["dump-openapi"]);
        assert_eq!(cli.out, "openapi.json");
    }

    #[test]
    fn dump_cli_accepts_explicit_out() {
        let cli = DumpOpenapiCli::parse_from(["dump-openapi", "--out", "foo.json"]);
        assert_eq!(cli.out, "foo.json");
    }

    #[test]
    fn dump_cli_rejects_unknown_flag() {
        assert!(DumpOpenapiCli::try_parse_from(["dump-openapi", "--nope"]).is_err());
    }

    #[test]
    fn legacy_argv_does_not_enter_dump_branch() {
        assert!(!is_dump_invocation(&[
            "server",
            "--startup-mode",
            "session-worker",
            "--pipe",
            "p",
        ]));
        assert!(!is_dump_invocation(&["server", "--install-service"]));
        assert!(is_dump_invocation(&["server", "dump-openapi"]));
    }
}
