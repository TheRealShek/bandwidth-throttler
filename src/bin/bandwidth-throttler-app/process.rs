//! Runs the selected app as the invoking user and handles its signals.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::time::timeout;

use super::Result;
use super::network::NAMESPACE;
use super::system::{run, stop_on_parent_death};

/// Reads the identity that sudo authenticated; the selected app never runs as root.
pub(super) fn invoking_user() -> Result<User> {
    let uid: u32 = std::env::var("SUDO_UID")?.parse()?;
    let gid: u32 = std::env::var("SUDO_GID")?.parse()?;
    if uid == 0 {
        return Err("Run through sudo from your normal user account.".into());
    }
    let output = run("/usr/bin/getent", &["passwd", &uid.to_string()])?;
    let fields: Vec<_> = output.trim_end().split(':').collect();
    if fields.len() < 7 || fields[2] != uid.to_string() || fields[3] != gid.to_string() {
        return Err("Cannot resolve the invoking user's account.".into());
    }
    Ok(User {
        uid,
        gid,
        name: fields[0].to_owned(),
        home: fields[5].to_owned(),
    })
}

/// The account that sudo authenticated before the launcher gained root.
pub(super) struct User {
    uid: u32,
    gid: u32,
    name: String,
    home: String,
}

/// Executes the app inside the namespace after dropping to the sudo caller.
pub(super) async fn run_app(
    program: &[OsString],
    user: &User,
    interrupt: &mut tokio::signal::unix::Signal,
    terminate: &mut tokio::signal::unix::Signal,
) -> Result<u8> {
    let mut command = tokio::process::Command::new("/usr/bin/ip");
    command
        .env_clear()
        .arg("netns")
        .arg("exec")
        .arg(NAMESPACE)
        .arg("/usr/bin/setpriv")
        .arg("--reuid")
        .arg(user.uid.to_string())
        .arg("--regid")
        .arg(user.gid.to_string())
        .arg("--init-groups")
        .arg("--")
        .args(program)
        .env("HOME", &user.home)
        .env("USER", &user.name)
        .env("LOGNAME", &user.name)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("XDG_RUNTIME_DIR", format!("/run/user/{}", user.uid));
    for key in [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "XAUTHORITY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
        "LANG",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        command.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path=/run/user/{}/bus", user.uid),
        );
    }
    command.as_std_mut().process_group(0);
    stop_on_parent_death(command.as_std_mut());
    let mut child = command.spawn()?;
    let pid = Pid::from_raw(child.id().ok_or("App process has no PID")? as i32);
    let status = tokio::select! {
        result = child.wait() => result?,
        _ = interrupt.recv() => stop_child(&mut child, pid).await?,
        _ = terminate.recv() => stop_child(&mut child, pid).await?,
    };
    Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

/// Gives the app a short grace period before killing its process group.
async fn stop_child(
    child: &mut tokio::process::Child,
    group: Pid,
) -> Result<std::process::ExitStatus> {
    signal_group(group, Signal::SIGTERM)?;
    match timeout(Duration::from_secs(3), child.wait()).await {
        Ok(result) => Ok(result?),
        Err(_) => {
            signal_group(group, Signal::SIGKILL)?;
            Ok(timeout(Duration::from_secs(2), child.wait())
                .await
                .map_err(|_| "Application did not exit after SIGKILL")??)
        }
    }
}

fn signal_group(group: Pid, signal: Signal) -> Result<()> {
    match killpg(group, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn graceful_shutdown_terminates_the_child() {
        let mut command = tokio::process::Command::new("/usr/bin/sleep");
        command.arg("30");
        command.as_std_mut().process_group(0);
        let mut child = command.spawn().unwrap();
        let group = Pid::from_raw(child.id().unwrap() as i32);

        let status = stop_child(&mut child, group).await.unwrap();
        assert!(!status.success());
    }
}
