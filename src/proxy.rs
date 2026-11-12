//! Caller-supplied egress proxy.
//!
//! Browsers take no proxy credentials on the command line, so a proxy URL
//! with credentials is fronted by a loopback relay: the browser connects to
//! the relay without credentials and the relay authenticates upstream. HTTP
//! proxies get a `Proxy-Authorization: Basic` header on each request head;
//! SOCKS5 proxies get the username/password sub-negotiation (RFC 1929). A URL
//! without credentials is handed to the browser directly.

use std::net::SocketAddr;

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};

const MAX_HEAD: usize = 64 * 1024;

/// Proxy configuration. `Debug` never shows credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct Proxy {
    url: url::Url,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut u = self.url.clone();
        let _ = u.set_username("");
        let _ = u.set_password(None);
        write!(f, "Proxy({u})")
    }
}

impl Proxy {
    /// Parse `http://[user:pass@]host:port` or `socks5://[user:pass@]host:port`.
    pub fn parse(url: &str) -> Result<Self> {
        let url =
            url::Url::parse(url).map_err(|e| Error::InvalidInput(format!("proxy URL: {e}")))?;
        if !matches!(url.scheme(), "http" | "socks5") {
            return Err(Error::InvalidInput(
                "proxy scheme must be http or socks5".into(),
            ));
        }
        if url.host_str().is_none() || url.port_or_known_default().is_none() {
            return Err(Error::InvalidInput(
                "proxy URL needs a host and port".into(),
            ));
        }
        Ok(Self { url })
    }

    fn credentials(&self) -> Option<(String, String)> {
        let user = percent_decode(self.url.username());
        let pass = percent_decode(self.url.password().unwrap_or(""));
        (!user.is_empty() || !pass.is_empty()).then_some((user, pass))
    }

    fn upstream(&self) -> String {
        format!(
            "{}:{}",
            self.url.host_str().unwrap_or_default(),
            self.url.port_or_known_default().unwrap_or(0)
        )
    }

    /// Start whatever the browser needs: nothing for an unauthenticated
    /// proxy, a loopback relay otherwise.
    pub async fn start(&self) -> Result<ProxyEndpoint> {
        let Some((user, pass)) = self.credentials() else {
            let mut u = self.url.clone();
            u.set_path("");
            return Ok(ProxyEndpoint {
                browser_url: u.as_str().trim_end_matches('/').to_string(),
                relay: None,
            });
        };
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let local: SocketAddr = listener.local_addr()?;
        let upstream = self.upstream();
        let socks = self.url.scheme() == "socks5";
        let header = format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
        );
        let relay = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let (upstream, header, user, pass) =
                    (upstream.clone(), header.clone(), user.clone(), pass.clone());
                tokio::spawn(async move {
                    let r = if socks {
                        relay_socks(client, &upstream, &user, &pass).await
                    } else {
                        relay_http(client, &upstream, &header).await
                    };
                    if let Err(e) = r {
                        tracing::debug!("proxy relay connection ended: {e}");
                    }
                });
            }
        });
        let scheme = if socks { "socks5" } else { "http" };
        Ok(ProxyEndpoint {
            browser_url: format!("{scheme}://{local}"),
            relay: Some(relay),
        })
    }
}

/// What the browser is pointed at. Dropping it stops the relay.
#[derive(Debug)]
pub struct ProxyEndpoint {
    /// Credential-free proxy URL for the browser.
    pub browser_url: String,
    relay: Option<JoinHandle<()>>,
}

impl Drop for ProxyEndpoint {
    fn drop(&mut self) {
        if let Some(r) = self.relay.take() {
            r.abort();
        }
    }
}

/// Forward one HTTP proxy connection: authorize the first request head, then
/// splice. Plain (non-CONNECT) requests are sent with `Connection: close` so
/// every request head passes through here on its own connection.
async fn relay_http(mut client: TcpStream, upstream: &str, header: &str) -> Result<()> {
    let Some(head) = read_head(&mut client).await? else {
        return Ok(());
    };
    let line_end = head
        .windows(2)
        .position(|w| w == b"\r\n")
        .ok_or_else(|| Error::Protocol("bad request head".into()))?
        + 2;
    let mut out = head[..line_end].to_vec();
    out.extend_from_slice(header.as_bytes());
    let rest: Vec<u8> = strip_header(&head[line_end..], b"proxy-authorization").collect();
    if head.starts_with(b"CONNECT ") {
        out.extend(rest);
    } else {
        let rest: Vec<u8> = strip_header(&rest, b"connection").collect();
        let rest: Vec<u8> = strip_header(&rest, b"proxy-connection").collect();
        out.extend_from_slice(b"Connection: close\r\n");
        out.extend(rest);
    }
    let mut up = TcpStream::connect(upstream).await?;
    up.write_all(&out).await?;
    tokio::io::copy_bidirectional(&mut client, &mut up).await?;
    Ok(())
}

fn strip_header<'a>(rest: &'a [u8], name: &'a [u8]) -> impl Iterator<Item = u8> + 'a {
    rest.split_inclusive(|b| *b == b'\n')
        .filter(move |line| {
            !(line.len() > name.len()
                && line[..name.len()].eq_ignore_ascii_case(name)
                && line[name.len()] == b':')
        })
        .flatten()
        .copied()
}

async fn read_head(s: &mut TcpStream) -> Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte).await? == 0 {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Err(Error::Protocol("truncated request head".into()))
            };
        }
        buf.push(byte[0]);
        if buf.len() > MAX_HEAD {
            return Err(Error::Protocol("request head too large".into()));
        }
    }
    Ok(Some(buf))
}

/// Accept an unauthenticated SOCKS5 client, authenticate upstream, splice.
async fn relay_socks(mut client: TcpStream, upstream: &str, user: &str, pass: &str) -> Result<()> {
    // Client greeting: VER, NMETHODS, METHODS; answer "no authentication".
    let mut hdr = [0u8; 2];
    client.read_exact(&mut hdr).await?;
    if hdr[0] != 5 {
        return Err(Error::Protocol("not SOCKS5".into()));
    }
    let mut methods = vec![0u8; hdr[1] as usize];
    client.read_exact(&mut methods).await?;
    client.write_all(&[5, 0]).await?;

    let mut up = TcpStream::connect(upstream).await?;
    up.write_all(&[5, 1, 2]).await?;
    let mut sel = [0u8; 2];
    up.read_exact(&mut sel).await?;
    if sel != [5, 2] {
        return Err(Error::Refused(
            "upstream SOCKS5 proxy refused username/password".into(),
        ));
    }
    if user.len() > 255 || pass.len() > 255 {
        return Err(Error::InvalidInput(
            "SOCKS5 credentials longer than 255 bytes".into(),
        ));
    }
    let mut auth = vec![1, user.len() as u8];
    auth.extend_from_slice(user.as_bytes());
    auth.push(pass.len() as u8);
    auth.extend_from_slice(pass.as_bytes());
    up.write_all(&auth).await?;
    let mut ok = [0u8; 2];
    up.read_exact(&mut ok).await?;
    if ok[1] != 0 {
        return Err(Error::Refused(
            "upstream SOCKS5 proxy rejected the credentials".into(),
        ));
    }
    tokio::io::copy_bidirectional(&mut client, &mut up).await?;
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_parse_never_expose_credentials() {
        let p = Proxy::parse("http://user:p%40ss@proxy.example.com:3128").unwrap();
        assert!(!format!("{p:?}").contains("p%40ss"));
        assert_eq!(p.credentials(), Some(("user".into(), "p@ss".into())));
        assert!(Proxy::parse("ftp://proxy.example.com:21").is_err());
        assert!(Proxy::parse("not a url").is_err());
    }

    #[tokio::test]
    async fn unauthenticated_proxy_is_passed_through() {
        let e = Proxy::parse("socks5://127.0.0.1:1080")
            .unwrap()
            .start()
            .await
            .unwrap();
        assert_eq!(e.browser_url, "socks5://127.0.0.1:1080");
    }

    #[tokio::test]
    async fn http_relay_adds_basic_auth_and_drops_client_supplied_auth() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = upstream.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let head = read_head(&mut s).await.unwrap().unwrap();
            s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(head).unwrap()
        });
        let p = Proxy::parse(&format!("http://u:pw@{addr}")).unwrap();
        let ep = p.start().await.unwrap();
        let relay = ep.browser_url.trim_start_matches("http://").to_string();
        let mut c = TcpStream::connect(relay).await.unwrap();
        c.write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nProxy-Authorization: Basic Zm9vOmJhcg==\r\n\r\n").await.unwrap();
        let mut reply = [0u8; 12];
        c.read_exact(&mut reply).await.unwrap();
        let head = seen.await.unwrap();
        assert!(head.starts_with(
            "CONNECT example.com:443 HTTP/1.1\r\nProxy-Authorization: Basic dTpwdw==\r\n"
        ));
        assert!(!head.contains("Zm9vOmJhcg=="));
        assert_eq!(&reply, b"HTTP/1.1 200");
    }

    #[tokio::test]
    async fn socks_relay_authenticates_upstream() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = upstream.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let mut g = [0u8; 3];
            s.read_exact(&mut g).await.unwrap();
            assert_eq!(g, [5, 1, 2]);
            s.write_all(&[5, 2]).await.unwrap();
            let mut a = [0u8; 7];
            s.read_exact(&mut a).await.unwrap();
            assert_eq!(&a, &[1, 1, b'u', 3, b'p', b'w', b'd']);
            s.write_all(&[1, 0]).await.unwrap();
            let mut payload = [0u8; 4];
            s.read_exact(&mut payload).await.unwrap();
            payload
        });
        let ep = Proxy::parse(&format!("socks5://u:pwd@{addr}"))
            .unwrap()
            .start()
            .await
            .unwrap();
        let mut c = TcpStream::connect(ep.browser_url.trim_start_matches("socks5://"))
            .await
            .unwrap();
        c.write_all(&[5, 1, 0]).await.unwrap();
        let mut sel = [0u8; 2];
        c.read_exact(&mut sel).await.unwrap();
        assert_eq!(sel, [5, 0]);
        c.write_all(b"ping").await.unwrap();
        assert_eq!(&server.await.unwrap(), b"ping");
    }
}
