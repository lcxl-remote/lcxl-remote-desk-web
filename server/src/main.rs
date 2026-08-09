use clap::Parser as _;
use desk_utils::host_data_paths::HostDataPaths;
use lcxl_remote_desk_server::model::settings::{Args, StartupMode};
use std::path::PathBuf;

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

    /// Explicit host config override inherited by the installed service.
    #[cfg(target_os = "windows")]
    #[arg(long)]
    config_file_path: Option<PathBuf>,

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

/// One-shot local remote-access controls. These commands only talk to the
/// daemon's authenticated native endpoint; they never start HTTP/signaling.
#[derive(clap::Parser, Debug)]
struct AccessCli {
    /// Config path used to locate the daemon's native control endpoint.
    #[arg(short, long)]
    config_file_path: Option<PathBuf>,

    #[command(subcommand)]
    command: AccessCommand,
}

#[derive(clap::Subcommand, Debug)]
enum AccessCommand {
    /// Print the durable local remote-access state as JSON.
    Status,
    /// Disconnect every current remote session and lock new admissions.
    Lock,
    /// Unlock after local OS elevation. Defaults to the current state version.
    Unlock {
        #[arg(long)]
        expected_version: Option<u64>,
    },
    /// Disconnect one current remote session without locking the host.
    Disconnect { connection_id: String },
}

fn run_access_cli(cli: AccessCli) -> anyhow::Result<()> {
    use lcxl_remote_desk_server::daemon::local_access_control::HostAccessControlAction;
    use lcxl_remote_desk_server::daemon::local_access_control_transport::{
        endpoint_for_paths, execute_native, query_native,
    };

    let paths = HostDataPaths::resolve_current(cli.config_file_path.as_deref())?;
    let endpoint = endpoint_for_paths(&paths);
    actix_web::rt::System::new().block_on(async move {
        if matches!(cli.command, AccessCommand::Status) {
            let status = match query_native(&endpoint).await {
                Ok(status) => status,
                Err(_) => {
                    let state = lcxl_remote_desk_server::daemon::remote_access::RemoteAccessStateStore::new(
                        paths.remote_access_state_file().to_path_buf(),
                    )
                    .load_read_only();
                    (&state).into()
                }
            };
            println!("{}", serde_json::to_string_pretty(&status)?);
            return Ok(());
        }

        let action = match cli.command {
            AccessCommand::Status => unreachable!(),
            AccessCommand::Lock => {
                confirm_cli_lock()?;
                HostAccessControlAction::LockAll
            }
            AccessCommand::Unlock { expected_version } => {
                let expected_version = match expected_version {
                    Some(version) => version,
                    None => query_native(&endpoint).await?.state_version,
                };
                HostAccessControlAction::Unlock { expected_version }
            }
            AccessCommand::Disconnect { connection_id } => {
                HostAccessControlAction::DisconnectConnection { connection_id }
            }
        };
        let result = execute_native(&endpoint, uuid::Uuid::new_v4().to_string(), action).await?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        Ok(())
    })
}

fn confirm_cli_lock() -> anyhow::Result<()> {
    use std::io::{IsTerminal as _, Write as _};

    if !std::io::stdin().is_terminal() {
        anyhow::bail!("access lock requires an interactive local terminal");
    }
    eprint!("Disconnect all remote sessions and lock new remote access? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        anyhow::bail!("lock cancelled");
    }
    Ok(())
}

fn main() {
    // Local access control is deliberately handled before the permissive
    // legacy parser and before any server infrastructure is initialized.
    if std::env::args().nth(1).as_deref() == Some("access") {
        let cli = AccessCli::parse_from(std::env::args().skip(1));
        if let Err(error) = run_access_cli(cli) {
            eprintln!("access: {error:#}");
            std::process::exit(1);
        }
        return;
    }

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
            if let Err(e) = install_service(
                dir,
                server_args.install_idd_driver,
                server_args.config_file_path.as_deref(),
            ) {
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
    use super::{AccessCli, AccessCommand, DumpOpenapiCli};
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

    #[test]
    fn access_cli_parses_all_commands_strictly() {
        let cli = AccessCli::try_parse_from(["access", "lock"]).unwrap();
        assert!(matches!(cli.command, AccessCommand::Lock));

        let cli = AccessCli::try_parse_from([
            "access",
            "--config-file-path",
            "state/config",
            "unlock",
            "--expected-version",
            "7",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            AccessCommand::Unlock {
                expected_version: Some(7)
            }
        ));

        let cli = AccessCli::try_parse_from(["access", "disconnect", "peer-1"]).unwrap();
        assert!(matches!(
            cli.command,
            AccessCommand::Disconnect { connection_id } if connection_id == "peer-1"
        ));
        assert!(AccessCli::try_parse_from(["access", "--unknown"]).is_err());
    }
}
