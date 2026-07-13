use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{lookup_host, TcpListener, TcpStream, UdpSocket};

use super::upstream::{SlotId, Upstream, UpstreamMap};

const SOCKS_VERSION: u8 = 5;
const CMD_CONNECT: u8 = 1;
const CMD_UDP_ASSOCIATE: u8 = 3;
const ATYP_IPV4: u8 = 1;
const ATYP_DOMAIN: u8 = 3;
const ATYP_IPV6: u8 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocksAddress {
    Ip(SocketAddr),
    Domain(String, u16),
}

impl SocksAddress {
    async fn resolve(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Ip(address) => Ok(*address),
            Self::Domain(host, port) => lookup_host((host.as_str(), *port))
                .await?
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "address resolved empty")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocksRequest {
    pub command: u8,
    pub target: SocksAddress,
}

pub struct RelayServer {
    slot: SlotId,
    upstreams: UpstreamMap,
    listener: TcpListener,
    local_addr: SocketAddr,
}

impl RelayServer {
    pub async fn bind(
        slot: SlotId,
        upstreams: UpstreamMap,
        address: SocketAddr,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(address).await?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            slot,
            upstreams,
            listener,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn run(self) -> io::Result<()> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let upstreams = self.upstreams.clone();
            let slot = self.slot;
            tokio::spawn(async move {
                let _ = serve_client(stream, slot, upstreams).await;
            });
        }
    }
}

async fn serve_client(
    mut client: TcpStream,
    slot: SlotId,
    upstreams: UpstreamMap,
) -> io::Result<()> {
    accept_client_greeting(&mut client).await?;
    let request = read_request(&mut client).await?;
    let version = upstreams.version(slot);
    let Some(upstream) = upstreams.get(slot) else {
        write_reply(
            &mut client,
            2,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        )
        .await?;
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "slot upstream is blackholed",
        ));
    };

    match request.command {
        CMD_CONNECT => {
            relay_connect(client, request.target, upstream, slot, upstreams, version).await
        }
        CMD_UDP_ASSOCIATE => relay_udp(client, upstream, slot, upstreams, version).await,
        _ => {
            write_reply(
                &mut client,
                7,
                SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            )
            .await
        }
    }
}

async fn relay_connect(
    mut client: TcpStream,
    target: SocksAddress,
    upstream: Upstream,
    slot: SlotId,
    upstreams: UpstreamMap,
    version: u64,
) -> io::Result<()> {
    let mut remote = connect_upstream(&upstream).await?;
    upstream_request(&mut remote, CMD_CONNECT, &target).await?;
    let bound = read_reply(&mut remote).await?;
    write_reply(&mut client, 0, bound).await?;
    tokio::select! {
        result = tokio::io::copy_bidirectional(&mut client, &mut remote) => {
            result?;
        }
        _ = wait_for_swap(upstreams, slot, version) => {}
    }
    Ok(())
}

async fn relay_udp(
    mut client: TcpStream,
    upstream: Upstream,
    slot: SlotId,
    upstreams: UpstreamMap,
    version: u64,
) -> io::Result<()> {
    let mut control = connect_upstream(&upstream).await?;
    let unspecified = SocksAddress::Ip(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
    upstream_request(&mut control, CMD_UDP_ASSOCIATE, &unspecified).await?;
    let mut upstream_relay = read_reply(&mut control).await?;
    if upstream_relay.ip().is_unspecified() {
        upstream_relay.set_ip(control.peer_addr()?.ip());
    }

    // TCP and UDP share this slot's stable relay port. The namespace firewall
    // therefore needs exactly two narrow exceptions (one per transport), and a
    // tun2socks reconnect after hot-swap observes the same endpoint.
    let local_relay = client.local_addr()?;
    let udp = UdpSocket::bind(local_relay).await?;
    write_reply(&mut client, 0, local_relay).await?;
    let mut client_addr = None;
    let mut buffer = vec![0_u8; 65_535];
    loop {
        tokio::select! {
            _ = wait_for_swap(upstreams.clone(), slot, version) => return Ok(()),
            read = client.read_u8() => {
                match read {
                    Ok(_) => continue,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            received = udp.recv_from(&mut buffer) => {
                let (length, source) = received?;
                if source == upstream_relay {
                    if let Some(client_addr) = client_addr {
                        udp.send_to(&buffer[..length], client_addr).await?;
                    }
                } else {
                    validate_udp_packet(&buffer[..length])?;
                    client_addr = Some(source);
                    udp.send_to(&buffer[..length], upstream_relay).await?;
                }
            }
        }
    }
}

async fn wait_for_swap(upstreams: UpstreamMap, slot: SlotId, version: u64) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if upstreams.version(slot) != version {
            return;
        }
    }
}

async fn connect_upstream(upstream: &Upstream) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect((upstream.host.as_str(), upstream.port)).await?;
    match (&upstream.username, &upstream.password) {
        (Some(username), password) => {
            stream.write_all(&[SOCKS_VERSION, 1, 2]).await?;
            let response = read_exact_array::<2, _>(&mut stream).await?;
            if response != [SOCKS_VERSION, 2] {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "upstream rejected username/password authentication",
                ));
            }
            let password = password.as_deref().unwrap_or_default();
            if username.len() > u8::MAX as usize || password.len() > u8::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "upstream credentials exceed SOCKS5 limits",
                ));
            }
            stream.write_u8(1).await?;
            stream.write_u8(username.len() as u8).await?;
            stream.write_all(username.as_bytes()).await?;
            stream.write_u8(password.len() as u8).await?;
            stream.write_all(password.as_bytes()).await?;
            if read_exact_array::<2, _>(&mut stream).await? != [1, 0] {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "upstream credentials were rejected",
                ));
            }
        }
        _ => {
            stream.write_all(&[SOCKS_VERSION, 1, 0]).await?;
            if read_exact_array::<2, _>(&mut stream).await? != [SOCKS_VERSION, 0] {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "upstream rejected no-auth negotiation",
                ));
            }
        }
    }
    Ok(stream)
}

async fn upstream_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    command: u8,
    target: &SocksAddress,
) -> io::Result<()> {
    writer.write_all(&[SOCKS_VERSION, command, 0]).await?;
    write_address(writer, target).await
}

async fn accept_client_greeting(stream: &mut TcpStream) -> io::Result<()> {
    if stream.read_u8().await? != SOCKS_VERSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not SOCKS5"));
    }
    let methods = stream.read_u8().await? as usize;
    let mut offered = vec![0_u8; methods];
    stream.read_exact(&mut offered).await?;
    if !offered.contains(&0) {
        stream.write_all(&[SOCKS_VERSION, 0xff]).await?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local relay requires no-auth SOCKS5",
        ));
    }
    stream.write_all(&[SOCKS_VERSION, 0]).await
}

pub async fn read_request<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<SocksRequest> {
    let header = read_exact_array::<3, _>(reader).await?;
    if header[0] != SOCKS_VERSION || header[2] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid SOCKS5 request header",
        ));
    }
    Ok(SocksRequest {
        command: header[1],
        target: read_address(reader).await?,
    })
}

async fn read_reply<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<SocketAddr> {
    let header = read_exact_array::<3, _>(reader).await?;
    if header[0] != SOCKS_VERSION || header[1] != 0 || header[2] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("upstream SOCKS5 reply code {}", header[1]),
        ));
    }
    read_address(reader).await?.resolve().await
}

pub async fn write_reply<W: AsyncWrite + Unpin>(
    writer: &mut W,
    reply: u8,
    address: SocketAddr,
) -> io::Result<()> {
    writer.write_all(&[SOCKS_VERSION, reply, 0]).await?;
    write_address(writer, &SocksAddress::Ip(address)).await
}

async fn read_address<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<SocksAddress> {
    match reader.read_u8().await? {
        ATYP_IPV4 => {
            let octets = read_exact_array::<4, _>(reader).await?;
            let port = reader.read_u16().await?;
            Ok(SocksAddress::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(octets)),
                port,
            )))
        }
        ATYP_IPV6 => {
            let octets = read_exact_array::<16, _>(reader).await?;
            let port = reader.read_u16().await?;
            Ok(SocksAddress::Ip(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(octets)),
                port,
            )))
        }
        ATYP_DOMAIN => {
            let length = reader.read_u8().await? as usize;
            let mut value = vec![0_u8; length];
            reader.read_exact(&mut value).await?;
            let host = String::from_utf8(value).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidData, format!("domain UTF-8: {error}"))
            })?;
            let port = reader.read_u16().await?;
            Ok(SocksAddress::Domain(host, port))
        }
        atyp => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported SOCKS5 address type {atyp}"),
        )),
    }
}

async fn write_address<W: AsyncWrite + Unpin>(
    writer: &mut W,
    address: &SocksAddress,
) -> io::Result<()> {
    match address {
        SocksAddress::Ip(SocketAddr::V4(address)) => {
            writer.write_u8(ATYP_IPV4).await?;
            writer.write_all(&address.ip().octets()).await?;
            writer.write_u16(address.port()).await
        }
        SocksAddress::Ip(SocketAddr::V6(address)) => {
            writer.write_u8(ATYP_IPV6).await?;
            writer.write_all(&address.ip().octets()).await?;
            writer.write_u16(address.port()).await
        }
        SocksAddress::Domain(host, port) => {
            if host.len() > u8::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SOCKS5 domain exceeds 255 bytes",
                ));
            }
            writer.write_u8(ATYP_DOMAIN).await?;
            writer.write_u8(host.len() as u8).await?;
            writer.write_all(host.as_bytes()).await?;
            writer.write_u16(*port).await
        }
    }
}

fn validate_udp_packet(packet: &[u8]) -> io::Result<()> {
    decode_udp_packet(packet).map(|_| ())
}

fn decode_udp_packet(packet: &[u8]) -> io::Result<(SocksAddress, &[u8])> {
    if packet.len() < 4 || packet[0..2] != [0, 0] || packet[2] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid or fragmented SOCKS5 UDP packet",
        ));
    }
    let atyp = packet[3];
    let (target, payload_offset) = match atyp {
        ATYP_IPV4 if packet.len() >= 10 => {
            let ip = Ipv4Addr::new(packet[4], packet[5], packet[6], packet[7]);
            let port = u16::from_be_bytes([packet[8], packet[9]]);
            (SocksAddress::Ip(SocketAddr::new(IpAddr::V4(ip), port)), 10)
        }
        ATYP_IPV6 if packet.len() >= 22 => {
            let mut octets = [0_u8; 16];
            octets.copy_from_slice(&packet[4..20]);
            let port = u16::from_be_bytes([packet[20], packet[21]]);
            (
                SocksAddress::Ip(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port)),
                22,
            )
        }
        ATYP_DOMAIN if packet.len() >= 5 => {
            let length = packet[4] as usize;
            let end = 5 + length;
            if packet.len() < end + 2 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated SOCKS5 UDP domain",
                ));
            }
            let host = std::str::from_utf8(&packet[5..end])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let port = u16::from_be_bytes([packet[end], packet[end + 1]]);
            (SocksAddress::Domain(host.to_owned(), port), end + 2)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid SOCKS5 UDP address",
            ))
        }
    };
    Ok((target, &packet[payload_offset..]))
}

#[cfg(test)]
fn encode_udp_packet(target: &SocksAddress, payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut packet = vec![0, 0, 0];
    match target {
        SocksAddress::Ip(SocketAddr::V4(address)) => {
            packet.push(ATYP_IPV4);
            packet.extend_from_slice(&address.ip().octets());
            packet.extend_from_slice(&address.port().to_be_bytes());
        }
        SocksAddress::Ip(SocketAddr::V6(address)) => {
            packet.push(ATYP_IPV6);
            packet.extend_from_slice(&address.ip().octets());
            packet.extend_from_slice(&address.port().to_be_bytes());
        }
        SocksAddress::Domain(host, port) => {
            if host.len() > u8::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SOCKS5 UDP domain exceeds 255 bytes",
                ));
            }
            packet.push(ATYP_DOMAIN);
            packet.push(host.len() as u8);
            packet.extend_from_slice(host.as_bytes());
            packet.extend_from_slice(&port.to_be_bytes());
        }
    }
    packet.extend_from_slice(payload);
    Ok(packet)
}

async fn read_exact_array<const N: usize, R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<[u8; N]> {
    let mut value = [0_u8; N];
    reader.read_exact(&mut value).await?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use tokio::io::AsyncReadExt;

    use crate::pipe::upstream::Upstream;

    #[tokio::test]
    async fn parses_connect_and_udp_associate_requests() {
        for (command, expected) in [
            (CMD_CONNECT, SocksAddress::Domain("example.com".into(), 443)),
            (
                CMD_UDP_ASSOCIATE,
                SocksAddress::Ip("127.0.0.1:9000".parse().unwrap()),
            ),
        ] {
            let (mut writer, mut reader) = tokio::io::duplex(128);
            tokio::spawn(async move {
                writer
                    .write_all(&[SOCKS_VERSION, command, 0])
                    .await
                    .unwrap();
                write_address(&mut writer, &expected).await.unwrap();
            });
            let request = read_request(&mut reader).await.unwrap();
            assert_eq!(request.command, command);
        }
    }

    #[tokio::test]
    async fn connect_forwards_against_mock_upstream() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let mut bytes = [0_u8; 4];
            stream.read_exact(&mut bytes).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
        });

        let mock = mock_upstream().await;
        let map = UpstreamMap::default();
        map.swap(
            0,
            Upstream {
                host: "127.0.0.1".into(),
                port: mock.port(),
                username: None,
                password: None,
                expected_exit_ip: "203.0.113.10".parse().unwrap(),
            },
        );
        let relay = RelayServer::bind(0, map, "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let relay_addr = relay.local_addr();
        tokio::spawn(relay.run());

        let mut client = TcpStream::connect(relay_addr).await.unwrap();
        client.write_all(&[SOCKS_VERSION, 1, 0]).await.unwrap();
        assert_eq!(read_exact_array::<2, _>(&mut client).await.unwrap(), [5, 0]);
        upstream_request(&mut client, CMD_CONNECT, &SocksAddress::Ip(echo_addr))
            .await
            .unwrap();
        let _ = read_reply(&mut client).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0_u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
    }

    #[tokio::test]
    async fn udp_associate_forwards_against_mock_upstream() {
        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let mut bytes = [0_u8; 32];
            let (length, peer) = echo.recv_from(&mut bytes).await.unwrap();
            echo.send_to(&bytes[..length], peer).await.unwrap();
        });

        let mock = mock_udp_upstream().await;
        let map = UpstreamMap::default();
        map.swap(
            0,
            Upstream {
                host: "127.0.0.1".into(),
                port: mock.port(),
                username: None,
                password: None,
                expected_exit_ip: "203.0.113.10".parse().unwrap(),
            },
        );
        let relay = RelayServer::bind(0, map, "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let relay_addr = relay.local_addr();
        tokio::spawn(relay.run());

        let mut control = TcpStream::connect(relay_addr).await.unwrap();
        control.write_all(&[SOCKS_VERSION, 1, 0]).await.unwrap();
        assert_eq!(
            read_exact_array::<2, _>(&mut control).await.unwrap(),
            [5, 0]
        );
        upstream_request(
            &mut control,
            CMD_UDP_ASSOCIATE,
            &SocksAddress::Ip("0.0.0.0:0".parse().unwrap()),
        )
        .await
        .unwrap();
        let local_udp_relay = read_reply(&mut control).await.unwrap();
        let client_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let packet = encode_udp_packet(&SocksAddress::Ip(echo_addr), b"udp-ping").unwrap();
        client_udp.send_to(&packet, local_udp_relay).await.unwrap();
        let mut response = [0_u8; 128];
        let (length, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_udp.recv_from(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        let (_, payload) = decode_udp_packet(&response[..length]).unwrap();
        assert_eq!(payload, b"udp-ping");
    }

    async fn mock_upstream() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(
                read_exact_array::<3, _>(&mut stream).await.unwrap(),
                [5, 1, 0]
            );
            stream.write_all(&[5, 0]).await.unwrap();
            let request = read_request(&mut stream).await.unwrap();
            assert_eq!(request.command, CMD_CONNECT);
            let target = request.target.resolve().await.unwrap();
            let mut destination = TcpStream::connect(target).await.unwrap();
            write_reply(&mut stream, 0, "127.0.0.1:0".parse().unwrap())
                .await
                .unwrap();
            tokio::io::copy_bidirectional(&mut stream, &mut destination)
                .await
                .unwrap();
        });
        address
    }

    async fn mock_udp_upstream() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut control, _) = listener.accept().await.unwrap();
            assert_eq!(
                read_exact_array::<3, _>(&mut control).await.unwrap(),
                [5, 1, 0]
            );
            control.write_all(&[5, 0]).await.unwrap();
            let request = read_request(&mut control).await.unwrap();
            assert_eq!(request.command, CMD_UDP_ASSOCIATE);
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            write_reply(&mut control, 0, udp.local_addr().unwrap())
                .await
                .unwrap();

            let mut packet = [0_u8; 256];
            let (length, relay_peer) = udp.recv_from(&mut packet).await.unwrap();
            let (target, payload) = decode_udp_packet(&packet[..length]).unwrap();
            let target = target.resolve().await.unwrap();
            udp.send_to(payload, target).await.unwrap();
            let (echoed_length, _) = udp.recv_from(&mut packet).await.unwrap();
            let response =
                encode_udp_packet(&SocksAddress::Ip(target), &packet[..echoed_length]).unwrap();
            udp.send_to(&response, relay_peer).await.unwrap();
        });
        address
    }

    #[test]
    fn udp_header_rejects_fragmentation() {
        assert!(validate_udp_packet(&[0, 0, 1, ATYP_IPV4]).is_err());
        let valid =
            encode_udp_packet(&SocksAddress::Ip("127.0.0.1:53".parse().unwrap()), b"dns").unwrap();
        assert!(validate_udp_packet(&valid).is_ok());
    }

    #[test]
    fn address_equality_is_stable_for_maps() {
        let mut values = HashMap::new();
        values.insert("key", SocksAddress::Domain("example.com".into(), 443));
        assert_eq!(
            values["key"],
            SocksAddress::Domain("example.com".into(), 443)
        );
    }
}
