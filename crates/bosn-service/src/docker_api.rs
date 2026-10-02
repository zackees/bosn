//! A minimal blocking Docker Engine API client over its Unix socket (#358).
//!
//! Runner teardown and cache-volume preparation issue a handful of small
//! requests per job. Talking to the socket directly costs well under a
//! millisecond per call, where each `docker` CLI process costs tens of
//! milliseconds, so a run with many leftovers is still torn down quickly.
//! Every request uses `Connection: close` and is bounded in time and size.

// The transport is a Unix socket; elsewhere only the types are used.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::{
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Labels Bosn puts on Docker objects that a runner job creates.
pub const LABEL_RUN: &str = "com.zackees.bosn.run";
pub const LABEL_DAEMON: &str = "com.zackees.bosn.daemon";
pub const LABEL_JOB: &str = "com.zackees.bosn.job";
pub const LABEL_SLOT: &str = "com.zackees.bosn.slot";
pub const LABEL_CACHE: &str = "com.zackees.bosn.cache";
pub const LABEL_CACHE_KEY: &str = "com.zackees.bosn.cache-key";

#[derive(Clone, Debug)]
pub struct DockerApi {
    socket: PathBuf,
}

#[derive(Debug)]
pub struct ApiResponse {
    pub status: u16,
    pub body: Vec<u8>,
}
impl ApiResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    pub fn json(&self) -> io::Result<Value> {
        serde_json::from_slice(&self.body).map_err(io::Error::other)
    }
}

/// What a teardown removed, for the job log and the accounting view.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Teardown {
    pub containers: usize,
    pub networks: usize,
    pub volumes: usize,
    pub failures: Vec<String>,
}
impl Teardown {
    pub fn is_empty(&self) -> bool {
        self.containers + self.networks + self.volumes == 0 && self.failures.is_empty()
    }
    pub fn summary(&self) -> String {
        let mut text = format!(
            "removed {} container(s), {} network(s), {} volume(s)",
            self.containers, self.networks, self.volumes
        );
        if !self.failures.is_empty() {
            text.push_str(&format!(
                "; {} failure(s): {}",
                self.failures.len(),
                self.failures.join("; ")
            ));
        }
        text
    }
}

impl DockerApi {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The engine socket the `docker` CLI would use: `DOCKER_HOST` when it is
    /// a `unix://` URL, else the first existing well-known path.
    pub fn from_environment() -> Option<Self> {
        if let Ok(host) = std::env::var("DOCKER_HOST") {
            return host.strip_prefix("unix://").map(Self::new);
        }
        ["/var/run/docker.sock", "/run/docker.sock"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .map(Self::new)
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> io::Result<ApiResponse> {
        #[cfg(unix)]
        {
            let stream = std::os::unix::net::UnixStream::connect(&self.socket)?;
            stream.set_read_timeout(Some(IO_TIMEOUT))?;
            stream.set_write_timeout(Some(IO_TIMEOUT))?;
            exchange(stream, method, path, body)
        }
        #[cfg(not(unix))]
        {
            let _ = (method, path, body);
            Err(io::Error::other(
                "the Docker API client needs a Unix socket",
            ))
        }
    }

    fn get_json(&self, path: &str) -> io::Result<Value> {
        let response = self.request("GET", path, None)?;
        if !response.ok() {
            return Err(io::Error::other(format!(
                "GET {path}: HTTP {}",
                response.status
            )));
        }
        response.json()
    }

    pub fn volume_exists(&self, name: &str) -> io::Result<bool> {
        let response = self.request("GET", &format!("/volumes/{}", encode(name)), None)?;
        match response.status {
            200 => Ok(true),
            404 => Ok(false),
            status => Err(io::Error::other(format!("volume inspect: HTTP {status}"))),
        }
    }

    /// Create a local volume with labels. An existing volume is left as it is.
    pub fn create_volume(&self, name: &str, labels: &[(String, String)]) -> io::Result<()> {
        let labels: serde_json::Map<String, Value> = labels
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let body = serde_json::json!({"Name": name, "Driver": "local", "Labels": labels});
        let response = self.request("POST", "/volumes/create", Some(&body))?;
        if response.ok() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "volume create {name}: HTTP {} {}",
                response.status,
                String::from_utf8_lossy(&response.body).trim()
            )))
        }
    }

    /// Names of objects of `kind` ("containers", "networks", "volumes")
    /// carrying `key=value`.
    pub fn labelled(&self, kind: &str, key: &str, value: Option<&str>) -> io::Result<Vec<String>> {
        let label = value.map_or_else(|| key.to_owned(), |v| format!("{key}={v}"));
        let filters = serde_json::json!({"label": [label]}).to_string();
        let path = match kind {
            "containers" => format!("/containers/json?all=1&filters={}", encode(&filters)),
            "networks" => format!("/networks?filters={}", encode(&filters)),
            "volumes" => format!("/volumes?filters={}", encode(&filters)),
            _ => return Err(io::Error::other("unknown Docker object kind")),
        };
        let listing = self.get_json(&path)?;
        let items = match kind {
            "volumes" => listing.get("Volumes").cloned().unwrap_or(Value::Null),
            _ => listing,
        };
        Ok(items
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let field = if kind == "volumes" { "Name" } else { "Id" };
                        item.get(field).and_then(Value::as_str).map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Labels of every object of `kind` carrying `key` (any value).
    pub fn labelled_with_labels(
        &self,
        kind: &str,
        key: &str,
    ) -> io::Result<Vec<(String, serde_json::Map<String, Value>)>> {
        let filters = serde_json::json!({"label": [key]}).to_string();
        let path = match kind {
            "containers" => format!("/containers/json?all=1&filters={}", encode(&filters)),
            "networks" => format!("/networks?filters={}", encode(&filters)),
            "volumes" => format!("/volumes?filters={}", encode(&filters)),
            _ => return Err(io::Error::other("unknown Docker object kind")),
        };
        let listing = self.get_json(&path)?;
        let items = match kind {
            "volumes" => listing.get("Volumes").cloned().unwrap_or(Value::Null),
            _ => listing,
        };
        Ok(items
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let field = if kind == "volumes" { "Name" } else { "Id" };
                        let id = item.get(field).and_then(Value::as_str)?.to_owned();
                        let labels = item
                            .get("Labels")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        Some((id, labels))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Remove every container, network and volume labelled
    /// `com.zackees.bosn.run=<run>`. Containers go first (with their
    /// anonymous volumes), so the networks and volumes they held are free.
    /// Only objects carrying this exact run label are touched: another job's
    /// objects, and cache volumes (which never carry a run label), survive.
    pub fn teardown_run(&self, run: &str) -> Teardown {
        let mut report = Teardown::default();
        let mut step = |kind: &str, delete: &dyn Fn(&str) -> String| {
            let names = match self.labelled(kind, LABEL_RUN, Some(run)) {
                Ok(names) => names,
                Err(error) => {
                    report.failures.push(format!("list {kind}: {error}"));
                    return;
                }
            };
            for name in names {
                match self.request("DELETE", &delete(&name), None) {
                    Ok(response) if response.ok() || response.status == 404 => match kind {
                        "containers" => report.containers += 1,
                        "networks" => report.networks += 1,
                        _ => report.volumes += 1,
                    },
                    Ok(response) => report.failures.push(format!(
                        "remove {kind} {}: HTTP {} {}",
                        short(&name),
                        response.status,
                        String::from_utf8_lossy(&response.body).trim()
                    )),
                    Err(error) => report
                        .failures
                        .push(format!("remove {kind} {}: {error}", short(&name))),
                }
            }
        };
        step("containers", &|id| {
            format!("/containers/{}?force=1&v=1", encode(id))
        });
        step("networks", &|id| format!("/networks/{}", encode(id)));
        step("volumes", &|name| format!("/volumes/{}", encode(name)));
        report
    }
}

fn short(id: &str) -> &str {
    if id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        &id[..12]
    } else {
        id
    }
}

/// Percent-encode everything outside RFC 3986's unreserved set.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn exchange<S: Read + Write>(
    mut stream: S,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> io::Result<ApiResponse> {
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n");
    if body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", payload.len()));
    stream.write_all(head.as_bytes())?;
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;
    read_response(BufReader::new(stream))
}

pub(crate) fn read_response<R: BufRead>(mut reader: R) -> io::Result<ApiResponse> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::other("malformed HTTP status line"))?;
    let mut length = None;
    let mut chunked = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::other("truncated HTTP head"));
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse::<usize>().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| io::Error::other("malformed chunk size"))?;
            if size == 0 {
                break;
            }
            if body.len() + size > MAX_RESPONSE_BYTES {
                return Err(io::Error::other("Docker API response too large"));
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..])?;
            line.clear();
            reader.read_line(&mut line)?;
        }
    } else if let Some(length) = length {
        if length > MAX_RESPONSE_BYTES {
            return Err(io::Error::other("Docker API response too large"));
        }
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    } else {
        reader
            .take(MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut body)?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(io::Error::other("Docker API response too large"));
        }
    }
    Ok(ApiResponse { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_are_read_by_length_by_chunks_or_to_eof() {
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}trailing";
        let response = read_response(&fixed[..]).unwrap();
        assert_eq!(
            (response.status, response.body.as_slice()),
            (200, &b"{}"[..])
        );
        let chunked = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n";
        let response = read_response(&chunked[..]).unwrap();
        assert_eq!(
            (response.status, response.body.as_slice()),
            (404, &b"abcde"[..])
        );
        let eof = b"HTTP/1.0 204 No Content\r\n\r\nrest";
        assert_eq!(read_response(&eof[..]).unwrap().body, b"rest");
        assert!(read_response(&b"garbage\r\n\r\n"[..]).is_err());
        assert!(read_response(&b"HTTP/1.1 200 OK\r\n"[..]).is_err());
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(
            encode(r#"{"label":["a=b c"]}"#),
            "%7B%22label%22%3A%5B%22a%3Db%20c%22%5D%7D"
        );
        assert_eq!(encode("bosn-cache_1.x~"), "bosn-cache_1.x~");
    }

    #[test]
    fn teardown_summaries_name_failures() {
        let report = Teardown {
            containers: 2,
            networks: 1,
            volumes: 0,
            failures: vec!["remove volumes v: HTTP 409".into()],
        };
        assert!(!report.is_empty());
        assert_eq!(
            report.summary(),
            "removed 2 container(s), 1 network(s), 0 volume(s); 1 failure(s): remove volumes v: HTTP 409"
        );
        assert!(Teardown::default().is_empty());
    }
}
