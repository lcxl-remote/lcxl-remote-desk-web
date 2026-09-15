//! Isolated protocol fixture using the same hub entry point as the desktop shell.
//! This is not a replacement for native dialog/UI validation.
use clap::Parser;
use lcxl_remote_desk_server::{
    host_control::HostControlHub,
    model::settings::{Args, Settings, StartupMode},
};
use std::{path::Path, sync::Arc};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    {
        let mut args = std::env::args_os().skip(1);
        if args.next().as_deref()
            == Some(std::ffi::OsStr::new(
                lcxl_remote_desk_server::windows_office_helper::MODE,
            ))
        {
            if args.next().is_some() {
                std::process::exit(2);
            }
            std::process::exit(lcxl_remote_desk_server::windows_office_helper::run_stdio());
        }
    }
    run()
}

#[actix_web::main]
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if !matches!(args.startup_mode, StartupMode::Default) {
        return Err("fixture only supports Default".into());
    }
    let config = args
        .config_file_path
        .as_ref()
        .ok_or("explicit fixture config required")?;
    let allowed = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../target/assistant-validation-tmp")
        .canonicalize()?;
    if !config.canonicalize()?.starts_with(&allowed) {
        return Err("config must be inside the isolated validation directory".into());
    }
    let settings =
        Settings::new(&args).map_err(|error| std::io::Error::other(error.to_string()))?;
    let (server, _telemetry) = lcxl_remote_desk_server::run_with_hub(
        &settings,
        Some(Arc::new(HostControlHub::new_local())),
    )
    .await
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    server.await?;
    Ok(())
}
