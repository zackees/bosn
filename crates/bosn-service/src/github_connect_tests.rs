//! The CONNECT front end to end, without an engine: a client that hard-codes
//! `https://api.github.com` and honours `HTTPS_PROXY` reaches the run's
//! proxy, which swaps the fake token for the real one (#650).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use kernal_api::async_engine::{self, RuntimeBuilder};
use kernal_api::http_server;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use openssl::x509::X509;

use super::*;
use crate::github_ci_policy::{CiPolicy, FakeToken, GithubWrites};
use crate::github_proxy::{GithubApiProxy, GithubCredential};

const REAL: &str = "gho_CANARYrealtoken0123456789abcdefXYZ";

/// (method, target, authorization) of every request the fake GitHub saw.
type Seen = Arc<Mutex<Vec<(String, String, Option<String>)>>>;

async fn fake_github(seen: Seen) -> (String, async_engine::Task<io::Result<()>>) {
    let server = http_server::Server::bind(
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
        http_server::Limits::default(),
    )
    .await
    .unwrap();
    let port = server.local_addr().unwrap().port();
    let task = async_engine::launch(server.serve(move |request| {
        let seen = seen.clone();
        async move {
            let auth = request
                .header("authorization")
                .map(|v| String::from_utf8_lossy(v).into_owned());
            seen.lock().unwrap().push((
                request.method().to_owned(),
                request.target().to_owned(),
                auth,
            ));
            let body = if request.method() == "DELETE" {
                Vec::new()
            } else {
                br#"{"total_count":1,"actions_caches":[{"id":42}]}"#.to_vec()
            };
            let status = if request.method() == "DELETE" {
                204
            } else {
                200
            };
            http_server::Response::new(status, body)
                .unwrap()
                .with_header("content-type", "application/json")
                .unwrap()
        }
    }));
    (format!("http://127.0.0.1:{port}"), task)
}

/// One front connection: what a relay channel hands the daemon.
fn front(ca: Arc<InterceptCa>, inner: Inner, logs: async_engine::Sender<String>) -> TcpStream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let log = |line: String| {
            let _ = logs.try_send(line);
        };
        serve_client(stream, &ca, &inner, &log);
    });
    TcpStream::connect(addr).unwrap()
}

/// CONNECT through the front, then TLS to `api.github.com`, trusting only
/// the run's CA, and send one request. Returns (status line, body).
fn https_call(mut stream: TcpStream, ca_pem: &str, request: &str) -> (String, String) {
    stream
        .write_all(b"CONNECT api.github.com:443 HTTP/1.1\r\nHost: api.github.com:443\r\n\r\n")
        .unwrap();
    let mut established = [0u8; 39];
    stream.read_exact(&mut established).unwrap();
    assert!(String::from_utf8_lossy(&established).starts_with("HTTP/1.1 200"));
    let mut connector = SslConnector::builder(SslMethod::tls_client()).unwrap();
    connector
        .cert_store_mut()
        .add_cert(X509::from_pem(ca_pem.as_bytes()).unwrap())
        .unwrap();
    connector.set_verify(SslVerifyMode::PEER);
    let mut tls = connector
        .build()
        .connect("api.github.com", stream)
        .expect("the leaf verifies for api.github.com under the run CA");
    tls.write_all(request.as_bytes()).unwrap();
    let mut reader = BufReader::new(tls);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap();
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    (
        status.trim().to_owned(),
        String::from_utf8_lossy(&body).into_owned(),
    )
}

fn get(path: &str, token: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: api.github.com\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )
}

fn delete(path: &str, token: &str) -> String {
    format!(
        "DELETE {path} HTTP/1.1\r\nHost: api.github.com\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

struct Rig {
    seen: Seen,
    ca: Arc<InterceptCa>,
    inner: Inner,
    fake: FakeToken,
    logs: async_engine::Sender<String>,
    log_rx: async_engine::Receiver<String>,
    _proxy: GithubApiProxy,
    _upstream: async_engine::Task<io::Result<()>>,
}

async fn rig(writes: GithubWrites) -> Rig {
    let seen: Seen = Arc::default();
    let (upstream, task) = fake_github(seen.clone()).await;
    let (logs, log_rx) = async_engine::channel::<String>(256);
    let fake = FakeToken::for_test(3);
    let proxy = GithubApiProxy::start_with_policy(
        &upstream,
        GithubCredential::for_test(REAL),
        Arc::default(),
        Some(logs.clone()),
        Some(CiPolicy {
            fake: fake.clone(),
            repository: "o/r".into(),
            writes,
        }),
    )
    .await
    .unwrap();
    let (addr, prefix) = proxy.listener();
    Rig {
        seen,
        ca: Arc::new(InterceptCa::mint(INTERCEPTED_HOST).unwrap()),
        inner: Inner { addr, prefix },
        fake,
        logs,
        log_rx,
        _proxy: proxy,
        _upstream: task,
    }
}

impl Rig {
    async fn call(&self, request: String) -> (String, String) {
        let stream = front(self.ca.clone(), self.inner.clone(), self.logs.clone());
        let pem = self.ca.ca_pem().to_owned();
        async_engine::launch_blocking(move || https_call(stream, &pem, &request))
            .await
            .unwrap()
    }
    fn seen(&self) -> Vec<(String, String, Option<String>)> {
        self.seen.lock().unwrap().clone()
    }
    fn logs(&mut self) -> String {
        let mut lines = Vec::new();
        while let Ok(line) = self.log_rx.try_recv() {
            lines.push(line);
        }
        lines.join("\n")
    }
}

fn runtime() -> async_engine::Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_direct_api_github_com_call_is_intercepted_and_the_fake_token_swapped() {
    runtime().run(async {
        let mut rig = rig(GithubWrites::Off).await;
        let fake = rig.fake.secret_value().to_owned();
        let (status, body) = rig
            .call(get("/repos/o/r/actions/caches?per_page=100", &fake))
            .await;
        assert_eq!(status, "HTTP/1.1 200 OK", "{body}");
        assert!(body.contains("actions_caches"));
        let seen = rig.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "GET");
        assert_eq!(seen[0].1, "/repos/o/r/actions/caches?per_page=100");
        assert_eq!(
            seen[0].2.as_deref(),
            Some(format!("Bearer {REAL}").as_str())
        );
        let logs = rig.logs();
        assert!(
            logs.contains("GET /repos/o/r/actions/caches -> 200 forwarded"),
            "{logs}"
        );
        assert!(!logs.contains(REAL) && !logs.contains(&fake), "{logs}");
        assert!(!body.contains(REAL));
    });
}

#[test]
fn a_foreign_token_is_rejected_without_upstream_contact() {
    runtime().run(async {
        let mut rig = rig(GithubWrites::On).await;
        let (status, body) = rig
            .call(get(
                "/repos/o/r/actions/caches",
                "ghp_0123456789abcdef0123456789abcdef0123",
            ))
            .await;
        assert!(status.starts_with("HTTP/1.1 401"), "{status}");
        assert!(body.contains("foreign token"));
        assert!(rig.seen().is_empty());
        assert!(rig.logs().contains("401 refused"));
    });
}

#[test]
fn writes_outside_the_allowlist_are_refused_and_dry_run_does_not_forward() {
    runtime().run(async {
        let mut rig = rig(GithubWrites::DryRun).await;
        let fake = rig.fake.secret_value().to_owned();
        let (status, _) = rig
            .call(delete("/repos/o/r/actions/caches/42", &fake))
            .await;
        assert!(status.starts_with("HTTP/1.1 204"), "{status}");
        let (status, _) = rig
            .call(delete("/repos/o/r/git/refs/heads/main", &fake))
            .await;
        assert!(status.starts_with("HTTP/1.1 403"), "{status}");
        let (status, _) = rig
            .call(delete("/repos/other/repo/actions/caches/42", &fake))
            .await;
        assert!(status.starts_with("HTTP/1.1 403"), "{status}");
        assert!(
            rig.seen().is_empty(),
            "nothing reached GitHub: {:?}",
            rig.seen()
        );
        let logs = rig.logs();
        assert!(
            logs.contains("DELETE /repos/o/r/actions/caches/42 -> 204 dry-run (not forwarded)"),
            "{logs}"
        );
        assert!(
            logs.contains("DELETE /repos/o/r/git/refs/heads/main -> 403 refused"),
            "{logs}"
        );
    });
}

#[test]
fn writes_on_forwards_an_allowlisted_delete_with_the_real_token() {
    runtime().run(async {
        let rig = rig(GithubWrites::On).await;
        let fake = rig.fake.secret_value().to_owned();
        let (status, _) = rig
            .call(delete("/repos/o/r/actions/caches/42", &fake))
            .await;
        assert!(status.starts_with("HTTP/1.1 204"), "{status}");
        let seen = rig.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            (seen[0].0.as_str(), seen[0].1.as_str()),
            ("DELETE", "/repos/o/r/actions/caches/42")
        );
        assert_eq!(
            seen[0].2.as_deref(),
            Some(format!("Bearer {REAL}").as_str())
        );
    });
}

#[test]
fn other_hosts_are_tunnelled_byte_for_byte() {
    let echo = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = echo.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = echo.accept().unwrap();
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).unwrap();
        s.write_all(&buf).unwrap();
    });
    let ca = Arc::new(InterceptCa::mint(INTERCEPTED_HOST).unwrap());
    let (logs, _rx) = async_engine::channel::<String>(8);
    let unused = Inner {
        addr: "127.0.0.1:9".parse().unwrap(),
        prefix: "/x".into(),
    };
    let mut stream = front(ca, unused, logs);
    stream
        .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
        .unwrap();
    let mut established = [0u8; 39];
    stream.read_exact(&mut established).unwrap();
    stream.write_all(b"hello").unwrap();
    let mut back = [0u8; 5];
    stream.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"hello");
}

#[test]
fn the_run_ca_is_a_constrained_ca_and_the_leaf_names_api_github_com() {
    let ca = InterceptCa::mint(INTERCEPTED_HOST).unwrap();
    let cert = X509::from_pem(ca.ca_pem().as_bytes()).unwrap();
    let text = String::from_utf8(cert.to_text().unwrap()).unwrap();
    assert!(text.contains("CA:TRUE, pathlen:0"), "{text}");
    assert!(text.contains("Certificate Sign"), "{text}");
    assert!(!ca.ca_pem().contains("PRIVATE KEY"));
    // Each run mints its own.
    assert_ne!(
        ca.ca_pem(),
        InterceptCa::mint(INTERCEPTED_HOST).unwrap().ca_pem()
    );
}
