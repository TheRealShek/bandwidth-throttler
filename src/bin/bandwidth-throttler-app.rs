//! Launches one application behind a kernel-enforced, bidirectional rate limit.

#[path = "bandwidth-throttler-app/network.rs"]
mod network;
#[path = "bandwidth-throttler-app/process.rs"]
mod process;
#[path = "bandwidth-throttler-app/system.rs"]
mod system;

use std::ffi::OsString;
use std::io;

use bandwidth_throttler::parse_rate;
use nix::unistd::geteuid;
use tokio::signal::unix::{SignalKind, signal};

use network::{Session, recover};
use process::{invoking_user, run_app};
use system::{acquire_lock, spawn_guard};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Bandwidth caps and the selected command from the launcher CLI.
struct Config {
    download: u64,
    upload: u64,
    program: Vec<OsString>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match entry().await {
        Ok(code) => std::process::ExitCode::from(code),
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Parses the new launcher command without changing the existing SOCKS proxy CLI.
async fn entry() -> Result<u8> {
    let mut args = std::env::args_os().skip(1);
    let action = args.next().ok_or_else(usage)?;
    if action != "run" && action != "cleanup" && action != "__guard" {
        return Err(usage().into());
    }
    if !geteuid().is_root() {
        return Err("Run the built binary with sudo. Build it as your normal user first.".into());
    }

    if action == "__guard" {
        if args.next().is_some() {
            return Err(usage().into());
        }
        // The launcher owns stdin. EOF means it exited, even without unwinding.
        let mut input = io::stdin().lock();
        io::copy(&mut input, &mut io::sink())?;
        let _lock = acquire_lock(true)?;
        recover()?;
        return Ok(0);
    }

    let _lock = acquire_lock(false)?;
    if action == "cleanup" {
        if args.next().is_some() {
            return Err(usage().into());
        }
        recover()?;
        return Ok(0);
    }

    let config = parse_run_args(args)?;
    let user = invoking_user()?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    recover()?;
    let _guard = spawn_guard()?;
    let mut session = Session::start(config.download, config.upload)?;
    eprintln!(
        "App cap: download {:.3} Mbps, upload {:.3} Mbps. Press Ctrl+C to stop.",
        config.download as f64 / 125_000.0,
        config.upload as f64 / 125_000.0
    );
    let result = run_app(&config.program, &user, &mut interrupt, &mut terminate).await;
    let cleanup = session.cleanup();
    match (result, cleanup) {
        (Ok(code), Ok(())) => Ok(code),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(app_error), Err(cleanup_error)) => Err(format!(
            "Application failed: {app_error}. Cleanup also failed: {cleanup_error}"
        )
        .into()),
    }
}

fn usage() -> &'static str {
    "Usage: sudo bandwidth-throttler-app run <download-Mbps> <upload-Mbps> -- <program> [args...]\n       sudo bandwidth-throttler-app cleanup"
}

/// Reads rates and an executable plus its exact arguments.
fn parse_run_args(mut args: impl Iterator<Item = OsString>) -> Result<Config> {
    let download = args.next().ok_or_else(usage)?;
    let upload = args.next().ok_or_else(usage)?;
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        return Err(usage().into());
    }
    let program: Vec<_> = args.collect();
    if program.is_empty() || program[0].is_empty() {
        return Err(usage().into());
    }
    let download = parse_rate(&download.to_string_lossy())?;
    let upload = parse_rate(&upload.to_string_lossy())?;
    if download > u64::MAX / 8 || upload > u64::MAX / 8 {
        return Err("Rate is too large for tc.".into());
    }
    Ok(Config {
        download,
        upload,
        program,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_program_and_overflowing_rate() {
        assert!(parse_run_args(["8", "8", "--"].into_iter().map(OsString::from)).is_err());
        assert!(parse_run_args(["8", "8", "--", "curl"].into_iter().map(OsString::from)).is_ok());
        assert!(
            parse_run_args(
                ["99999999999999", "8", "--", "curl"]
                    .into_iter()
                    .map(OsString::from)
            )
            .is_err()
        );
    }
}
