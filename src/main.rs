//! Starts the local SOCKS5 bandwidth-limited proxy.

mod proxy;

use std::net::Ipv4Addr;

use bandwidth_throttler::parse_rate;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let (download, upload, port) = match parse_args() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}\nUsage: bandwidth-throttler <download-Mbps> <upload-Mbps> [port]");
            return std::process::ExitCode::from(2);
        }
    };

    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("Cannot listen on 127.0.0.1:{port}: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    eprintln!(
        "SOCKS5 proxy on 127.0.0.1:{port}; download {:.3} Mbps, upload {:.3} Mbps",
        download as f64 / 125_000.0,
        upload as f64 / 125_000.0
    );
    if let Err(error) = proxy::serve(listener, download, upload).await {
        eprintln!("Proxy stopped: {error}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

/// Parses decimal megabits per second and an optional local port.
fn parse_args() -> Result<(u64, u64, u16), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(2..=3).contains(&args.len()) {
        return Err("Expected download and upload limits.".into());
    }
    let download = parse_rate(&args[0]).map_err(|error| error.to_string())?;
    let upload = parse_rate(&args[1]).map_err(|error| error.to_string())?;
    let port = match args.get(2) {
        Some(value) => value
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| "Port must be between 1 and 65535.".to_owned())?,
        None => 1080,
    };
    Ok((download, upload, port))
}
