# Bandwidth throttler prototypes

This repository has two experimental ways to cap bandwidth. The original SOCKS5 proxy limits TCP traffic sent through it without root. The Linux app launcher limits traffic from an application started inside its own network namespace. Both give the selected traffic a shared download cap and a shared upload cap. Neither reserves bandwidth for other applications.

## Linux app launcher

Build as your normal user, then launch one application through the new binary:

```sh
cargo build --release --bin bandwidth-throttler-app
sudo target/release/bandwidth-throttler-app run 8 8 -- curl -L -o /dev/null https://example.com/
```

The numbers are decimal megabits per second, download then upload. `8` Mbps equals `1` MB/s. The app runs as your normal user after the launcher configures networking with `sudo`. Its child processes inherit the same network namespace and share the two limits. Apps already running are unaffected.

For a GUI app on Wayland, preserve the session variables that `sudo` may otherwise remove. Give Chrome a fresh profile so it cannot hand the request to an existing Chrome process:

```sh
mkdir -p "$HOME/.local/share/bandwidth-throttler/chrome-netns-test"
sudo --preserve-env=WAYLAND_DISPLAY,DISPLAY,DBUS_SESSION_BUS_ADDRESS,XAUTHORITY,XDG_CURRENT_DESKTOP,XDG_SESSION_TYPE \
  target/release/bandwidth-throttler-app run 8 8 -- \
  google-chrome-stable --user-data-dir="$HOME/.local/share/bandwidth-throttler/chrome-netns-test"
```

Press Ctrl+C to ask the app to stop. The launcher then removes its network namespace, virtual interfaces, traffic limits, resolver file, and firewall table, and restores the uplink's previous forwarding setting. A separate guard process performs the same cleanup if the launcher is killed without a chance to handle the signal. If both processes are killed or cleanup fails, the next run retries cleanup before starting another app. You can also retry it explicitly:

```sh
sudo target/release/bandwidth-throttler-app cleanup
```

After a reboot, run `cleanup` or start another session to remove any resolver files left under `/etc/netns`. The launcher checks a persistent owner record before removing them. It does not run automatically during boot. If both the launcher and guard die in the instant after creating `/etc/netns` but before saving that record, a reboot can leave an empty `/etc/netns` directory. The launcher leaves that directory alone because it cannot prove ownership; it has no effect on network traffic.

The launcher allows one capped app session at a time. It uses IPv4 on the host's current default route and a DNS server discovered at startup; IPv6 has no internet route in this version. It does not follow Wi-Fi or VPN changes during a session. Inside the namespace, `127.0.0.1` refers to that namespace, so an app relying on a service on the host's loopback address may need additional work. Traffic delegated to a process that was already running outside the namespace is not capped. A host firewall that blocks forwarded traffic may prevent internet access; this tool does not replace existing firewall rules. The `tc` limits count packet bytes, so a download's reported payload speed can be a little lower than the configured rate. End-to-end transfers and cleanup passed in disposable namespaces; a real `sudo` run on the host remains to be tested.

## SOCKS5 proxy

The local SOCKS5 proxy gives all connections through one proxy a shared download cap and a shared upload cap. It supports TCP connections only. It does not need root or change Linux network settings. `cargo run` still starts this proxy by default.

## Run

From this directory:

```sh
cargo run --release -- 1 1
```

The first number is the download cap and the second is the upload cap, both in decimal megabits per second. `1` Mbps is `125,000` bytes per second. The proxy listens on `127.0.0.1:1080`. An optional third argument changes the port:

```sh
cargo run --release -- 1 0.5 1081
```

Keep the proxy running while testing. Press Ctrl+C to stop it. To give Chrome and Firefox separate limits, run two copies on different ports and configure each browser to use its own port. Browsers using the same port share both caps.

Every tab, image, script, and video in a proxied browser shares its download cap. At 1 Mbps, a 500 KB transfer takes about four seconds even when the proxy and network work correctly. [YouTube recommends about 5 Mbps for 1080p, 2.5 Mbps for 720p, and 0.7 Mbps for 360p](https://support.google.com/youtube/answer/3037019?hl=en). If your whole internet connection is 2 Mbps, no browser or proxy setting can sustain 1080p at that recommended rate. To keep normal browsing fast, proxy a traffic-heavy app that supports SOCKS5 or a separate browser profile, and leave your main browser outside the proxy.

## Chrome

Launch a separate Chrome profile so an already running Chrome session cannot take the request:

```sh
mkdir -p "$HOME/.local/share/bandwidth-throttler/chrome-test"
google-chrome-stable \
  --user-data-dir="$HOME/.local/share/bandwidth-throttler/chrome-test" \
  --proxy-server="socks5://127.0.0.1:1080"
```

The command above is for the `google-chrome-stable` package. Replace the executable name if your browser uses another Chromium build. The separate profile has its own cookies, extensions, and history.

## Firefox

Create a dedicated profile with `firefox -P` if you do not have one. Start it with `firefox --no-remote -P bandwidth-test`, replacing `bandwidth-test` with the profile name you chose. In that profile, search Firefox settings for **proxy**, open the connection settings, select **Manual proxy configuration**, and set:

- SOCKS host: `127.0.0.1`
- Port: `1080`
- SOCKS v5
- Proxy DNS when using SOCKS v5: enabled

Leave the HTTP and HTTPS proxy fields empty. Firefox stores these settings in the selected profile.

## Check the result

1. With the proxy running, open an HTTPS website and download a file large enough to run for several seconds. At a `1` Mbps cap, a `1` MB file should take roughly eight seconds, plus connection overhead.
2. Start two downloads at once. Their **combined** speed should remain near the configured download cap. The proxy prints transferred byte counts when each connection closes.
3. Stop the proxy and refresh a public website in the test profile. If it still loads, that traffic is bypassing the proxy or another browser instance opened the URL.
4. Repeat with a browser-managed file download and the sites you actually care about. Test video calls separately because direct UDP and WebRTC traffic are outside this prototype's TCP proxy.

You can check the proxy without a browser:

```sh
curl --noproxy '' --socks5-hostname 127.0.0.1:1080 -o /dev/null https://example.com/
```

If Chrome reports `ERR_SOCKS_CONNECTION_FAILED`, run that `curl` command and also try the same URL without `--socks5-hostname 127.0.0.1:1080`. If both fail, check the host's connection and DNS with `resolvectl query example.com`. A log entry naming an IPv6 target with `Network is unreachable` means the host has no route to that IPv6 address. A timed-out domain target can also mean its DNS lookup or TCP connection stalled. Browsers may cancel connections during navigation; the proxy omits routine broken-pipe and reset errors from its log.

The cap is an upper bound on bytes relayed by this proxy, not a target speed. A slower server or internet connection can deliver less. It does not guarantee the same instantaneous rate on the Wi-Fi interface, because incoming packets can arrive before the proxy slows the remote TCP sender. This prototype has a limit of 128 concurrent connections. It accepts only local clients and supports SOCKS5 without authentication, so do not expose its port to a network.
