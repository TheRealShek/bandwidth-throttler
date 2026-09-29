//! Locks session state and runs privileged helpers with parent-death protection.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use nix::errno::Errno;
use nix::sys::prctl;
use nix::sys::signal::Signal;
use nix::unistd::{getpid, getppid};

use super::Result;

const RUN_DIR: &str = "/run/bandwidth-throttler-app";
const LOCK_FILE: &str = "/run/bandwidth-throttler-app/lock";

/// Holds an advisory lock for the whole setup, app lifetime, and cleanup.
pub(super) fn acquire_lock(wait: bool) -> Result<File> {
    if !Path::new(RUN_DIR).exists() {
        DirBuilder::new().mode(0o700).create(RUN_DIR)?;
    }
    let metadata = fs::metadata(RUN_DIR)?;
    if metadata.uid() != 0 || metadata.mode() & 0o777 != 0o700 {
        return Err(format!("Unsafe permissions on {RUN_DIR}; expected root-owned 0700").into());
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(LOCK_FILE)?;
    if file.metadata()?.uid() != 0 {
        return Err("Lock file is not owned by root.".into());
    }
    if wait {
        file.lock()?;
    } else {
        file.try_lock()
            .map_err(|_| "Another bandwidth-throttler-app operation is running.")?;
    }
    Ok(file)
}

/// Keeps recovery alive if the launcher dies without running its cleanup code.
pub(super) fn spawn_guard() -> Result<std::process::Child> {
    let executable = std::env::current_exe()?;
    let child = Command::new(executable)
        .arg("__guard")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(child)
}

/// Runs a fixed system tool without a shell or the caller's PATH.
pub(super) fn run(program: &str, args: &[&str]) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args).env_clear();
    stop_on_parent_death(&mut command);
    let output = command.output()?;
    output_text(program, args, output)
}

/// Prevents an in-flight system command from changing resources after recovery.
pub(super) fn stop_on_parent_death(command: &mut Command) {
    let parent = getpid();
    // SAFETY: the pre-exec callback only makes prctl/getppid syscalls. It does
    // not allocate, lock, or access Rust state shared with another thread.
    unsafe {
        command.pre_exec(move || {
            prctl::set_pdeathsig(Signal::SIGKILL)
                .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
            if getppid() != parent {
                return Err(io::Error::from_raw_os_error(Errno::ESRCH as i32));
            }
            Ok(())
        });
    }
}

pub(super) fn output_text(program: &str, args: &[&str], output: Output) -> Result<String> {
    if !output.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}
