use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{Instant, sleep_until, timeout};

const MAX_CONNECTIONS: usize = 128;
const CHUNK_SIZE: usize = 4 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Holds the rate and the next time a chunk may leave the proxy.
struct RateLimit {
    bytes_per_second: u64,
    next: Mutex<Instant>,
}

impl RateLimit {
    fn new(bytes_per_second: u64) -> Self {
        Self {
            bytes_per_second,
            next: Mutex::new(Instant::now()),
        }
    }

    /// Grants capacity shared by every connection in this direction.
    async fn wait_for(&self, bytes: usize) {
        let interval = Duration::from_secs_f64(bytes as f64 / self.bytes_per_second as f64);
        let mut next = self.next.lock().await;
        // A late wakeup must not add its scheduling delay to every following chunk.
        let deadline = (*next + interval).max(Instant::now());
        sleep_until(deadline).await;
        *next = deadline;
    }
}

/// Uses one aggregate upload limit and one aggregate download limit.
struct Limits {
    upload: RateLimit,
    download: RateLimit,
}

impl Limits {
    fn new(download_bytes_per_second: u64, upload_bytes_per_second: u64) -> Self {
        Self {
            upload: RateLimit::new(upload_bytes_per_second),
            download: RateLimit::new(download_bytes_per_second),
        }
    }
}

enum Destination {
    Ip(SocketAddr),
    Domain(String, u16),
}

impl Destination {
    fn kind(&self) -> &'static str {
        match self {
            Self::Ip(SocketAddr::V4(_)) => "IPv4",
            Self::Ip(SocketAddr::V6(_)) => "IPv6",
            Self::Domain(_, _) => "domain",
        }
    }
}

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
    let limits = Arc::new(Limits::new(download, upload));
    if let Err(error) = serve(listener, limits).await {
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
    let download = parse_rate(&args[0])?;
    let upload = parse_rate(&args[1])?;
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

/// Converts decimal Mbps to bytes per second without accepting zero or overflow.
fn parse_rate(text: &str) -> Result<u64, String> {
    let mbps = text
        .parse::<f64>()
        .map_err(|_| format!("Invalid rate: {text}"))?;
    let bytes_per_second = mbps * 125_000.0;
    if !bytes_per_second.is_finite() || !(1.0..u64::MAX as f64).contains(&bytes_per_second) {
        return Err(format!(
            "Rate must be a positive finite number of Mbps: {text}"
        ));
    }
    Ok(bytes_per_second.round() as u64)
}

/// Accepts loopback connections while bounding task and buffer counts.
async fn serve(listener: TcpListener, limits: Arc<Limits>) -> io::Result<()> {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let slot = slots
            .clone()
            .acquire_owned()
            .await
            .map_err(io::Error::other)?;
        let (client, _) = listener.accept().await?;
        let limits = Arc::clone(&limits);
        tokio::spawn(async move {
            let _slot = slot;
            match timeout(HANDSHAKE_TIMEOUT, negotiate(client)).await {
                Ok(Ok((client, upstream))) => {
                    if let Err(error) = relay_both(client, upstream, &limits).await
                        && !matches!(
                            error.kind(),
                            io::ErrorKind::BrokenPipe
                                | io::ErrorKind::ConnectionAborted
                                | io::ErrorKind::ConnectionReset
                        )
                    {
                        eprintln!("Connection ended with error: {error}");
                    }
                }
                Ok(Err(error)) => eprintln!("SOCKS connection failed: {error}"),
                Err(_) => eprintln!("SOCKS handshake timed out"),
            }
        });
    }
}

/// Completes SOCKS5 no-auth negotiation and connects the requested TCP target.
async fn negotiate(mut client: TcpStream) -> io::Result<(TcpStream, TcpStream)> {
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).await?;
    if greeting[0] != 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported SOCKS version",
        ));
    }
    let mut methods = vec![0u8; usize::from(greeting[1])];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        client.write_all(&[5, 0xff]).await?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "no supported authentication method",
        ));
    }
    client.write_all(&[5, 0]).await?;

    let mut request = [0u8; 4];
    client.read_exact(&mut request).await?;
    if request[0] != 5 || request[2] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid SOCKS request",
        ));
    }
    if request[1] != 1 {
        send_reply(&mut client, 7, None).await?;
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "only TCP CONNECT is supported",
        ));
    }

    let destination = match request[3] {
        1 => {
            let mut address = [0u8; 4];
            client.read_exact(&mut address).await?;
            let port = read_port(&mut client).await?;
            Destination::Ip(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(address)), port))
        }
        3 => {
            let length = client.read_u8().await? as usize;
            if length == 0 {
                send_reply(&mut client, 8, None).await?;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "empty destination host",
                ));
            }
            let mut name = vec![0u8; length];
            client.read_exact(&mut name).await?;
            let host = String::from_utf8(name).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid destination host")
            })?;
            Destination::Domain(host, read_port(&mut client).await?)
        }
        4 => {
            let mut address = [0u8; 16];
            client.read_exact(&mut address).await?;
            let port = read_port(&mut client).await?;
            Destination::Ip(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(address)), port))
        }
        _ => {
            send_reply(&mut client, 8, None).await?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported address type",
            ));
        }
    };

    let destination_kind = destination.kind();
    let connection = timeout(CONNECT_TIMEOUT, async {
        match destination {
            Destination::Ip(address) => TcpStream::connect(address).await,
            Destination::Domain(host, port) => TcpStream::connect((host.as_str(), port)).await,
        }
    })
    .await;
    let upstream = match connection {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(error)) => {
            let reply = match error.kind() {
                io::ErrorKind::ConnectionRefused => 5,
                io::ErrorKind::NetworkUnreachable => 3,
                io::ErrorKind::HostUnreachable => 4,
                _ => 1,
            };
            send_reply(&mut client, reply, None).await?;
            return Err(io::Error::new(
                error.kind(),
                format!("{destination_kind} target: {error}"),
            ));
        }
        Err(_) => {
            send_reply(&mut client, 6, None).await?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{destination_kind} target connection timed out"),
            ));
        }
    };
    send_reply(&mut client, 0, Some(upstream.local_addr()?)).await?;
    Ok((client, upstream))
}

/// Reads the network-byte-order TCP destination port.
async fn read_port(client: &mut TcpStream) -> io::Result<u16> {
    let mut bytes = [0u8; 2];
    client.read_exact(&mut bytes).await?;
    Ok(u16::from_be_bytes(bytes))
}

/// Sends a SOCKS5 reply using the local TCP address on success.
async fn send_reply(
    client: &mut TcpStream,
    status: u8,
    bound: Option<SocketAddr>,
) -> io::Result<()> {
    match bound {
        Some(SocketAddr::V4(address)) => {
            let mut reply = vec![5, status, 0, 1];
            reply.extend_from_slice(&address.ip().octets());
            reply.extend_from_slice(&address.port().to_be_bytes());
            client.write_all(&reply).await
        }
        Some(SocketAddr::V6(address)) => {
            let mut reply = vec![5, status, 0, 4];
            reply.extend_from_slice(&address.ip().octets());
            reply.extend_from_slice(&address.port().to_be_bytes());
            client.write_all(&reply).await
        }
        None => client.write_all(&[5, status, 0, 1, 0, 0, 0, 0, 0, 0]).await,
    }
}

/// Relays both directions until both sides close, with independent shared caps.
async fn relay_both(client: TcpStream, upstream: TcpStream, limits: &Limits) -> io::Result<()> {
    let (client_read, client_write) = client.into_split();
    let (upstream_read, upstream_write) = upstream.into_split();
    let (uploaded, downloaded) = tokio::try_join!(
        relay(client_read, upstream_write, &limits.upload),
        relay(upstream_read, client_write, &limits.download),
    )?;
    if uploaded != 0 || downloaded != 0 {
        eprintln!("Connection closed: uploaded {uploaded} bytes, downloaded {downloaded} bytes");
    }
    Ok(())
}

/// Keeps each connection's turn near 10 ms so small responses are not stuck behind large ones.
async fn relay<R, W>(mut source: R, mut destination: W, limit: &RateLimit) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = [0u8; CHUNK_SIZE];
    let chunk_size = (limit.bytes_per_second / 100).clamp(1, CHUNK_SIZE as u64) as usize;
    let mut transferred = 0u64;
    loop {
        let count = source.read(&mut buffer[..chunk_size]).await?;
        if count == 0 {
            destination.shutdown().await?;
            return Ok(transferred);
        }
        limit.wait_for(count).await;
        destination.write_all(&buffer[..count]).await?;
        transferred += count as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn late_wakeup_does_not_reduce_the_next_chunk_rate() {
        let limit = RateLimit::new(1_000);
        limit.wait_for(100).await;
        tokio::time::advance(Duration::from_millis(20)).await;

        let start = Instant::now();
        limit.wait_for(100).await;
        assert_eq!(Instant::now() - start, Duration::from_millis(80));
    }

    #[tokio::test]
    async fn socks5_connect_relays_data() {
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let echo = tokio::spawn(async move {
            let (mut peer, _) = target.accept().await.unwrap();
            let mut message = [0u8; 4];
            peer.read_exact(&mut message).await.unwrap();
            peer.write_all(&message).await.unwrap();
        });

        let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let limits = Arc::new(Limits::new(125_000, 125_000));
        let server = tokio::spawn(serve(proxy, limits));

        let mut client = TcpStream::connect(proxy_address).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut method = [0u8; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [5, 0]);

        let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
        request.extend_from_slice(&target_port.to_be_bytes());
        client.write_all(&request).await.unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply[..2], &[5, 0]);

        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");

        echo.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn rejects_missing_no_auth_method() {
        let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = proxy.local_addr().unwrap();
        let server = tokio::spawn(serve(proxy, Arc::new(Limits::new(125_000, 125_000))));
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&[5, 1, 2]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 0xff]);
        server.abort();
    }
}
