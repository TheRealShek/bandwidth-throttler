//! Shares upload and download limits across local SOCKS5 connections.

mod socks5;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{Instant, sleep_until, timeout};

use socks5::negotiate;

const MAX_CONNECTIONS: usize = 128;
const CHUNK_SIZE: usize = 4 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Accepts loopback connections while bounding task and buffer counts.
pub(super) async fn serve(listener: TcpListener, download: u64, upload: u64) -> io::Result<()> {
    let limits = Arc::new(Limits::new(download, upload));
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
    use std::net::Ipv4Addr;

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
        let server = tokio::spawn(serve(proxy, 125_000, 125_000));

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
        let server = tokio::spawn(serve(proxy, 125_000, 125_000));
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&[5, 1, 2]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 0xff]);
        server.abort();
    }
}
