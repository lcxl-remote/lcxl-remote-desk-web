//! Explicit terminal consent; the open connection owns the control period.
use super::client::NativeInputControlClient;
use clap::Parser;
use std::{
    io::{self, BufRead, IsTerminal, Read},
    path::PathBuf,
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "ai-input-control",
    about = "Locally authorize bounded best-effort AI input blocking (GNOME Wayland)"
)]
struct Args {
    /// Socket path printed by the running session worker.
    socket: PathBuf,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u16).range(1..=300))]
    seconds: u16,
    /// Explicitly accept partial coverage if some existing devices cannot be grabbed.
    #[arg(long)]
    allow_partial: bool,
}

pub fn run() -> io::Result<()> {
    let args = Args::parse_from(std::env::args_os().skip(1));
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(io::Error::other(
            "Run this command interactively in a local terminal",
        ));
    }
    // Connect only after consent, so reading the explanation cannot occupy the
    // worker's short request timeout. No approval token is persisted or printed.
    eprintln!(
        "AI input control for {} seconds: best effort on existing devices only.\nNo screen hiding; new devices are not covered. Ctrl+Alt+L releases local input.\nPartial device failures: {}. No permissions will be changed.\nThis allows already authorized AI desktop actions; it does not grant new AI permissions.\nType START to begin; anything else cancels:",
        args.seconds,
        if args.allow_partial {
            "accepted and reported"
        } else {
            "reject and release"
        }
    );
    let mut confirmation = String::new();
    io::stdin().lock().take(64).read_line(&mut confirmation)?;
    if confirmation.trim() != "START" {
        return Err(io::Error::other("Local input control cancelled"));
    }
    // Let the confirmation key be released before the backend checks held
    // keys. Otherwise pressing Enter can itself make keyboard acquisition fail.
    eprintln!("Starting in 3 seconds. Release all keys; Ctrl+Alt+L will end blocking.");
    std::thread::sleep(Duration::from_secs(3));
    let client = NativeInputControlClient::connect(&args.socket, args.seconds, args.allow_partial)?;
    let report = client.report();
    eprintln!(
        "Active: {} blocked, {} failed, {} skipped. Keep this command connected. Ctrl+Alt+L ends blocking.",
        report.grabbed, report.failed, report.skipped
    );
    client.wait()?;
    eprintln!("AI input control ended; blocked devices released.");
    Ok(())
}
