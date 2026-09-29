//! Recovers interrupted sessions and removes their host network resources.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use super::{
    APP_VETH, HOST_VETH, NAMESPACE, NFT_TABLE, STATE_FILE, State, forwarding_path, resolver,
};
use crate::Result;
use crate::system::{run, stop_on_parent_death};

/// Recovers a session left by a killed launcher before creating another one.
pub(crate) fn recover() -> Result<()> {
    if !Path::new(STATE_FILE).exists() {
        let temporary = format!("{STATE_FILE}.tmp");
        if Path::new(&temporary).exists() {
            fs::remove_file(temporary)?;
        }
        resolver::recover_after_reboot()?;
        return Ok(());
    }
    eprintln!("Cleaning up a previous bandwidth-throttler-app session...");
    let state = State::load()?;
    cleanup_resources(&state)?;
    fs::remove_file(STATE_FILE)?;
    Ok(())
}

/// Makes cleanup idempotent so a failed cleanup can be retried.
pub(super) fn cleanup_resources(state: &State) -> Result<()> {
    let mut errors = Vec::new();
    let processes_stopped = match stop_namespace_processes() {
        Ok(()) => true,
        Err(error) => {
            errors.push(error.to_string());
            false
        }
    };
    if let Err(error) = delete_if_present(
        "/usr/bin/nft",
        &["list", "table", "ip", NFT_TABLE],
        &["delete", "table", "ip", NFT_TABLE],
    ) {
        errors.push(error.to_string());
    }
    if let Err(error) = delete_if_present(
        "/usr/bin/ip",
        &["link", "show", "dev", HOST_VETH],
        &["link", "delete", HOST_VETH],
    ) {
        errors.push(error.to_string());
    }
    if let Err(error) = delete_if_present(
        "/usr/bin/ip",
        &["link", "show", "dev", APP_VETH],
        &["link", "delete", APP_VETH],
    ) {
        errors.push(error.to_string());
    }
    if processes_stopped {
        match namespace_exists() {
            Ok(true) => {
                if let Err(error) = run("/usr/bin/ip", &["netns", "delete", NAMESPACE]) {
                    errors.push(error.to_string());
                }
            }
            Ok(false) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }
    if let Err(error) = resolver::cleanup(state.resolver_parent_created) {
        errors.push(error.to_string());
    }
    let path = forwarding_path(&state.uplink);
    if Path::new(&path).exists()
        && let Err(error) = fs::write(&path, format!("{}\n", state.previous_forwarding))
    {
        errors.push(error.to_string());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}

fn namespace_exists() -> Result<bool> {
    let namespaces = run("/usr/bin/ip", &["netns", "list"])?;
    Ok(namespaces
        .lines()
        .any(|line| line.split_whitespace().next() == Some(NAMESPACE)))
}

fn delete_if_present(program: &str, probe: &[&str], deletion: &[&str]) -> Result<()> {
    if resource_exists(program, probe)? {
        run(program, deletion)?;
    }
    Ok(())
}

/// Distinguishes a missing resource from an actual inspection failure.
pub(super) fn resource_exists(program: &str, args: &[&str]) -> Result<bool> {
    let mut command = Command::new(program);
    command.args(args).env_clear();
    stop_on_parent_death(&mut command);
    let output = command.output()?;
    if output.status.success() {
        return Ok(true);
    }
    let error = String::from_utf8_lossy(&output.stderr);
    if error.contains("No such file or directory") || error.contains("does not exist") {
        return Ok(false);
    }
    Err(format!("Cannot inspect resource with {program}: {}", error.trim()).into())
}

/// Stops any children that outlived the main application process.
fn stop_namespace_processes() -> Result<()> {
    if !namespace_exists()? {
        return Ok(());
    }
    for process in namespace_pids()? {
        let _ = kill(process, Signal::SIGTERM);
    }
    for _ in 0..20 {
        if namespace_pids()?.is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for process in namespace_pids()? {
        let _ = kill(process, Signal::SIGKILL);
    }
    for _ in 0..10 {
        if namespace_pids()?.is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("Some application processes are still in the network namespace.".into())
}

fn namespace_pids() -> Result<Vec<Pid>> {
    let output = run("/usr/bin/ip", &["netns", "pids", NAMESPACE])?;
    output
        .split_whitespace()
        .map(|word| {
            let number: i32 = word.parse()?;
            if number <= 1 || number == std::process::id() as i32 {
                return Err("Unsafe PID in namespace listing.".into());
            }
            Ok(Pid::from_raw(number))
        })
        .collect()
}
