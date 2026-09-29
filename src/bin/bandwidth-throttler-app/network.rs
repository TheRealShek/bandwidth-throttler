//! Sets up the temporary network namespace and records host settings.

#[path = "network/recovery.rs"]
mod recovery;
#[path = "network/resolver.rs"]
mod resolver;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use super::Result;
use super::system::{output_text, run, stop_on_parent_death};
pub(super) use recovery::recover;
use recovery::{cleanup_resources, resource_exists};
use resolver::{RESOLVER_DIR, RESOLVER_PARENT};

const STATE_FILE: &str = "/run/bandwidth-throttler-app/state";
pub(super) const NAMESPACE: &str = "bt-app-v1";
const HOST_VETH: &str = "bt-host-v1";
const APP_VETH: &str = "bt-peer-v1";
const NFT_TABLE: &str = "bt_app_v1";
const HOST_ADDRESS: &str = "10.253.47.1/30";
const APP_ADDRESS: &str = "10.253.47.2/30";
const HOST_IP: &str = "10.253.47.1";
const APP_IP: &str = "10.253.47.2";

/// Records the host settings that cleanup must restore after a crash.
struct State {
    uplink: String,
    previous_forwarding: String,
    resolver_parent_created: bool,
}

/// Owns the temporary network resources for one selected app.
pub(super) struct Session {
    state: State,
    active: bool,
}

/// Discovers the interface used for the host's default IPv4 route.
fn default_uplink() -> Result<String> {
    let routes = run("/usr/bin/ip", &["-4", "route", "show", "default"])?;
    let line = routes
        .lines()
        .next()
        .ok_or("No default IPv4 route is available.")?;
    let name = route_device(line).ok_or("Default route has no network interface.")?;
    if !valid_interface(name) {
        return Err("Unsupported characters in the default network interface name.".into());
    }
    Ok(name.to_owned())
}

/// Refuses either fixed private address if another route already claims it.
fn check_private_address_route() -> Result<()> {
    for address in [HOST_IP, APP_IP] {
        let route = run("/usr/bin/ip", &["-4", "route", "get", "fibmatch", address])?;
        if route.split_whitespace().next() != Some("default") {
            return Err(format!("{address} is already covered by a non-default route.").into());
        }
    }
    Ok(())
}

fn valid_interface(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

/// Finds an IPv4 DNS server routed through the selected uplink.
fn upstream_dns(uplink: &str) -> Result<Ipv4Addr> {
    for path in ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"] {
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        for line in contents.lines() {
            let mut parts = line.split_whitespace();
            if parts.next() != Some("nameserver") {
                continue;
            }
            if let Some(Some(IpAddr::V4(address))) = parts.next().map(|s| s.parse().ok())
                && !address.is_loopback()
                && !address.is_unspecified()
            {
                let route = run("/usr/bin/ip", &["-4", "route", "get", &address.to_string()])?;
                if route_device(&route) == Some(uplink) {
                    return Ok(address);
                }
            }
        }
    }
    Err("No IPv4 DNS server is reachable through the default uplink.".into())
}

fn route_device(route: &str) -> Option<&str> {
    let words: Vec<_> = route.split_whitespace().collect();
    words
        .windows(2)
        .find(|pair| pair[0] == "dev")
        .map(|pair| pair[1])
}

/// Applies the NAT and forwarding rules in one nftables transaction.
fn add_firewall(uplink: &str) -> Result<()> {
    let rules = format!(
        "add table ip {NFT_TABLE}\n\
         add chain ip {NFT_TABLE} forward {{ type filter hook forward priority 0; policy accept; }}\n\
         add rule ip {NFT_TABLE} forward iifname \"{HOST_VETH}\" oifname \"{uplink}\" accept\n\
         add rule ip {NFT_TABLE} forward iifname \"{uplink}\" oifname \"{HOST_VETH}\" ct state established,related accept\n\
         add chain ip {NFT_TABLE} postrouting {{ type nat hook postrouting priority 100; policy accept; }}\n\
         add rule ip {NFT_TABLE} postrouting ip saddr {APP_IP}/32 oifname \"{uplink}\" masquerade\n"
    );
    let mut command = Command::new("/usr/bin/nft");
    command
        .args(["-f", "-"])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    stop_on_parent_death(&mut command);
    let mut child = command.spawn()?;
    let write_result = child
        .stdin
        .take()
        .ok_or("Cannot open nftables input")?
        .write_all(rules.as_bytes());
    let output = child.wait_with_output()?;
    write_result?;
    output_text("/usr/bin/nft", &["-f", "-"], output)?;
    Ok(())
}

/// Checks for resources with our fixed names before claiming them.
fn check_name_collisions() -> Result<()> {
    let namespaces = run("/usr/bin/ip", &["netns", "list"])?;
    if namespaces
        .lines()
        .any(|line| line.split_whitespace().next() == Some(NAMESPACE))
        || resource_exists("/usr/bin/ip", &["link", "show", "dev", HOST_VETH])?
        || resource_exists("/usr/bin/ip", &["link", "show", "dev", APP_VETH])?
        || resource_exists("/usr/bin/nft", &["list", "table", "ip", NFT_TABLE])?
        || Path::new(RESOLVER_DIR).exists()
    {
        return Err("Named network resources already exist without our state file; refusing to replace them.".into());
    }
    Ok(())
}

impl Session {
    /// Writes recovery state before the first host-network mutation.
    pub(super) fn start(download: u64, upload: u64) -> Result<Self> {
        check_name_collisions()?;
        check_private_address_route()?;
        let uplink = default_uplink()?;
        let dns = upstream_dns(&uplink)?;
        if Path::new(RESOLVER_PARENT).exists() && !Path::new(RESOLVER_PARENT).is_dir() {
            return Err(format!("{RESOLVER_PARENT} exists but is not a directory.").into());
        }
        let forward_path = forwarding_path(&uplink);
        let previous_forwarding = fs::read_to_string(&forward_path)?.trim().to_owned();
        if previous_forwarding != "0" && previous_forwarding != "1" {
            return Err("Unexpected forwarding setting on the uplink.".into());
        }
        let state = State {
            uplink,
            previous_forwarding,
            resolver_parent_created: !Path::new(RESOLVER_PARENT).exists(),
        };
        state.save()?;
        let mut session = Self {
            state,
            active: true,
        };
        if let Err(setup_error) = session.setup(download, upload, dns) {
            return match session.cleanup() {
                Ok(()) => Err(setup_error),
                Err(cleanup_error) => Err(format!(
                    "Setup failed: {setup_error}. Cleanup also failed: {cleanup_error}. Run cleanup again after fixing the cause."
                )
                .into()),
            };
        }
        Ok(session)
    }

    /// Connects the app namespace to the host, then caps both veth directions.
    fn setup(&self, download: u64, upload: u64, dns: Ipv4Addr) -> Result<()> {
        run("/usr/bin/ip", &["netns", "add", NAMESPACE])?;
        run(
            "/usr/bin/ip",
            &[
                "link", "add", HOST_VETH, "type", "veth", "peer", "name", APP_VETH,
            ],
        )?;
        run(
            "/usr/bin/ip",
            &["link", "set", APP_VETH, "netns", NAMESPACE],
        )?;
        run(
            "/usr/bin/ip",
            &["addr", "add", HOST_ADDRESS, "dev", HOST_VETH],
        )?;
        run("/usr/bin/ip", &["link", "set", HOST_VETH, "up"])?;
        run(
            "/usr/bin/ip",
            &["-n", NAMESPACE, "addr", "add", APP_ADDRESS, "dev", APP_VETH],
        )?;
        run("/usr/bin/ip", &["-n", NAMESPACE, "link", "set", "lo", "up"])?;
        run(
            "/usr/bin/ip",
            &["-n", NAMESPACE, "link", "set", APP_VETH, "up"],
        )?;
        run(
            "/usr/bin/ip",
            &["-n", NAMESPACE, "route", "add", "default", "via", HOST_IP],
        )?;

        resolver::create(dns, self.state.resolver_parent_created)?;

        // Change only the interfaces that carry this app's packets.
        fs::write(forwarding_path(&self.state.uplink), "1\n")?;
        fs::write(forwarding_path(HOST_VETH), "1\n")?;
        add_firewall(&self.state.uplink)?;

        let download_rate = tc_rate(download);
        let upload_rate = tc_rate(upload);
        let download_burst = tc_burst(download);
        let upload_burst = tc_burst(upload);
        run(
            "/usr/bin/tc",
            &[
                "qdisc",
                "add",
                "dev",
                HOST_VETH,
                "root",
                "tbf",
                "rate",
                &download_rate,
                "burst",
                &download_burst,
                "latency",
                "100ms",
            ],
        )?;
        run(
            "/usr/bin/ip",
            &[
                "netns",
                "exec",
                NAMESPACE,
                "/usr/bin/tc",
                "qdisc",
                "add",
                "dev",
                APP_VETH,
                "root",
                "tbf",
                "rate",
                &upload_rate,
                "burst",
                &upload_burst,
                "latency",
                "100ms",
            ],
        )?;
        Ok(())
    }

    /// Removes only session-owned resources, retaining the state file on failure.
    pub(super) fn cleanup(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let result = cleanup_resources(&self.state);
        if result.is_ok() {
            fs::remove_file(STATE_FILE)?;
            self.active = false;
        }
        result
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = self.cleanup()
        {
            eprintln!("Cleanup failed: {error}. Run `sudo bandwidth-throttler-app cleanup`.");
        }
    }
}

impl State {
    /// Stores the uplink value needed for cleanup after an abrupt exit.
    fn save(&self) -> Result<()> {
        let temporary = format!("{STATE_FILE}.tmp");
        if Path::new(&temporary).exists() {
            fs::remove_file(&temporary)?;
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        writeln!(file, "{}", self.uplink)?;
        writeln!(file, "{}", self.previous_forwarding)?;
        writeln!(file, "{}", u8::from(self.resolver_parent_created))?;
        file.sync_all()?;
        fs::rename(temporary, STATE_FILE)?;
        Ok(())
    }

    fn load() -> Result<Self> {
        let contents = fs::read_to_string(STATE_FILE)?;
        let mut lines = contents.lines();
        let uplink = lines.next().ok_or("Incomplete recovery state")?;
        let previous_forwarding = lines.next().ok_or("Incomplete recovery state")?;
        let resolver_parent_created = lines.next().ok_or("Incomplete recovery state")?;
        if !valid_interface(uplink)
            || !matches!(previous_forwarding, "0" | "1")
            || !matches!(resolver_parent_created, "0" | "1")
            || lines.next().is_some()
        {
            return Err("Invalid recovery state; leaving system resources untouched.".into());
        }
        Ok(Self {
            uplink: uplink.to_owned(),
            previous_forwarding: previous_forwarding.to_owned(),
            resolver_parent_created: resolver_parent_created == "1",
        })
    }
}

fn forwarding_path(interface: &str) -> String {
    format!("/proc/sys/net/ipv4/conf/{interface}/forwarding")
}

fn tc_rate(bytes_per_second: u64) -> String {
    format!("{}bit", bytes_per_second * 8)
}

fn tc_burst(bytes_per_second: u64) -> String {
    format!("{}b", (bytes_per_second / 50).clamp(16_384, 1_048_576))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_name_rejects_nft_syntax() {
        assert!(valid_interface("wlan0"));
        assert!(!valid_interface("wlan0\";flush ruleset"));
    }
}
