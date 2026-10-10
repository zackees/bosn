//! Per-job Docker API accounting proxy (#358).
//!
//! A task that mounts the host Docker socket (act, typically) creates sibling
//! containers that Bosn would otherwise never see. For such a task the daemon
//! starts one of these proxies on a private Unix socket and hands the task
//! only `DOCKER_HOST=unix://<socket>`. The socket lives in a directory bound
//! into the setup container at its own host path, so when act bind-mounts
//! "its" Docker socket into a job container, that container talks to the
//! same proxy.
//!
//! The proxy forwards every request and response byte for byte, except the
//! bodies of three create calls, which it rewrites:
//!
//! * `POST /containers/create`: adds the job's ownership labels, caps CPU
//!   (`NanoCpus`) and optionally memory at the runner slot's limits, maps
//!   named volumes through the job's cache policy, and adds the job's
//!   injected cache mounts.
//! * `POST /networks/create`, `POST /volumes/create`: adds the labels (and,
//!   for volumes, the cache mapping).
//!
//! Everything a job creates therefore carries `com.zackees.bosn.run=<run>`,
//! and teardown removes exactly that set. Every byte in either direction
//! also counts as job activity for stall detection.
//!
//! Two more rewrites isolate concurrent runs of one workflow. act derives
//! container (and per-job volume) names from the workflow and job names
//! only, and before it creates a job container it lists **all** containers
//! and force-removes any with its name, which is another session's job
//! container whenever two checkouts run the same workflow at once. So:
//!
//! * a container create's `name` gets the run's suffix (act addresses its
//!   containers by the ID the create returned, so it never notices), and
//! * `GET /containers/json` is filtered to the run's own containers, so a
//!   job can neither see nor remove another run's containers by listing.
//!
//! A job's whole-host calls are scoped to the run too (#560), so a workflow's
//! "free disk space" step cannot delete other runs' or Bosn's objects:
//!
//! * container, volume, network and image prunes, and the volume listing,
//!   get the run's label filter;
//! * a build-cache prune is narrowed to match nothing (BuildKit takes no
//!   label filter, and its cache is the host's);
//! * a request that names one object (container, exec, network, volume, image
//!   delete) is forwarded only when that object carries the run's label (#547,
//!   see `access`), and a container create cannot reach into another run's
//!   containers or networks;
//! * with [`ProxySettings::cgroup_parent`], every container is forced into the
//!   run's cgroup parent, whatever the caller asked for.
//!
//! Hijacked streams (`attach`, `exec start`: `Upgrade: tcp`) switch to a raw
//! copy after the request, so interactive streams pass through untouched.

// The transport is a Unix socket; elsewhere only the types are used.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

mod access;
mod rewrite;
use access::{Refusal, addressed, create_references, refusal_status};
pub use rewrite::{LABEL_CGROUP_PARENT, rewrite_create};
use rewrite::{Scope, create_kind, scope, scope_build_prune, scope_listing, suffix_name};

const MAX_HEAD_BYTES: usize = 1024 * 1024;
const MAX_REWRITE_BODY: usize = 8 * 1024 * 1024;
const MAX_INSPECT_BYTES: u64 = 1024 * 1024;
const INSPECT_DEADLINE: Duration = Duration::from_secs(10);

/// Last activity of a job, shared between its proxy, the executor and the
/// daemon's stall sweep. Milliseconds since the Unix epoch.
#[derive(Debug, Default)]
pub struct Activity {
    last_ms: AtomicU64,
    requests: AtomicU64,
    bytes: AtomicU64,
    creates: AtomicU64,
}
impl Activity {
    pub fn new() -> Self {
        let activity = Self::default();
        activity.touch();
        activity
    }
    pub fn touch(&self) {
        self.last_ms.store(now_ms(), Ordering::Relaxed);
    }
    fn traffic(&self, bytes: usize) {
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.touch();
    }
    pub fn last_ms(&self) -> u64 {
        self.last_ms.load(Ordering::Relaxed)
    }
    /// The last activity as an [`Instant`] on this process's clock.
    pub fn last_instant(&self) -> Instant {
        let age = now_ms().saturating_sub(self.last_ms());
        Instant::now()
            .checked_sub(Duration::from_millis(age))
            .unwrap_or_else(Instant::now)
    }
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    pub fn creates(&self) -> u64 {
        self.creates.load(Ordering::Relaxed)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// How a job's named volumes are mapped; implemented by the runner
/// registry's cache leases (see `runners`).
pub trait VolumePolicy: Send + Sync {
    /// The volume to attach where the task asked for `name`. It must exist
    /// (or be creatable by Docker) when this returns.
    fn map_volume(&self, name: &str) -> io::Result<String>;
    /// `(container target, volume)` mounts added to every container the job
    /// creates that does not already mount something at that target.
    fn injected_mounts(&self) -> io::Result<Vec<(String, String)>>;
}

/// A policy that maps nothing; for tests and for jobs without caches.
pub struct NoVolumes;
impl VolumePolicy for NoVolumes {
    fn map_volume(&self, name: &str) -> io::Result<String> {
        Ok(name.to_owned())
    }
    fn injected_mounts(&self) -> io::Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }
}

pub struct ProxySettings {
    pub upstream: PathBuf,
    /// The `com.zackees.bosn.run` value that scopes container listings.
    pub run: String,
    /// Appended (after `-`) to the names of containers the job creates.
    pub name_suffix: String,
    pub labels: BTreeMap<String, String>,
    /// `0` leaves CPU unlimited.
    pub nano_cpus: i64,
    pub memory: Option<u64>,
    /// Forced as `HostConfig.CgroupParent` on every container the job
    /// creates (#547); `None` leaves Docker's default.
    pub cgroup_parent: Option<String>,
    pub volumes: Arc<dyn VolumePolicy>,
    pub activity: Arc<Activity>,
    /// Receives one line per notable rewrite, for the job log.
    pub notes: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

pub struct DockerProxy {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    connections: Arc<Mutex<BTreeMap<u64, Connection>>>,
}

struct Connection {
    #[cfg(unix)]
    client: UnixStream,
    #[cfg(unix)]
    upstream: UnixStream,
}

impl DockerProxy {
    /// Bind `path` (mode 0600) and serve until [`Self::stop`] or drop.
    #[cfg(unix)]
    pub fn start(path: &Path, settings: ProxySettings) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let connections: Arc<Mutex<BTreeMap<u64, Connection>>> = Arc::default();
        let settings = Arc::new(settings);
        let accept = {
            let stop = Arc::clone(&stop);
            let connections = Arc::clone(&connections);
            std::thread::Builder::new()
                .name("bosn-docker-proxy".into())
                .spawn(move || accept_loop(listener, stop, connections, settings))?
        };
        Ok(Self {
            path: path.to_owned(),
            stop,
            accept: Some(accept),
            connections,
        })
    }

    #[cfg(not(unix))]
    pub fn start(_path: &Path, _settings: ProxySettings) -> io::Result<Self> {
        Err(io::Error::other("the Docker proxy needs Unix sockets"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop accepting, close every open connection, and remove the socket.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        #[cfg(unix)]
        for (_, connection) in std::mem::take(&mut *self.connections.lock().unwrap()) {
            let _ = connection.client.shutdown(std::net::Shutdown::Both);
            let _ = connection.upstream.shutdown(std::net::Shutdown::Both);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
impl Drop for DockerProxy {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(unix)]
fn accept_loop(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<BTreeMap<u64, Connection>>>,
    settings: Arc<ProxySettings>,
) {
    let mut next = 0u64;
    while !stop.load(Ordering::SeqCst) {
        let client = match listener.accept() {
            Ok((client, _)) => client,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        let _ = client.set_nonblocking(false);
        let Ok(upstream) = UnixStream::connect(&settings.upstream) else {
            let _ = (&client).write_all(
                b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            continue;
        };
        let (Ok(client_copy), Ok(upstream_copy)) = (client.try_clone(), upstream.try_clone())
        else {
            continue;
        };
        next += 1;
        let id = next;
        connections.lock().unwrap().insert(
            id,
            Connection {
                client: client_copy,
                upstream: upstream_copy,
            },
        );
        let settings = Arc::clone(&settings);
        let connections = Arc::clone(&connections);
        let _ = std::thread::Builder::new()
            .name("bosn-docker-proxy-conn".into())
            .spawn(move || {
                serve_connection(client, upstream, &settings);
                connections.lock().unwrap().remove(&id);
            });
    }
}

#[cfg(unix)]
fn serve_connection(client: UnixStream, upstream: UnixStream, settings: &ProxySettings) {
    let Ok(client_writer) = client.try_clone() else {
        return;
    };
    let Ok(upstream_reader) = upstream.try_clone() else {
        return;
    };
    let activity = Arc::clone(&settings.activity);
    // Responses (and hijacked output) flow back untouched.
    let responses = std::thread::Builder::new()
        .name("bosn-docker-proxy-resp".into())
        .spawn(move || {
            pump(upstream_reader, &client_writer, &activity);
            let _ = client_writer.shutdown(std::net::Shutdown::Write);
        });
    let mut upstream_writer = upstream;
    let refusal_writer = client.try_clone();
    let mut reader = BufReader::new(client);
    if let Err(error) = forward_requests(&mut reader, &mut upstream_writer, settings)
        && error.kind() == io::ErrorKind::PermissionDenied
        && let Ok(mut writer) = refusal_writer
    {
        // Answer the refused request in Docker's error shape; the client
        // reports the message instead of a bare EOF.
        let status = refusal_status(&error);
        let reason = match error.get_ref().and_then(|e| e.downcast_ref::<Refusal>()) {
            Some(refusal) => refusal.reason(),
            None => "Internal Server Error",
        };
        let body = error.to_string();
        let _ = writer.write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        let _ = writer.shutdown(std::net::Shutdown::Both);
    }
    let _ = upstream_writer.shutdown(std::net::Shutdown::Write);
    if let Ok(handle) = responses {
        let _ = handle.join();
    }
}

fn pump(mut from: impl Read, mut to: impl Write, activity: &Activity) {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                activity.traffic(n);
                if to.write_all(&buffer[..n]).is_err() {
                    return;
                }
            }
        }
    }
}

/// One parsed request head.
#[derive(Debug)]
struct Head {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
}
impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
    fn chunked(&self) -> bool {
        self.header("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    }
    fn content_length(&self) -> io::Result<usize> {
        match self.header("content-length") {
            None => Ok(0),
            Some(value) => value
                .trim()
                .parse()
                .map_err(|_| io::Error::other("malformed Content-Length")),
        }
    }
    fn upgrade(&self) -> bool {
        self.header("upgrade").is_some()
            || self
                .header("connection")
                .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
    }
    fn encode(&self, without: &[&str]) -> Vec<u8> {
        let mut text = format!("{} {} {}\r\n", self.method, self.target, self.version);
        for (key, value) in &self.headers {
            if without.iter().any(|w| key.eq_ignore_ascii_case(w)) {
                continue;
            }
            text.push_str(&format!("{key}: {value}\r\n"));
        }
        text.into_bytes()
    }
}

fn read_head<R: BufRead>(reader: &mut R) -> io::Result<Option<Head>> {
    let mut total = 0usize;
    let mut line = String::new();
    // Tolerate stray CRLFs between requests (RFC 9112 2.2).
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        total += n;
        if !line.trim().is_empty() {
            break;
        }
        if total > MAX_HEAD_BYTES {
            return Err(io::Error::other("request head too large"));
        }
    }
    let mut parts = line.trim_end().splitn(3, ' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(io::Error::other("malformed request line"));
    };
    let mut head = Head {
        method: method.to_owned(),
        target: target.to_owned(),
        version: version.to_owned(),
        headers: Vec::new(),
    };
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        total += n;
        if n == 0 || total > MAX_HEAD_BYTES {
            return Err(io::Error::other("truncated or oversized request head"));
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            return Ok(Some(head));
        }
        let (key, value) = header
            .split_once(':')
            .ok_or_else(|| io::Error::other("malformed header"))?;
        head.headers.push((key.to_owned(), value.trim().to_owned()));
    }
}

fn forward_requests<R: BufRead, W: Write>(
    reader: &mut R,
    upstream: &mut W,
    settings: &ProxySettings,
) -> io::Result<()> {
    while let Some(head) = read_head(reader)? {
        settings.activity.requests.fetch_add(1, Ordering::Relaxed);
        settings.activity.touch();
        if let Some(kind) = create_kind(&head.method, &head.target) {
            let body = read_body(reader, &head)?;
            let rewritten = rewrite_create(kind, &body, settings).and_then(|rewritten| {
                if kind == "container" {
                    for reference in create_references(&rewritten)? {
                        // 403, not 404: the Docker CLI reads a 404 on create
                        // as a missing image and starts pulling.
                        access::check(&settings.upstream, &reference, &settings.run).map_err(
                            |error| match error.get_ref().and_then(|e| e.downcast_ref::<Refusal>())
                            {
                                Some(refusal) => Refusal::error(
                                    403,
                                    format!(
                                        "bosn docker proxy refused the container create: {}",
                                        refusal.message
                                    ),
                                ),
                                None => error,
                            },
                        )?;
                    }
                }
                Ok(rewritten)
            });
            let body = match rewritten {
                Ok(rewritten) => {
                    settings.activity.creates.fetch_add(1, Ordering::Relaxed);
                    rewritten
                }
                Err(error) if error.get_ref().is_some_and(|e| e.is::<Refusal>()) => {
                    return Err(error);
                }
                Err(error) => {
                    // Never forward an unaccounted create: refuse it with a
                    // Docker-shaped error the client will report.
                    let message = serde_json::json!({
                        "message": format!("bosn docker proxy refused the {kind} create: {error}")
                    })
                    .to_string();
                    if let Some(notes) = &settings.notes {
                        notes(format!(
                            "[bosn] docker proxy refused a {kind} create: {error}"
                        ));
                    }
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, message));
                }
            };
            let mut head = head;
            if kind == "container" {
                head.target = suffix_name(&head.target, &settings.name_suffix);
            }
            let mut out = head.encode(&["content-length", "transfer-encoding"]);
            out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
            out.extend_from_slice(&body);
            upstream.write_all(&out)?;
            upstream.flush()?;
            continue;
        }
        let mut head = head;
        match scope(&head.method, &head.target) {
            Scope::Forward => {}
            Scope::Label => head.target = scope_listing(&head.target, &settings.run)?,
            Scope::BuildCache => head.target = scope_build_prune(&head.target)?,
        }
        if let Some(access) = addressed(&head.method, &head.target) {
            access::check(&settings.upstream, &access, &settings.run).map_err(|error| {
                if error.get_ref().is_some_and(|e| e.is::<Refusal>()) {
                    error
                } else {
                    // Fail closed: an object the proxy cannot resolve is not
                    // forwarded.
                    Refusal::error(
                        500,
                        format!("bosn docker proxy could not resolve {}: {error}", access.id),
                    )
                }
            })?;
        }
        let mut out = head.encode(&[]);
        out.extend_from_slice(b"\r\n");
        upstream.write_all(&out)?;
        if head.chunked() {
            copy_chunked(reader, upstream)?;
        } else {
            let length = head.content_length()?;
            let copied = io::copy(&mut reader.by_ref().take(length as u64), upstream)?;
            if copied != length as u64 {
                return Err(io::Error::other("request body ended early"));
            }
        }
        upstream.flush()?;
        if head.upgrade() {
            // A hijacked stream: whatever the client sends next is raw.
            io::copy(reader, upstream)?;
            return Ok(());
        }
    }
    Ok(())
}

fn read_body<R: BufRead>(reader: &mut R, head: &Head) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    if head.chunked() {
        copy_chunked(reader, &mut ChunkSink(&mut body))?;
    } else {
        let length = head.content_length()?;
        if length > MAX_REWRITE_BODY {
            return Err(io::Error::other("create body too large"));
        }
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    }
    Ok(body)
}

/// Decodes chunk framing while copying, so a rewritten body is plain bytes.
struct ChunkSink<'a>(&'a mut Vec<u8>);

/// Copy one chunked body. With a [`ChunkSink`] destination the framing is
/// removed; otherwise it is forwarded verbatim.
fn copy_chunked<R: BufRead, W: ChunkWrite>(reader: &mut R, out: &mut W) -> io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::other("chunked body ended early"));
        }
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|_| io::Error::other("malformed chunk size"))?;
        out.frame(line.as_bytes())?;
        if size == 0 {
            // Trailers, then the blank line that ends the body.
            loop {
                line.clear();
                if reader.read_line(&mut line)? == 0 {
                    return Err(io::Error::other("chunked trailer ended early"));
                }
                out.frame(line.as_bytes())?;
                if line.trim().is_empty() {
                    return Ok(());
                }
            }
        }
        let mut remaining = size;
        let mut buffer = [0u8; 64 * 1024];
        while remaining > 0 {
            let want = remaining.min(buffer.len());
            reader.read_exact(&mut buffer[..want])?;
            out.data(&buffer[..want])?;
            remaining -= want;
        }
        line.clear();
        reader.read_line(&mut line)?;
        out.frame(line.as_bytes())?;
    }
}

trait ChunkWrite {
    fn frame(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn data(&mut self, bytes: &[u8]) -> io::Result<()>;
}
impl<W: Write> ChunkWrite for W {
    fn frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes)
    }
    fn data(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes)
    }
}
impl ChunkWrite for ChunkSink<'_> {
    fn frame(&mut self, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn data(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.0.len() + bytes.len() > MAX_REWRITE_BODY {
            return Err(io::Error::other("create body too large"));
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod access_tests;
#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// A cache policy whose volume could not be prepared (a full disk, a
    /// refused volume create).
    struct BrokenVolumes;
    impl VolumePolicy for BrokenVolumes {
        fn map_volume(&self, _name: &str) -> io::Result<String> {
            Err(io::Error::other("volume create: no space left on device"))
        }
        fn injected_mounts(&self) -> io::Result<Vec<(String, String)>> {
            Err(io::Error::other("volume create: no space left on device"))
        }
    }

    pub(super) fn settings(volumes: Arc<dyn VolumePolicy>) -> ProxySettings {
        ProxySettings {
            upstream: PathBuf::from("/nonexistent"),
            run: "r-1".into(),
            name_suffix: "b1".into(),
            labels: BTreeMap::from([("com.zackees.bosn.run".into(), "r-1".into())]),
            nano_cpus: 4_000_000_000,
            memory: Some(1 << 30),
            cgroup_parent: None,
            volumes,
            activity: Arc::new(Activity::new()),
            notes: None,
        }
    }

    #[test]
    fn a_create_whose_cache_volume_cannot_be_prepared_is_refused_not_forwarded() {
        let s = settings(Arc::new(BrokenVolumes));
        let (result, out) = forward(
            b"POST /containers/create HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
            &s,
        );
        let error = result.unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::PermissionDenied,
            "answered with a 500"
        );
        assert!(error.to_string().contains("no space left"), "{error}");
        assert!(out.is_empty(), "nothing reached Docker");
    }

    #[test]
    fn listings_and_named_creates_are_rewritten_on_the_wire() {
        let s = settings(Arc::new(NoVolumes));
        let (result, out) = forward(
            b"GET /v1.47/containers/json?all=1 HTTP/1.1\r\nHost: docker\r\n\r\nPOST /v1.47/containers/create?name=job HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
            &s,
        );
        result.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("GET /v1.47/containers/json?all=1&filters="),
            "{text}"
        );
        assert!(
            text.contains("POST /v1.47/containers/create?name=job-b1 HTTP/1.1"),
            "{text}"
        );
    }

    /// Run [`forward_requests`] over in-memory buffers.
    pub(super) fn forward(input: &[u8], settings: &ProxySettings) -> (io::Result<()>, Vec<u8>) {
        let mut reader = BufReader::new(input);
        let mut out = Vec::new();
        let result = forward_requests(&mut reader, &mut out, settings);
        (result, out)
    }

    #[cfg(unix)]
    #[test]
    fn requests_pass_through_byte_for_byte_except_create_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = settings(Arc::new(NoVolumes));
        let own = r#"{"Config":{"Labels":{"com.zackees.bosn.run":"r-1"}}}"#;
        s.upstream = super::access_tests::inspecting_upstream(
            dir.path(),
            &[
                ("/containers/x/json", own),
                ("/exec/1/json", r#"{"ContainerID":"x"}"#),
            ],
        );
        let plain = b"GET /_ping HTTP/1.1\r\nHost: docker\r\n\r\nPUT /containers/x/archive?path=/ HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nPOST /exec/1/start HTTP/1.1\r\nContent-Length: 2\r\nUpgrade: tcp\r\nConnection: Upgrade\r\n\r\n{}raw stdin bytes";
        let (result, out) = forward(plain, &s);
        result.unwrap();
        assert_eq!(
            out,
            plain.to_vec(),
            "framing, chunks and the hijacked tail are untouched"
        );
        assert_eq!(s.activity.requests(), 3);

        let create = b"POST /v1.47/containers/create HTTP/1.1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"Ima\r\n9\r\nge\":\"u\"}\r\n0\r\n\r\nGET /_ping HTTP/1.1\r\n\r\n";
        let (result, out) = forward(create, &s);
        result.unwrap();
        let text = String::from_utf8(out).unwrap();
        let (head, rest) = text.split_once("\r\n\r\n").unwrap();
        assert!(!head.to_ascii_lowercase().contains("transfer-encoding"));
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        let body: Value = serde_json::from_str(&rest[..length]).unwrap();
        assert_eq!(body["Image"], "u");
        assert_eq!(body["HostConfig"]["NanoCpus"], 4_000_000_000_i64);
        assert_eq!(&rest[length..], "GET /_ping HTTP/1.1\r\n\r\n");
        assert_eq!(s.activity.creates(), 1);
    }

    #[test]
    fn malformed_or_oversized_requests_stop_the_connection() {
        let s = settings(Arc::new(NoVolumes));
        assert!(forward(b"NONSENSE\r\n\r\n", &s).0.is_err());
        assert!(
            forward(b"POST /x HTTP/1.1\r\nContent-Length: abc\r\n\r\n", &s)
                .0
                .is_err()
        );
        assert!(
            forward(b"POST /x HTTP/1.1\r\nContent-Length: 10\r\n\r\nshort", &s)
                .0
                .is_err()
        );
        assert!(
            forward(
                b"PUT /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
                &s
            )
            .0
            .is_err()
        );
        let huge = format!(
            "POST /containers/create HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_REWRITE_BODY + 1
        );
        assert!(forward(huge.as_bytes(), &s).0.is_err());
        // A create the proxy cannot account for is never forwarded.
        let (result, out) = forward(
            b"POST /containers/create HTTP/1.1\r\nContent-Length: 3\r\n\r\n[1]",
            &s,
        );
        assert!(result.is_err());
        assert!(out.is_empty());
        // A clean EOF between requests is a normal close.
        assert!(forward(b"", &s).0.is_ok());
        assert!(forward(b"\r\n", &s).0.is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_live_proxy_forwards_to_its_upstream_and_closes_on_stop() {
        let dir = tempfile::tempdir().unwrap();
        let upstream_path = dir.path().join("up.sock");
        let upstream = UnixListener::bind(&upstream_path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = upstream.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let head = read_head(&mut reader).unwrap().unwrap();
            let body = read_body(&mut reader, &head).unwrap();
            let mut writer = stream;
            let reply = format!(
                "HTTP/1.1 201 Created\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            writer.write_all(reply.as_bytes()).unwrap();
            writer.write_all(&body).unwrap();
            (head.target, body)
        });
        let mut s = settings(Arc::new(NoVolumes));
        s.upstream = upstream_path;
        let activity = Arc::clone(&s.activity);
        let before = activity.last_ms();
        let socket = dir.path().join("p.sock");
        let mut proxy = DockerProxy::start(&socket, s).unwrap();
        let mut client = UnixStream::connect(&socket).unwrap();
        client
            .write_all(b"POST /v1.47/networks/create HTTP/1.1\r\nContent-Length: 13\r\n\r\n{\"Name\":\"n\"}\n")
            .unwrap();
        let response =
            crate::docker_api::read_response(BufReader::new(client.try_clone().unwrap())).unwrap();
        assert_eq!(response.status, 201);
        let echoed: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(echoed["Labels"]["com.zackees.bosn.run"], "r-1");
        let (target, _) = server.join().unwrap();
        assert_eq!(target, "/v1.47/networks/create");
        assert!(activity.last_ms() >= before);
        assert!(activity.bytes() > 0);
        proxy.stop();
        assert!(!socket.exists(), "stop removes the socket");
        assert!(UnixStream::connect(&socket).is_err());
    }
}
