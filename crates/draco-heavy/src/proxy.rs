use std::io::{Error, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use percent_encoding::percent_decode_str;
use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_socks::tcp::Socks5Stream;
use url::Url;

#[derive(Debug)]
pub(crate) struct PreparedBrowserProxy {
    server: String,
    credentials: Option<(String, String)>,
    relay: Option<JoinHandle<()>>,
}

impl PreparedBrowserProxy {
    pub(crate) async fn prepare(raw: &str) -> Result<Self, String> {
        let parsed = Url::parse(raw).map_err(|error| format!("invalid proxy URL: {error}"))?;
        if !matches!(parsed.path(), "" | "/")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err("proxy URL must not contain a path, query, or fragment".into());
        }

        let host = parsed
            .host_str()
            .ok_or_else(|| "proxy URL is missing a hostname".to_string())?;
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| "proxy URL is missing a port".to_string())?;
        let username = decode_component(parsed.username())?;
        let password = decode_component(parsed.password().unwrap_or(""))?;
        let authenticated = !username.is_empty() || !password.is_empty();

        match parsed.scheme() {
            "http" | "https" => Ok(Self {
                server: format!("{}://{}", parsed.scheme(), authority(host, port)),
                credentials: authenticated.then_some((username, password)),
                relay: None,
            }),
            "socks5" | "socks5h" if authenticated => {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                    .await
                    .map_err(|error| format!("could not bind browser proxy relay: {error}"))?;
                let local = listener
                    .local_addr()
                    .map_err(|error| format!("could not inspect browser proxy relay: {error}"))?;
                let upstream = (host.to_owned(), port, username, password);
                let relay = tokio::spawn(async move {
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            break;
                        };
                        let upstream = upstream.clone();
                        tokio::spawn(async move {
                            if let Err(error) = relay_connection(stream, upstream).await {
                                eprintln!("draco browser proxy relay failed: {error}");
                            }
                        });
                    }
                });
                Ok(Self {
                    server: format!("socks5://{local}"),
                    credentials: None,
                    relay: Some(relay),
                })
            }
            "socks5" | "socks5h" => Ok(Self {
                server: format!("socks5://{}", authority(host, port)),
                credentials: None,
                relay: None,
            }),
            scheme => Err(format!(
                "unsupported browser proxy scheme {scheme:?}; expected http, https, socks5, or socks5h"
            )),
        }
    }

    pub(crate) fn server(&self) -> &str {
        &self.server
    }

    pub(crate) fn credentials(&self) -> Option<(&str, &str)> {
        self.credentials
            .as_ref()
            .map(|(username, password)| (username.as_str(), password.as_str()))
    }
}

impl Drop for PreparedBrowserProxy {
    fn drop(&mut self) {
        if let Some(relay) = self.relay.take() {
            relay.abort();
        }
    }
}

fn decode_component(value: &str) -> Result<String, String> {
    percent_decode_str(value)
        .decode_utf8()
        .map(String::from)
        .map_err(|_| "proxy credentials are not valid UTF-8".into())
}

fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

async fn relay_connection(
    mut downstream: TcpStream,
    upstream: (String, u16, String, String),
) -> std::io::Result<()> {
    accept_no_auth(&mut downstream).await?;
    let target = read_connect_target(&mut downstream).await?;
    let (host, port, username, password) = upstream;
    let connected =
        Socks5Stream::connect_with_password((host.as_str(), port), target, &username, &password)
            .await;
    let mut upstream = match connected {
        Ok(stream) => stream,
        Err(error) => {
            let _ = downstream.write_all(&[5, 1, 0, 1, 0, 0, 0, 0, 0, 0]).await;
            return Err(Error::other(format!(
                "upstream SOCKS connection failed: {error}"
            )));
        }
    };

    downstream
        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
        .await?;
    copy_bidirectional(&mut downstream, &mut upstream).await?;
    Ok(())
}

async fn accept_no_auth(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut greeting = [0_u8; 2];
    stream.read_exact(&mut greeting).await?;
    if greeting[0] != 5 || greeting[1] == 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "invalid SOCKS5 greeting",
        ));
    }
    let mut methods = vec![0_u8; usize::from(greeting[1])];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        stream.write_all(&[5, 0xff]).await?;
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "browser did not offer unauthenticated SOCKS5",
        ));
    }
    stream.write_all(&[5, 0]).await
}

async fn read_connect_target(stream: &mut TcpStream) -> std::io::Result<(String, u16)> {
    let mut request = [0_u8; 4];
    stream.read_exact(&mut request).await?;
    if request[0] != 5 || request[1] != 1 || request[2] != 0 {
        stream.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Err(Error::new(
            ErrorKind::Unsupported,
            "browser proxy relay only supports SOCKS5 CONNECT",
        ));
    }

    let host = match request[3] {
        1 => {
            let mut octets = [0_u8; 4];
            stream.read_exact(&mut octets).await?;
            IpAddr::V4(Ipv4Addr::from(octets)).to_string()
        }
        3 => {
            let length = stream.read_u8().await?;
            let mut domain = vec![0_u8; usize::from(length)];
            stream.read_exact(&mut domain).await?;
            String::from_utf8(domain)
                .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid SOCKS5 domain"))?
        }
        4 => {
            let mut octets = [0_u8; 16];
            stream.read_exact(&mut octets).await?;
            IpAddr::V6(Ipv6Addr::from(octets)).to_string()
        }
        _ => {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "unsupported SOCKS5 address type",
            ))
        }
    };
    let port = stream.read_u16().await?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_credentials_are_kept_out_of_chrome_argument() {
        let proxy = PreparedBrowserProxy::prepare("http://user:p%40ss@proxy.example:8080")
            .await
            .unwrap();
        assert_eq!(proxy.server(), "http://proxy.example:8080");
        assert_eq!(proxy.credentials(), Some(("user", "p@ss")));
    }

    #[tokio::test]
    async fn authenticated_socks_uses_a_loopback_relay() {
        let proxy = PreparedBrowserProxy::prepare("socks5h://user:pass@proxy.example:1080")
            .await
            .unwrap();
        assert!(proxy.server().starts_with("socks5://127.0.0.1:"));
        assert_eq!(proxy.credentials(), None);
    }

    #[tokio::test]
    async fn authenticated_socks_relay_forwards_browser_connect() {
        let upstream_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let upstream_address = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let (mut stream, _) = upstream_listener.accept().await.unwrap();

            let mut greeting = [0_u8; 4];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 2, 0, 2]);
            stream.write_all(&[5, 2]).await.unwrap();

            let mut auth_header = [0_u8; 2];
            stream.read_exact(&mut auth_header).await.unwrap();
            assert_eq!(auth_header, [1, 4]);
            let mut username = [0_u8; 4];
            stream.read_exact(&mut username).await.unwrap();
            assert_eq!(&username, b"user");
            assert_eq!(stream.read_u8().await.unwrap(), 4);
            let mut password = [0_u8; 4];
            stream.read_exact(&mut password).await.unwrap();
            assert_eq!(&password, b"pass");
            stream.write_all(&[1, 0]).await.unwrap();

            assert_eq!(
                read_connect_target(&mut stream).await.unwrap(),
                ("example.com".to_string(), 443),
            );
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 1])
                .await
                .unwrap();
            let mut ping = [0_u8; 4];
            stream.read_exact(&mut ping).await.unwrap();
            assert_eq!(&ping, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });

        let proxy =
            PreparedBrowserProxy::prepare(&format!("socks5h://user:pass@{upstream_address}"))
                .await
                .unwrap();
        let local = Url::parse(proxy.server()).unwrap();
        let mut browser = TcpStream::connect((local.host_str().unwrap(), local.port().unwrap()))
            .await
            .unwrap();
        browser.write_all(&[5, 1, 0]).await.unwrap();
        let mut method = [0_u8; 2];
        browser.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [5, 0]);

        browser
            .write_all(&[
                5, 1, 0, 3, 11, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.', b'c', b'o', b'm',
                1, 187,
            ])
            .await
            .unwrap();
        let mut connected = [0_u8; 10];
        browser.read_exact(&mut connected).await.unwrap();
        assert_eq!(connected[1], 0);
        browser.write_all(b"ping").await.unwrap();
        let mut pong = [0_u8; 4];
        browser.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");
        upstream_task.await.unwrap();
    }

    #[tokio::test]
    async fn plain_socks_is_normalized_for_chromium() {
        let proxy = PreparedBrowserProxy::prepare("socks5h://proxy.example:1080")
            .await
            .unwrap();
        assert_eq!(proxy.server(), "socks5://proxy.example:1080");
    }
}
