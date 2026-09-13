//! Local, verified-TLS fixture for black-box Bosn setup tests.
//!
//! The identity is public test-only material. Clients are given its CA only
//! through their child-process `SSL_CERT_FILE`; production code continues to
//! use kernal-api's normal verified TLS constructor without a trust bypass.

use std::{
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const IDENTITY_HEX: &str = include_str!("../fixtures/remote-setup-tls/identity.p12.hex");
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// Absolute path to the public fixture CA, suitable only for an isolated child
/// process in an integration test.
pub fn certificate_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/remote-setup-tls/cert.pem")
}

#[derive(Default)]
struct ServerState {
    body: Vec<u8>,
    request_targets: Vec<String>,
}

/// A small local HTTPS server that serves the currently selected setup body.
/// It intentionally accepts only ordinary GET requests and does not log query
/// strings, so tests can prove Bosn itself redacts source credentials.
pub struct TlsSetupServer {
    state: Arc<Mutex<ServerState>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    port: u16,
}

impl TlsSetupServer {
    pub fn start(body: impl Into<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local HTTPS fixture");
        listener
            .set_nonblocking(true)
            .expect("make local HTTPS fixture nonblocking");
        let port = listener
            .local_addr()
            .expect("read local HTTPS address")
            .port();
        let state = Arc::new(Mutex::new(ServerState {
            body: body.into(),
            request_targets: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = Arc::clone(&state);
        let worker_stop = Arc::clone(&stop);
        let acceptor = tls_acceptor();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => serve_one(socket, &acceptor, &worker_state),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => panic!("accept local HTTPS fixture: {error}"),
                }
            }
        });
        Self {
            state,
            stop,
            worker: Some(worker),
            port,
        }
    }

    pub fn url(&self, path_and_query: &str) -> String {
        assert!(path_and_query.starts_with('/'));
        format!("https://localhost:{}{path_and_query}", self.port)
    }

    pub fn replace_body(&self, body: impl Into<Vec<u8>>) {
        self.state.lock().expect("lock local HTTPS fixture").body = body.into();
    }

    pub fn request_count(&self) -> usize {
        self.state
            .lock()
            .expect("lock local HTTPS fixture")
            .request_targets
            .len()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join local HTTPS fixture");
        }
    }
}

impl Drop for TlsSetupServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn tls_acceptor() -> native_tls::TlsAcceptor {
    let hex: String = IDENTITY_HEX.split_whitespace().collect();
    let identity: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("valid fixture hex"))
        .collect();
    native_tls::TlsAcceptor::new(
        native_tls::Identity::from_pkcs12(&identity, "fixture").expect("load fixture identity"),
    )
    .expect("construct local TLS acceptor")
}

fn serve_one(socket: TcpStream, acceptor: &native_tls::TlsAcceptor, state: &Mutex<ServerState>) {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set local HTTPS fixture read timeout");
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set local HTTPS fixture write timeout");
    let Ok(mut tls) = acceptor.accept(socket) else {
        return;
    };
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") && request.len() < MAX_REQUEST_BYTES {
        let mut byte = [0];
        if tls.read_exact(&mut byte).is_err() {
            return;
        }
        request.push(byte[0]);
    }
    if !request.ends_with(b"\r\n\r\n") {
        return;
    }
    let request = match std::str::from_utf8(&request) {
        Ok(request) => request,
        Err(_) => return,
    };
    let Some(target) = request
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|line| line.split_once(' '))
        .map(|(target, _)| target)
    else {
        return;
    };
    let body = {
        let mut state = state.lock().expect("lock local HTTPS fixture");
        state.request_targets.push(target.into());
        state.body.clone()
    };
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/toml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tls.write_all(header.as_bytes())
        .and_then(|()| tls.write_all(&body))
        .expect("write local HTTPS fixture response");
}
