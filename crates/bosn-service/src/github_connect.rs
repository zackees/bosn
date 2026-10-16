//! The CONNECT front of `bosn ci run --github-proxy` (#650), Linux only.
//!
//! Jobs reach it as their `HTTPS_PROXY`. A `CONNECT api.github.com:443` is
//! intercepted: the front answers TLS itself with a leaf certificate signed
//! by a CA minted for this run alone (trusted only inside the run's job
//! containers), reads each HTTP request and relays it, as plain HTTP on
//! loopback, to the run's [`crate::github_proxy::GithubApiProxy`], which
//! checks the fake token and the allowlist and swaps in the real credential.
//! Any other `CONNECT host:port` is tunnelled byte for byte, so tools that
//! honour `HTTPS_PROXY` keep reaching everything else directly.
//!
//! The CA's private key is dropped as soon as the leaf is signed: nothing can
//! mint another certificate under it, and it never leaves daemon memory.

use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{SslAcceptor, SslConnector, SslMethod, SslVerifyMode};
use openssl::x509::extension::{
    AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName,
    SubjectKeyIdentifier,
};
use openssl::x509::{X509, X509NameBuilder};

use crate::github_http1::{self as http1, BodyLen, Head};

/// The only host whose TLS the front terminates.
pub const INTERCEPTED_HOST: &str = "api.github.com";
const MAX_REQUEST_BODY: usize = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(180);

/// A run's interception identity: the CA certificate jobs trust (public)
/// and the TLS acceptor holding the leaf key (daemon memory only).
pub struct InterceptCa {
    ca_pem: String,
    acceptor: SslAcceptor,
}

fn ssl_err(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("tls: {error}"))
}

fn name(common: &str) -> Result<openssl::x509::X509Name, openssl::error::ErrorStack> {
    let mut builder = X509NameBuilder::new()?;
    builder.append_entry_by_nid(Nid::COMMONNAME, common)?;
    builder.append_entry_by_nid(Nid::ORGANIZATIONNAME, "bosn ci run --github-proxy")?;
    Ok(builder.build())
}

fn p256() -> Result<PKey<Private>, openssl::error::ErrorStack> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    PKey::from_ec_key(EcKey::generate(&group)?)
}

fn serial() -> Result<openssl::asn1::Asn1Integer, openssl::error::ErrorStack> {
    let mut bn = BigNum::new()?;
    bn.rand(127, MsbOption::MAYBE_ZERO, false)?;
    bn.to_asn1_integer()
}

impl InterceptCa {
    /// Mint a fresh CA and a leaf for `host`, valid from an hour ago (clock
    /// skew between host and containers) for two days.
    pub fn mint(host: &str) -> io::Result<Self> {
        Self::mint_inner(host).map_err(ssl_err)
    }

    fn mint_inner(host: &str) -> Result<Self, openssl::error::ErrorStack> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let not_before = Asn1Time::from_unix(now - 3600)?;
        let not_after = Asn1Time::from_unix(now + 2 * 86_400)?;
        let ca_key = p256()?;
        let ca_name = name("bosn per-run GitHub proxy CA")?;
        let mut ca = X509::builder()?;
        ca.set_version(2)?;
        let ca_serial = serial()?;
        ca.set_serial_number(&ca_serial)?;
        ca.set_subject_name(&ca_name)?;
        ca.set_issuer_name(&ca_name)?;
        ca.set_pubkey(&ca_key)?;
        ca.set_not_before(&not_before)?;
        ca.set_not_after(&not_after)?;
        ca.append_extension(BasicConstraints::new().critical().ca().pathlen(0).build()?)?;
        ca.append_extension(
            KeyUsage::new()
                .critical()
                .key_cert_sign()
                .crl_sign()
                .build()?,
        )?;
        let ski = SubjectKeyIdentifier::new().build(&ca.x509v3_context(None, None))?;
        ca.append_extension(ski)?;
        ca.sign(&ca_key, MessageDigest::sha256())?;
        let ca = ca.build();

        let leaf_key = p256()?;
        let mut leaf = X509::builder()?;
        leaf.set_version(2)?;
        let leaf_serial = serial()?;
        leaf.set_serial_number(&leaf_serial)?;
        let leaf_name = name(host)?;
        leaf.set_subject_name(&leaf_name)?;
        leaf.set_issuer_name(ca.subject_name())?;
        leaf.set_pubkey(&leaf_key)?;
        leaf.set_not_before(&not_before)?;
        leaf.set_not_after(&not_after)?;
        leaf.append_extension(BasicConstraints::new().critical().build()?)?;
        leaf.append_extension(
            KeyUsage::new()
                .critical()
                .digital_signature()
                .key_agreement()
                .build()?,
        )?;
        leaf.append_extension(ExtendedKeyUsage::new().server_auth().build()?)?;
        let san = SubjectAlternativeName::new()
            .dns(host)
            .build(&leaf.x509v3_context(Some(&ca), None))?;
        leaf.append_extension(san)?;
        let aki = AuthorityKeyIdentifier::new()
            .keyid(true)
            .build(&leaf.x509v3_context(Some(&ca), None))?;
        leaf.append_extension(aki)?;
        let ski = SubjectKeyIdentifier::new().build(&leaf.x509v3_context(Some(&ca), None))?;
        leaf.append_extension(ski)?;
        leaf.sign(&ca_key, MessageDigest::sha256())?;
        let leaf = leaf.build();
        drop(ca_key);

        let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
        acceptor.set_private_key(&leaf_key)?;
        acceptor.set_certificate(&leaf)?;
        acceptor.add_extra_chain_cert(ca.clone())?;
        acceptor.check_private_key()?;
        let ca_pem = String::from_utf8(ca.to_pem()?).unwrap_or_default();
        Ok(Self {
            ca_pem,
            acceptor: acceptor.build(),
        })
    }

    /// The CA certificate, PEM. Public: it is what job containers trust.
    #[must_use]
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }
}

/// Where intercepted requests go: the run's nonce-prefixed loopback proxy.
#[derive(Clone)]
pub struct Inner {
    pub addr: SocketAddr,
    /// `/<nonce>`, prefixed to every relayed path.
    pub prefix: String,
}

/// Serve one client connection that arrived on `stream`: its CONNECT, then
/// either the intercepted TLS session or a blind tunnel. Errors only end
/// this connection. `log` receives one line per refused CONNECT.
pub fn serve_client(stream: TcpStream, ca: &InterceptCa, inner: &Inner, log: &dyn Fn(String)) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let Ok(mut reader) = stream.try_clone().map(BufReader::new) else {
        return;
    };
    let mut writer = stream;
    let head = match http1::read_head(&mut reader) {
        Ok(Some(head)) => head,
        _ => return,
    };
    let mut parts = head.start.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "CONNECT" {
        log(format!(
            "[github-api-proxy] {method} (not CONNECT) -> 405 refused; only HTTPS_PROXY tunnels are served"
        ));
        let _ = writer.write_all(
            b"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
        return;
    }
    let Some((host, port)) = target
        .rsplit_once(':')
        .and_then(|(h, p)| Some((h.to_ascii_lowercase(), p.parse::<u16>().ok()?)))
    else {
        let _ = writer.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n");
        return;
    };
    if !reader.buffer().is_empty() {
        // A client never sends tunnel bytes before the 200.
        return;
    }
    let ok = b"HTTP/1.1 200 Connection Established\r\n\r\n";
    if host == INTERCEPTED_HOST && port == 443 {
        if writer.write_all(ok).is_err() {
            return;
        }
        let Ok(tls) = ca.acceptor.accept(writer) else {
            return;
        };
        let _ = serve_intercepted(tls, inner);
    } else {
        let Some(upstream) = (host.as_str(), port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| {
                addrs.find_map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(15)).ok())
            })
        else {
            let _ = writer.write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n");
            return;
        };
        if writer.write_all(ok).is_err() {
            return;
        }
        tunnel(writer, upstream);
    }
}

/// Copy bytes both ways until either side closes.
fn tunnel(client: TcpStream, upstream: TcpStream) {
    let (Ok(mut client_r), Ok(mut upstream_w)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    let up = std::thread::spawn(move || {
        let _ = io::copy(&mut client_r, &mut upstream_w);
        let _ = upstream_w.shutdown(Shutdown::Write);
    });
    let (mut upstream_r, mut client_w) = (upstream, client);
    let _ = io::copy(&mut upstream_r, &mut client_w);
    let _ = client_w.shutdown(Shutdown::Write);
    let _ = up.join();
}

/// Hop-by-hop and framing headers the relay sets itself.
fn relayed(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "content-length"
            | "upgrade"
    )
}

/// Relay every request of one intercepted TLS session to the inner proxy.
fn serve_intercepted<S: Read + Write>(tls: S, inner: &Inner) -> io::Result<()> {
    let mut client = BufReader::new(tls);
    loop {
        let Some(request) = http1::read_head(&mut client)? else {
            return Ok(());
        };
        let mut parts = request.start.split(' ');
        let (method, target) = (
            parts.next().unwrap_or("").to_owned(),
            parts.next().unwrap_or("").to_owned(),
        );
        let target = target
            .strip_prefix("https://api.github.com")
            .map_or(target.clone(), str::to_owned);
        if !target.starts_with('/') {
            client.get_mut().write_all(
                b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )?;
            return Ok(());
        }
        let body = http1::read_body(
            &mut client,
            http1::request_body(&request)?,
            MAX_REQUEST_BODY,
        )?;
        let close = request.lists("connection", "close");
        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), "127.0.0.1".into()),
            ("connection".into(), "close".into()),
            ("content-length".into(), body.len().to_string()),
        ];
        headers.extend(request.headers.iter().filter(|(k, _)| relayed(k)).cloned());
        let mut upstream = TcpStream::connect_timeout(&inner.addr, Duration::from_secs(10))?;
        upstream.set_read_timeout(Some(IO_TIMEOUT))?;
        upstream.write_all(&http1::render_head(
            &format!("{method} {}{target} HTTP/1.1", inner.prefix),
            &headers,
        ))?;
        upstream.write_all(&body)?;
        let mut upstream = BufReader::new(upstream);
        let response: Head = http1::read_head(&mut upstream)?
            .ok_or_else(|| io::Error::other("inner proxy closed without a response"))?;
        let len = http1::response_body(&method, &response)?;
        let until_close = len == BodyLen::UntilClose;
        let mut out: Vec<(String, String)> = response
            .headers
            .iter()
            .filter(|(k, _)| {
                !k.eq_ignore_ascii_case("connection") && !k.eq_ignore_ascii_case("keep-alive")
            })
            .cloned()
            .collect();
        if close || until_close {
            out.push(("connection".into(), "close".into()));
        }
        let writer = client.get_mut();
        writer.write_all(&http1::render_head(&response.start, &out))?;
        http1::copy_body(&mut upstream, writer, len)?;
        writer.flush()?;
        if close || until_close {
            return Ok(());
        }
    }
}

/// Send `DELETE url` with `authorization` and return (status, body). Used
/// only when `--github-writes=on` forwards an allowlisted write; kernal-api's
/// client has no DELETE. `https://` URLs verify against the host's roots.
pub fn send_delete(url: &str, headers: &[(String, String)]) -> io::Result<http1::Reply> {
    let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(io::Error::other("unsupported upstream scheme"));
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| io::Error::other("bad port"))?),
        None => (authority, if tls { 443 } else { 80 }),
    };
    let tcp = (host, port)
        .to_socket_addrs()?
        .find_map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(15)).ok())
        .ok_or_else(|| io::Error::other("upstream unreachable"))?;
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    let mut all = vec![
        ("host".to_owned(), authority.to_owned()),
        ("connection".to_owned(), "close".to_owned()),
        ("content-length".to_owned(), "0".to_owned()),
    ];
    all.extend(headers.iter().cloned());
    let head = http1::render_head(&format!("DELETE {path} HTTP/1.1"), &all);
    if tls {
        let mut connector = SslConnector::builder(SslMethod::tls_client()).map_err(ssl_err)?;
        connector.set_verify(SslVerifyMode::PEER);
        let stream = connector.build().connect(host, tcp).map_err(ssl_err)?;
        exchange(stream, &head)
    } else {
        exchange(tcp, &head)
    }
}

fn exchange<S: Read + Write>(mut stream: S, head: &[u8]) -> io::Result<http1::Reply> {
    stream.write_all(head)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let response = http1::read_head(&mut reader)?
        .ok_or_else(|| io::Error::other("upstream closed without a response"))?;
    let status = response
        .start
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other("bad upstream status line"))?;
    let len = http1::response_body("DELETE", &response)?;
    let body = http1::read_body(&mut reader, len, MAX_REQUEST_BODY)?;
    Ok((status, response.headers, body))
}

/// Shared by the relay workers of one run.
pub struct Front {
    pub ca: Arc<InterceptCa>,
    pub inner: Inner,
}

#[cfg(test)]
#[path = "github_connect_tests.rs"]
mod tests;
