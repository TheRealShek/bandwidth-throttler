# Bandwidth throttler prototype

This is a local SOCKS5 proxy for testing browser bandwidth limits. It gives all connections through one proxy a shared download cap and a shared upload cap. It supports TCP connections only. It does not need root or change Linux network settings.

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
