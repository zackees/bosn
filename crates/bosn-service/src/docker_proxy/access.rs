//! Per-object access checks of the Docker accounting proxy (#547).
//!
//! Listings are filtered to the run (see `rewrite`), but Docker also takes
//! requests that name one object by ID, ID prefix or name: container
//! inspect/kill/rm/exec/attach/archive, exec start, network and volume
//! calls, image deletes. The proxy resolves each such object with one bounded
//! upstream inspect and forwards the request only when the object carries the
//! run's label. Another run's object (and any unlabelled container, such as
//! Bosn's own) is answered with Docker's own 404, so a job cannot even learn
//! that it exists. Shared, unlabelled networks, volumes and images stay
//! readable, but a write to them is refused with 403. An object Docker does
//! not know is forwarded, and Docker answers the 404 itself.
//!
//! A container create may not reach into another run either: its
//! `VolumesFrom`, `Links` and `container:<id>` network, PID and IPC modes must
//! name the run's own containers, and its networks must not be another
//! run's.

use std::{
    collections::BTreeMap,
    fmt, io,
    io::{Read, Write},
    path::Path,
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use serde::Deserialize;

use super::{INSPECT_DEADLINE, MAX_INSPECT_BYTES, rewrite::api_path, rewrite::decode};
use crate::docker_api::LABEL_RUN;

/// A request the proxy answers itself instead of forwarding.
#[derive(Debug)]
pub(super) struct Refusal {
    pub status: u16,
    pub message: String,
}
impl Refusal {
    pub fn error(status: u16, message: String) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, Self { status, message })
    }
    /// The HTTP reason phrase of [`Self::status`].
    pub fn reason(&self) -> &'static str {
        match self.status {
            403 => "Forbidden",
            404 => "Not Found",
            _ => "Internal Server Error",
        }
    }
}
/// Displays as Docker's error body, `{"message": ...}`.
impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let body = serde_json::json!({ "message": self.message });
        write!(f, "{body}")
    }
}
impl std::error::Error for Refusal {}

/// The HTTP status a refused request is answered with.
pub(super) fn refusal_status(error: &io::Error) -> u16 {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Refusal>())
        .map_or(500, |refusal| refusal.status)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Container,
    Exec,
    Network,
    Volume,
    Image,
}
impl Kind {
    fn noun(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Exec => "exec instance",
            Self::Network => "network",
            Self::Volume => "volume",
            Self::Image => "image",
        }
    }
    /// The inspect path of an object of this kind.
    fn inspect_path(self, id: &str) -> String {
        let id = crate::docker_api::encode(id);
        match self {
            Self::Container => format!("/containers/{id}/json"),
            Self::Exec => format!("/exec/{id}/json"),
            Self::Network => format!("/networks/{id}"),
            Self::Volume => format!("/volumes/{id}"),
            Self::Image => format!("/images/{id}/json"),
        }
    }
}

/// One object a request addresses, and whether the request changes it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Access {
    pub kind: Kind,
    pub id: String,
    pub write: bool,
}

/// Collection-level routes that share a prefix with object routes.
const COLLECTION_ROUTES: &[&str] = &["json", "create", "prune", "search", "load", "get"];

/// The object a request names, if any. Image reads, pushes and tags are
/// forwarded: images are shared and carry no run label.
pub(super) fn addressed(method: &str, target: &str) -> Option<Access> {
    let path = api_path(target);
    let write = !matches!(method, "GET" | "HEAD");
    let access = |kind, id: &str| {
        let id = decode(id);
        (!id.is_empty() && !COLLECTION_ROUTES.contains(&id.as_str())).then_some(Access {
            kind,
            id,
            write,
        })
    };
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    let (collection, id) = (parts.next()?, parts.next());
    match (collection, id) {
        ("containers", Some(id)) => access(Kind::Container, id),
        ("exec", Some(id)) => access(Kind::Exec, id),
        ("networks", Some(id)) => access(Kind::Network, id),
        ("volumes", Some(id)) if parts.next().is_none() => access(Kind::Volume, id),
        // Image names contain slashes: everything after `/images/` is one.
        ("images", Some(_)) if method == "DELETE" => {
            access(Kind::Image, path.trim_start_matches("/images/"))
        }
        ("commit", None) => {
            let (_, pairs) = super::rewrite::query_pairs(target);
            let container = pairs.into_iter().find(|(k, _)| k == "container")?.1;
            Some(Access {
                kind: Kind::Container,
                id: container,
                write: true,
            })
        }
        _ => None,
    }
}

#[derive(Deserialize, Default)]
struct Labelled {
    #[serde(rename = "Labels", default)]
    labels: Option<BTreeMap<String, String>>,
}
#[derive(Deserialize)]
struct Configured {
    #[serde(rename = "Config", default)]
    config: Option<Labelled>,
}
#[derive(Deserialize)]
struct ExecInspect {
    #[serde(rename = "ContainerID")]
    container_id: String,
}

/// Who owns an inspected object.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Owner {
    /// Docker does not know it; Docker answers the request itself.
    Missing,
    Run(String),
    Unlabelled,
}

/// An exec instance is owned by its container; anything else by its label.
enum Inspected {
    Owner(Owner),
    ExecOf(String),
}

/// Decide an upstream inspect `response` (an HTTP/1.0 reply) for `kind`.
fn parse_inspect(kind: Kind, response: &[u8]) -> io::Result<Inspected> {
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other(format!("{} inspect returned no response", kind.noun())))?;
    let head = String::from_utf8_lossy(&response[..split]);
    let body = &response[split + 4..];
    let malformed =
        |_| io::Error::other(format!("{} inspect returned malformed JSON", kind.noun()));
    match head.split_whitespace().nth(1).unwrap_or("") {
        "404" => Ok(Inspected::Owner(Owner::Missing)),
        "200" => {
            let labels = match kind {
                Kind::Exec => {
                    let exec: ExecInspect = serde_json::from_slice(body).map_err(malformed)?;
                    return Ok(Inspected::ExecOf(exec.container_id));
                }
                Kind::Container | Kind::Image => {
                    let inspect: Configured = serde_json::from_slice(body).map_err(malformed)?;
                    inspect.config.unwrap_or_default().labels
                }
                Kind::Network | Kind::Volume => {
                    serde_json::from_slice::<Labelled>(body)
                        .map_err(malformed)?
                        .labels
                }
            };
            Ok(Inspected::Owner(
                match labels.and_then(|mut l| l.remove(LABEL_RUN)) {
                    Some(run) => Owner::Run(run),
                    None => Owner::Unlabelled,
                },
            ))
        }
        status => Err(io::Error::other(format!(
            "{} inspect answered {status}",
            kind.noun()
        ))),
    }
}

/// Whether the run may make `access` to an object `owner` owns.
pub(super) fn verdict(access: &Access, owner: &Owner, run: &str) -> io::Result<()> {
    let noun = access.kind.noun();
    let hidden = || Refusal::error(404, format!("No such {noun}: {}", access.id));
    match owner {
        Owner::Missing => Ok(()),
        Owner::Run(owner) if owner == run => Ok(()),
        Owner::Run(_) => Err(hidden()),
        // Containers (and their execs) are hidden unless the run made them:
        // an unlabelled one is Bosn's own or the engine's.
        Owner::Unlabelled if matches!(access.kind, Kind::Container | Kind::Exec) => Err(hidden()),
        Owner::Unlabelled if !access.write => Ok(()),
        Owner::Unlabelled => Err(Refusal::error(
            403,
            format!(
                "bosn docker proxy refused changing {noun} {}: it was not created by this run",
                access.id
            ),
        )),
    }
}

/// Resolve the object `access` names upstream and refuse the request unless
/// the run may make it.
pub(super) fn check(upstream: &Path, access: &Access, run: &str) -> io::Result<()> {
    let owner = match parse_inspect(access.kind, &inspect(upstream, access.kind, &access.id)?)? {
        Inspected::Owner(owner) => owner,
        Inspected::ExecOf(container) => {
            match parse_inspect(
                Kind::Container,
                &inspect(upstream, Kind::Container, &container)?,
            )? {
                Inspected::Owner(owner) => owner,
                Inspected::ExecOf(_) => unreachable!("a container inspect is never an exec"),
            }
        }
    };
    verdict(access, &owner, run)
}

/// One bounded inspect on a separate upstream connection.
#[cfg(unix)]
fn inspect(upstream: &Path, kind: Kind, id: &str) -> io::Result<Vec<u8>> {
    let mut stream = UnixStream::connect(upstream)?;
    stream.set_read_timeout(Some(INSPECT_DEADLINE))?;
    stream.set_write_timeout(Some(INSPECT_DEADLINE))?;
    let target = kind.inspect_path(id);
    stream.write_all(format!("GET {target} HTTP/1.0\r\nHost: docker\r\n\r\n").as_bytes())?;
    let mut response = Vec::new();
    stream.take(MAX_INSPECT_BYTES).read_to_end(&mut response)?;
    Ok(response)
}

#[cfg(not(unix))]
fn inspect(_upstream: &Path, _kind: Kind, _id: &str) -> io::Result<Vec<u8>> {
    Err(io::Error::other("the Docker proxy needs Unix sockets"))
}

/// The objects of other containers a container create names.
#[derive(Deserialize, Default)]
struct CreateRefs {
    #[serde(rename = "HostConfig", default)]
    host: Option<HostRefs>,
    #[serde(rename = "NetworkingConfig", default)]
    networking: Option<NetworkingRefs>,
}
#[derive(Deserialize, Default)]
struct HostRefs {
    #[serde(rename = "VolumesFrom", default)]
    volumes_from: Option<Vec<String>>,
    #[serde(rename = "Links", default)]
    links: Option<Vec<String>>,
    #[serde(rename = "NetworkMode", default)]
    network_mode: Option<String>,
    #[serde(rename = "PidMode", default)]
    pid_mode: Option<String>,
    #[serde(rename = "IpcMode", default)]
    ipc_mode: Option<String>,
}
#[derive(Deserialize, Default)]
struct NetworkingRefs {
    #[serde(rename = "EndpointsConfig", default)]
    endpoints: Option<BTreeMap<String, serde::de::IgnoredAny>>,
}

/// Network modes that name no network object.
const BUILTIN_NETWORK_MODES: &[&str] = &["", "default", "bridge", "host", "none"];

/// Every object a container create `body` reaches into, as read accesses.
pub(super) fn create_references(body: &[u8]) -> io::Result<Vec<Access>> {
    let refs: CreateRefs = serde_json::from_slice(body)
        .map_err(|e| io::Error::other(format!("container create body: {e}")))?;
    let read = |kind, id: &str| Access {
        kind,
        id: id.to_owned(),
        write: false,
    };
    let mut out = Vec::new();
    let host = refs.host.unwrap_or_default();
    for source in host.volumes_from.unwrap_or_default() {
        // `container[:ro|rw]`
        out.push(read(
            Kind::Container,
            source.split(':').next().unwrap_or(""),
        ));
    }
    for link in host.links.unwrap_or_default() {
        // `name:alias`, Docker also accepts a leading slash.
        let name = link.split(':').next().unwrap_or("").trim_start_matches('/');
        out.push(read(Kind::Container, name));
    }
    for mode in [&host.pid_mode, &host.ipc_mode].into_iter().flatten() {
        if let Some(container) = mode.strip_prefix("container:") {
            out.push(read(Kind::Container, container));
        }
    }
    let mut networks: Vec<String> = refs
        .networking
        .and_then(|n| n.endpoints)
        .map(|e| e.into_keys().collect())
        .unwrap_or_default();
    if let Some(mode) = host.network_mode {
        match mode.strip_prefix("container:") {
            Some(container) => out.push(read(Kind::Container, container)),
            None => networks.push(mode),
        }
    }
    for network in networks {
        if !BUILTIN_NETWORK_MODES.contains(&network.as_str()) {
            out.push(read(Kind::Network, &network));
        }
    }
    out.retain(|access| !access.id.is_empty());
    Ok(out)
}

#[cfg(test)]
pub(super) fn owner_of(kind: Kind, response: &[u8]) -> io::Result<Owner> {
    match parse_inspect(kind, response)? {
        Inspected::Owner(owner) => Ok(owner),
        Inspected::ExecOf(container) => Ok(Owner::Run(format!("exec-of:{container}"))),
    }
}
