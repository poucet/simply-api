//! One HTTP/1.1 POST, over tokio and rustls: all a webhook needs, so no HTTP
//! client crate is compiled in.
//!
//! - `https://` anywhere; `http://` only to a loopback host (a local receiver,
//!   the tests): a payload never crosses a network in clear.
//! - `Connection: close`, no redirects followed, no proxy, no keep-alive.
//! - Only the status line is read: the response's body is never looked at.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use http::Uri;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// The most of a response's head read before giving up on it.
const MAX_HEAD: usize = 16 * 1024;

/// A target URL, checked once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    tls: bool,
    /// The host as it connects (no brackets around an IPv6 address).
    host: String,
    port: u16,
    /// `Host:`'s value.
    authority: String,
    path: String,
}

impl Endpoint {
    pub(crate) fn parse(url: &str) -> Result<Endpoint, String> {
        let uri: Uri = url.parse().map_err(|e| format!("not a URL: {e}"))?;
        let tls = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err("the URL must start with https://".into()),
        };
        let raw_host = uri.host().ok_or("the URL has no host")?;
        let host = raw_host.trim_start_matches('[').trim_end_matches(']').to_owned();
        if !tls && !is_loopback(&host) {
            return Err("http:// is only for this machine (localhost): use https://".into());
        }
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
        let authority = match uri.port_u16() {
            Some(p) => format!("{raw_host}:{p}"),
            None => raw_host.to_owned(),
        };
        let path =
            uri.path_and_query().map(|p| p.as_str()).filter(|p| !p.is_empty()).unwrap_or("/");
        Ok(Endpoint { tls, host, port, authority, path: path.to_owned() })
    }

    /// The host alone, for logs: a URL's path may carry a token.
    pub(crate) fn host(&self) -> &str {
        &self.host
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// What came back: the status, and `Retry-After` in seconds when given so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Answer {
    pub code: u16,
    pub retry_after: Option<u64>,
}

/// POST `body` to `to` with `headers`, within `timeout` from connect to the
/// answer's head: the answer, or why there is none.
pub(crate) async fn post(
    to: &Endpoint,
    headers: &[(&str, String)],
    body: &[u8],
    timeout: Duration,
) -> Result<Answer, String> {
    match tokio::time::timeout(timeout, exchange(to, headers, body)).await {
        Ok(r) => r,
        Err(_) => Err(format!("no answer within {} s", timeout.as_secs_f32())),
    }
}

async fn exchange(
    to: &Endpoint,
    headers: &[(&str, String)],
    body: &[u8],
) -> Result<Answer, String> {
    let tcp = TcpStream::connect((to.host.as_str(), to.port))
        .await
        .map_err(|e| format!("could not connect: {e}"))?;
    let _ = tcp.set_nodelay(true);
    if to.tls {
        let name = rustls::pki_types::ServerName::try_from(to.host.clone())
            .map_err(|e| format!("bad host name: {e}"))?;
        let tls = tokio_rustls::TlsConnector::from(tls_config())
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS failed: {e}"))?;
        send(tls, to, headers, body).await
    } else {
        send(tcp, to, headers, body).await
    }
}

async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    to: &Endpoint,
    headers: &[(&str, String)],
    body: &[u8],
) -> Result<Answer, String> {
    let mut head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        to.path,
        to.authority,
        body.len()
    );
    for (k, v) in headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let mut req = head.into_bytes();
    req.extend_from_slice(body);
    s.write_all(&req).await.map_err(|e| format!("could not send: {e}"))?;
    s.flush().await.map_err(|e| format!("could not send: {e}"))?;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = s.read(&mut chunk).await.map_err(|e| format!("no answer: {e}"))?;
        if n == 0 {
            return Err("the connection closed before an answer".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut hs = [httparse::EMPTY_HEADER; 64];
        let mut res = httparse::Response::new(&mut hs);
        match res.parse(&buf) {
            Ok(httparse::Status::Complete(_)) => {
                let code = res.code.ok_or_else(|| "an answer with no status".to_owned())?;
                let retry_after = res
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("retry-after"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .and_then(|v| v.trim().parse().ok());
                return Ok(Answer { code, retry_after });
            }
            Ok(httparse::Status::Partial) if buf.len() < MAX_HEAD => continue,
            Ok(httparse::Status::Partial) => return Err("an answer too long to read".into()),
            Err(e) => return Err(format!("not an HTTP answer: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_checked_once() {
        let e = Endpoint::parse("https://hooks.example.com/in/abc?x=1").unwrap();
        assert_eq!(
            (e.tls, e.host.as_str(), e.port, e.authority.as_str(), e.path.as_str()),
            (true, "hooks.example.com", 443, "hooks.example.com", "/in/abc?x=1")
        );
        let e = Endpoint::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!(
            (e.tls, e.port, e.authority.as_str(), e.path.as_str()),
            (false, 8080, "127.0.0.1:8080", "/")
        );
        let e = Endpoint::parse("http://[::1]:9/x").unwrap();
        assert_eq!((e.host.as_str(), e.authority.as_str()), ("::1", "[::1]:9"));
        assert!(Endpoint::parse("http://localhost/x").is_ok());
        assert!(
            Endpoint::parse("http://hooks.example.com/x").is_err(),
            "clear text off this machine"
        );
        assert!(Endpoint::parse("ftp://hooks.example.com/x").is_err());
        assert!(Endpoint::parse("hooks.example.com/x").is_err());
        assert!(Endpoint::parse("").is_err());
    }
}
