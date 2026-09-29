//! Parses SOCKS5 requests and opens the requested TCP connection.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The address requested by a SOCKS5 CONNECT command.
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

/// Completes SOCKS5 no-auth negotiation and connects the requested TCP target.
pub(super) async fn negotiate(mut client: TcpStream) -> io::Result<(TcpStream, TcpStream)> {
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
