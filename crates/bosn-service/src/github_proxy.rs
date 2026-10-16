//! Read-only GitHub API proxy for manifest tasks (`github_api = "proxy"`).
//!
//! The daemon starts one proxy per task run, bound to `127.0.0.1` on an
//! ephemeral port, and hands the task only its URL,
//! `http://127.0.0.1:<port>/<nonce>`, as `GITHUB_API_URL`. act passes that
//! URL on to its job containers, which run with `--network host` (act's
//! default), so they reach the same loopback listener directly. The
//! credential stays in daemon memory: it is resolved from `gh auth token`
//! (falling back to the stored `github_token` secret, then to anonymous
//! access), attached to allowlisted upstream reads, and never written to
//! disk, the registry, argv, an environment or a log line.
//!
//! Only `GET`/`HEAD` to a closed set of read endpoints is forwarded; every
//! other request is refused with 403. Responses with an `ETag` are cached and
//! revalidated with `If-None-Match`, which GitHub does not count against the
//! rate limit when it answers 304.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kernal_api::async_engine;
use kernal_api::http;
use kernal_api::http_server;
use kernal_api::{SpawnSpec, StreamMode};

use crate::github_ci_policy::{CiPolicy, Decision};
use crate::secrets;

/// The only upstream the production proxy talks to.
pub const GITHUB_API_UPSTREAM: &str = "https://api.github.com";

const USER_AGENT: &str = "bosn-github-api-proxy";
const MAX_UPSTREAM_BODY: u64 = 64 * 1024 * 1024;
const MAX_CACHED_BODY: usize = 4 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 1024;
const MAX_LOGGED_PATH: usize = 200;

/// Where the proxy's credential came from. Only this label is ever reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialSource {
    GhCli,
    StoredSecret,
    Anonymous,
}

impl CredentialSource {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::GhCli => "`gh auth token` on the host",
            Self::StoredSecret => "the stored github_token secret",
            Self::Anonymous => "none (anonymous, 60 requests/hour per IP)",
        }
    }
}

/// A resolved credential. `Debug` never prints the value.
#[derive(Clone)]
pub struct GithubCredential {
    token: Option<String>,
    source: CredentialSource,
}

impl fmt::Debug for GithubCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubCredential")
            .field("source", &self.source)
            .field("token", &self.token.as_ref().map(|_| "***"))
            .finish()
    }
}

impl GithubCredential {
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            token: None,
            source: CredentialSource::Anonymous,
        }
    }

    #[must_use]
    pub fn source(&self) -> CredentialSource {
        self.source
    }

    /// The value, for the output masker only.
    #[must_use]
    pub fn secret_value(&self) -> Option<&str> {
        self.token.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn for_test(token: &str) -> Self {
        Self {
            token: Some(token.to_owned()),
            source: CredentialSource::StoredSecret,
        }
    }

    /// A short, non-reversible identity so cached responses made with one
    /// credential are never served to a proxy holding another.
    fn fingerprint(&self) -> String {
        match &self.token {
            None => "anonymous".into(),
            Some(token) => {
                let digest = kernal_api::hash::sha256_bytes(token.as_bytes());
                digest.to_hex()[..24].to_owned()
            }
        }
    }
}

/// A token as printed by `gh auth token` or stored by `bosn secret set`:
/// one line of URL-safe characters. Anything else is not used.
fn plausible_token(raw: &str) -> Option<String> {
    let token = raw.trim();
    let ok = (20..=255).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
    ok.then(|| token.to_owned())
}

/// Resolve the proxy credential: a fresh `gh auth token`, else the stored
/// `github_token` secret, else anonymous. `gh`'s output is captured through
/// a pipe and never logged; a failing `gh` only moves to the next source. A
/// stored secret that exists but is refused (symlink, loose mode) fails, as
/// it does for `secrets = ["github_token"]`.
pub async fn resolve_credential(
    state_dir: &Path,
    gh_program: &OsString,
) -> Result<GithubCredential, String> {
    let spec = SpawnSpec::new(gh_program.as_os_str())
        .arg("auth")
        .arg("token")
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Piped)
        .create_process_group(true)
        .kill_when_owner_dies(true);
    if let Ok(output) =
        kernal_api::run_bounded_command_async(spec, Duration::from_secs(15), 4096).await
        && output.exit.raw_code() == 0
        && let Some(token) = std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(plausible_token)
    {
        return Ok(GithubCredential {
            token: Some(token),
            source: CredentialSource::GhCli,
        });
    }
    match secrets::read_secret(state_dir, "github_token")? {
        Some(value) => Ok(GithubCredential {
            token: plausible_token(&value),
            source: if plausible_token(&value).is_some() {
                CredentialSource::StoredSecret
            } else {
                CredentialSource::Anonymous
            },
        }),
        None => Ok(GithubCredential::anonymous()),
    }
}

/// Decide whether a proxied request may be forwarded. `path` is the encoded
/// path after the nonce prefix. Returns the refusal reason otherwise.
pub fn check_request(method: &str, path: &str) -> Result<(), &'static str> {
    if method != "GET" && method != "HEAD" {
        return Err("only GET and HEAD are forwarded; this proxy is read-only");
    }
    canonical_path(path)?;
    let segments: Vec<&str> = path[1..].split('/').collect();
    match segments.as_slice() {
        ["rate_limit"] => Ok(()),
        ["repos", owner, repo, rest @ ..] if !owner.is_empty() && !repo.is_empty() => match rest {
            [] => Ok(()),
            ["releases", ..]
            | ["tags"]
            | ["branches", ..]
            | ["commits", ..]
            | ["contents", ..]
            | ["zipball", ..]
            | ["tarball", ..]
            | [
                "git",
                "ref" | "refs" | "matching-refs" | "trees" | "blobs" | "commits" | "tags",
                ..,
            ] => Ok(()),
            _ => Err("endpoint is not on the read-only allowlist"),
        },
        _ => Err("endpoint is not on the read-only allowlist"),
    }
}

/// Refuse a path that is not a plain canonical API path: no `..`, `//`,
/// backslashes, controls or encoded `.` `/` `\`.
pub fn canonical_path(path: &str) -> Result<(), &'static str> {
    if !path.starts_with('/')
        || path.contains("//")
        || path.contains('\\')
        || path.bytes().any(|b| b.is_ascii_control() || b == b' ')
        || path.split('/').any(|seg| seg == "." || seg == "..")
        || path.to_ascii_lowercase().contains("%2e")
        || path.to_ascii_lowercase().contains("%2f")
        || path.to_ascii_lowercase().contains("%5c")
    {
        return Err("path is not a canonical API path");
    }
    Ok(())
}

#[derive(Clone)]
struct CachedResponse {
    etag: String,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

/// Bounded ETag cache, shared by every proxy the daemon starts.
#[derive(Default)]
pub struct ResponseCache {
    inner: Mutex<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    entries: BTreeMap<String, CachedResponse>,
    order: VecDeque<String>,
    bytes: usize,
}

impl ResponseCache {
    fn get(&self, key: &str) -> Option<CachedResponse> {
        self.inner.lock().ok()?.entries.get(key).cloned()
    }

    fn put(&self, key: String, value: CachedResponse) {
        if value.body.len() > MAX_CACHED_BODY {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(old) = inner.entries.remove(&key) {
            inner.bytes -= old.body.len();
            inner.order.retain(|k| k != &key);
        }
        while inner.bytes + value.body.len() > MAX_CACHE_BYTES
            || inner.entries.len() >= MAX_CACHE_ENTRIES
        {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some(old) = inner.entries.remove(&oldest) {
                inner.bytes -= old.body.len();
            }
        }
        inner.bytes += value.body.len();
        inner.order.push_back(key.clone());
        inner.entries.insert(key, value);
    }
}

/// Response headers relayed from GitHub; everything else is dropped.
const RELAYED_HEADERS: &[&str] = &[
    "content-type",
    "etag",
    "last-modified",
    "link",
    "location",
    "cache-control",
    "x-github-media-type",
    "x-github-request-id",
    "x-ratelimit-limit",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
    "x-ratelimit-used",
    "x-ratelimit-resource",
];

struct ProxyState {
    nonce: String,
    base_url: String,
    upstream: String,
    credential: GithubCredential,
    fingerprint: String,
    cache: Arc<ResponseCache>,
    client: http::Client,
    logs: Option<async_engine::Sender<String>>,
    /// Set for a `bosn ci run --github-proxy` run (#650): the fake token,
    /// the run's repository and its writes mode.
    policy: Option<CiPolicy>,
}

/// A running proxy. Dropping it stops the listener and every connection.
pub struct GithubApiProxy {
    url: String,
    addr: SocketAddr,
    nonce: String,
    _server: async_engine::Task<io::Result<()>>,
}

impl fmt::Debug for GithubApiProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubApiProxy").finish_non_exhaustive()
    }
}

impl GithubApiProxy {
    /// Start a proxy on `127.0.0.1:0`. `logs`, when given, receives one line
    /// per request (method, path without query, status, cache outcome).
    pub async fn start(
        upstream: &str,
        credential: GithubCredential,
        cache: Arc<ResponseCache>,
        logs: Option<async_engine::Sender<String>>,
    ) -> io::Result<Self> {
        Self::start_with_policy(upstream, credential, cache, logs, None).await
    }

    /// [`Self::start`] for a CI run: only requests carrying the run's fake
    /// token pass, and `policy` decides reads, GraphQL queries and writes.
    pub async fn start_with_policy(
        upstream: &str,
        credential: GithubCredential,
        cache: Arc<ResponseCache>,
        logs: Option<async_engine::Sender<String>>,
        policy: Option<CiPolicy>,
    ) -> io::Result<Self> {
        let nonce_bytes = kernal_api::random::SecureRandom::new(1, Duration::from_secs(5))
            .map_err(|_| io::Error::other("secure random unavailable"))?
            .bytes(24)
            .await
            .map_err(|_| io::Error::other("secure random unavailable"))?;
        let nonce: String = nonce_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let limits = http_server::Limits {
            max_connections: 64,
            // Bodies are never forwarded; accepting a small one lets a write get
            // the explicit 403 below instead of a bare 413.
            max_request_body_bytes: 1024 * 1024,
            max_response_body_bytes: MAX_UPSTREAM_BODY as usize,
            handler_timeout: Duration::from_secs(180),
            write_timeout: Duration::from_secs(60),
            connection_timeout: Duration::from_secs(600),
            ..http_server::Limits::default()
        };
        let server = http_server::Server::bind(
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            limits,
        )
        .await?;
        let addr = server.local_addr()?;
        let port = addr.port();
        let base_url = format!("http://127.0.0.1:{port}/{nonce}");
        let client = http::Client::new(http::Limits {
            max_redirects: 0,
            max_body_bytes: MAX_UPSTREAM_BODY,
            total_timeout: Duration::from_secs(170),
            ..http::Limits::default()
        })?;
        let fingerprint = credential.fingerprint();
        let state = Arc::new(ProxyState {
            nonce: nonce.clone(),
            base_url: base_url.clone(),
            upstream: upstream.trim_end_matches('/').to_owned(),
            credential,
            fingerprint,
            cache,
            client,
            logs,
            policy,
        });
        let task = async_engine::launch(server.serve(move |request| {
            let state = state.clone();
            async move { handle(&state, request).await }
        }));
        Ok(Self {
            url: base_url,
            addr,
            nonce,
            _server: task,
        })
    }

    /// The value for `GITHUB_API_URL`. It is a capability (the nonce), not a
    /// credential: it grants allowlisted reads only while the task runs.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The loopback listener and its `/<nonce>` path prefix, for the CONNECT
    /// front that relays intercepted requests here.
    #[must_use]
    pub fn listener(&self) -> (SocketAddr, String) {
        (self.addr, format!("/{}", self.nonce))
    }
}

fn json_error(status: u16, message: &str) -> http_server::Response {
    let body = serde_json::json!({
        "message": format!("bosn github api proxy: {message}"),
        "documentation_url": "https://github.com/zackees/bosn/blob/main/docs/github-api-proxy.md",
    })
    .to_string();
    http_server::Response::new(status, body)
        .and_then(|r| r.with_header("content-type", "application/json; charset=utf-8"))
        .unwrap_or_else(|_| http_server::Response::new(500, Vec::new()).expect("500 is valid"))
}

fn header_str(request: &http_server::Request, name: &str) -> Option<String> {
    request
        .header(name)
        .and_then(|v| std::str::from_utf8(v).ok())
        .map(str::to_owned)
}

fn log(state: &ProxyState, method: &str, path: &str, outcome: &str) {
    if let Some(logs) = &state.logs {
        let path: String = path.chars().take(MAX_LOGGED_PATH).collect();
        let _ = logs.try_send(format!("[github-api-proxy] {method} {path} -> {outcome}"));
    }
}

#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
async fn handle(state: &ProxyState, request: http_server::Request) -> http_server::Response {
    let method = request.method().to_owned();
    let raw_path = request.path();
    let Some(rest) = raw_path
        .strip_prefix('/')
        .and_then(|p| p.strip_prefix(state.nonce.as_str()))
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
    else {
        return json_error(404, "not found");
    };
    let path = if rest.is_empty() { "/" } else { rest }.to_owned();
    if let Some(policy) = &state.policy {
        let authorization = header_str(&request, "authorization");
        if let Err(reason) = policy.authorize(authorization.as_deref()) {
            log(state, &method, &path, "401 refused (token)");
            return json_error(401, reason);
        }
        match policy.decide(&method, &path, request.query(), request.body()) {
            Decision::Refuse(status, reason) => {
                log(state, &method, &path, &format!("{status} refused"));
                return json_error(status, reason);
            }
            Decision::DryRun => {
                log(state, &method, &path, "204 dry-run (not forwarded)");
                return http_server::Response::new(204, Vec::new())
                    .and_then(|r| r.with_header("x-bosn-dry-run", "1"))
                    .unwrap_or_else(|_| json_error(500, "response"));
            }
            Decision::Forward if method == "POST" => {
                return forward_post(state, &request, &path).await;
            }
            Decision::Forward if method == "DELETE" => {
                return forward_delete(state, &request, &path).await;
            }
            Decision::Forward => {}
        }
    } else if let Err(reason) = check_request(&method, &path) {
        log(state, &method, &path, "403 refused");
        return json_error(403, reason);
    }
    let mut url = format!("{}{}", state.upstream, path);
    if let Some(query) = request.query() {
        url.push('?');
        url.push_str(query);
    }
    let accept =
        header_str(&request, "accept").unwrap_or_else(|| "application/vnd.github+json".into());
    let api_version = header_str(&request, "x-github-api-version");
    let client_inm = header_str(&request, "if-none-match");
    let client_ims = header_str(&request, "if-modified-since");
    let cache_key = format!(
        "{}\n{}\n{}\n{}",
        state.fingerprint,
        accept,
        api_version.as_deref().unwrap_or(""),
        url
    );
    let cacheable = method == "GET" && client_inm.is_none() && client_ims.is_none();
    let cached = if cacheable {
        state.cache.get(&cache_key)
    } else {
        None
    };
    let authorization = state
        .credential
        .token
        .as_ref()
        .map(|token| format!("Bearer {token}"));
    let mut headers: Vec<(&str, &str)> =
        vec![("accept", accept.as_str()), ("user-agent", USER_AGENT)];
    if let Some(v) = &api_version {
        headers.push(("x-github-api-version", v.as_str()));
    }
    if let Some(auth) = &authorization {
        headers.push(("authorization", auth.as_str()));
    }
    if let Some(entry) = &cached {
        headers.push(("if-none-match", entry.etag.as_str()));
    } else {
        if let Some(v) = &client_inm {
            headers.push(("if-none-match", v.as_str()));
        }
        if let Some(v) = &client_ims {
            headers.push(("if-modified-since", v.as_str()));
        }
    }
    let upstream = http::Request {
        method: if method == "HEAD" {
            http::Method::Head
        } else {
            http::Method::Get
        },
        url: &url,
        headers: &headers,
        body: &[],
    };
    let response = match state.client.execute(upstream).await {
        Ok(response) => response,
        Err(error) => {
            // kernal-api strips URLs from transport errors; report only the kind.
            log(state, &method, &path, "502 upstream unreachable");
            return json_error(
                502,
                &format!("upstream request failed ({:?})", error.kind()),
            );
        }
    };
    let status = response.status();
    if status == 304
        && let Some(entry) = cached
    {
        log(state, &method, &path, "200 (etag revalidated, cache hit)");
        return build_response(state, 200, &entry.headers, entry.body, "hit");
    }
    let relayed = relayed_headers(&response);
    let etag = response
        .header("etag")
        .and_then(|v| std::str::from_utf8(v).ok())
        .map(str::to_owned);
    let body = if method == "HEAD" || matches!(status, 204 | 205 | 304) {
        Vec::new()
    } else {
        match response.into_bytes().await {
            Ok(body) => body,
            Err(error) => {
                log(state, &method, &path, "502 upstream body failed");
                return json_error(502, &format!("upstream body failed ({:?})", error.kind()));
            }
        }
    };
    let outcome = match etag {
        Some(etag) if cacheable && status == 200 => {
            state.cache.put(
                cache_key,
                CachedResponse {
                    etag,
                    headers: relayed.clone(),
                    body: body.clone(),
                },
            );
            "miss"
        }
        _ => "bypass",
    };
    log(
        state,
        &method,
        &path,
        &format!("{status} forwarded (cache {outcome})"),
    );
    build_response(state, status, &relayed, body, outcome)
}

fn build_response(
    state: &ProxyState,
    status: u16,
    headers: &[(&'static str, String)],
    body: Vec<u8>,
    cache: &str,
) -> http_server::Response {
    let status = if (200..=599).contains(&status) {
        status
    } else {
        502
    };
    let Ok(mut response) = http_server::Response::new(status, body) else {
        return json_error(502, "upstream returned an unrelayable response");
    };
    for (name, value) in headers {
        // Pagination links point back through this proxy.
        let value = if *name == "link" {
            value.replace(&state.upstream, &state.base_url)
        } else {
            value.clone()
        };
        if let Ok(next) = response.with_header(name, &value) {
            response = next;
        } else {
            return json_error(502, "upstream returned an unrelayable header");
        }
    }
    response
        .with_header("x-bosn-cache", cache)
        .unwrap_or_else(|_| json_error(500, "header"))
}

/// The client headers a forwarded GraphQL query or write keeps; the
/// credential is always the daemon's, never the client's.
fn upstream_headers(state: &ProxyState, request: &http_server::Request) -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "accept".to_owned(),
            header_str(request, "accept").unwrap_or_else(|| "application/vnd.github+json".into()),
        ),
        ("user-agent".to_owned(), USER_AGENT.to_owned()),
    ];
    if let Some(v) = header_str(request, "x-github-api-version") {
        headers.push(("x-github-api-version".into(), v));
    }
    if let Some(token) = &state.credential.token {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    }
    headers
}

/// Forward an allowlisted `POST /graphql` query with its body.
async fn forward_post(
    state: &ProxyState,
    request: &http_server::Request,
    path: &str,
) -> http_server::Response {
    let url = format!("{}{}", state.upstream, path);
    let mut owned = upstream_headers(state, request);
    owned.push(("content-type".into(), "application/json".into()));
    let headers: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let upstream = http::Request {
        method: http::Method::Post,
        url: &url,
        headers: &headers,
        body: request.body(),
    };
    let response = match state.client.execute(upstream).await {
        Ok(response) => response,
        Err(error) => {
            log(state, "POST", path, "502 upstream unreachable");
            return json_error(
                502,
                &format!("upstream request failed ({:?})", error.kind()),
            );
        }
    };
    let status = response.status();
    let relayed = relayed_headers(&response);
    let body = match response.into_bytes().await {
        Ok(body) => body,
        Err(error) => {
            log(state, "POST", path, "502 upstream body failed");
            return json_error(502, &format!("upstream body failed ({:?})", error.kind()));
        }
    };
    log(state, "POST", path, &format!("{status} forwarded"));
    build_response(state, status, &relayed, body, "bypass")
}

fn relayed_headers(response: &http::Response) -> Vec<(&'static str, String)> {
    RELAYED_HEADERS
        .iter()
        .filter_map(|name| {
            response
                .header(name)
                .and_then(|v| std::str::from_utf8(v).ok())
                .map(|v| (*name, v.to_owned()))
        })
        .collect()
}

/// Forward an allowlisted write (`--github-writes=on`). kernal-api's client
/// has no DELETE, so the CONNECT front's blocking client sends it.
async fn forward_delete(
    state: &ProxyState,
    request: &http_server::Request,
    path: &str,
) -> http_server::Response {
    let mut url = format!("{}{}", state.upstream, path);
    if let Some(query) = request.query() {
        url.push('?');
        url.push_str(query);
    }
    let headers = upstream_headers(state, request);
    #[cfg(target_os = "linux")]
    let sent =
        async_engine::launch_blocking(move || crate::github_connect::send_delete(&url, &headers))
            .await
            .map_err(|_| io::Error::other("write task failed"))
            .and_then(|r| r);
    #[cfg(not(target_os = "linux"))]
    let sent: io::Result<crate::github_http1::Reply> = {
        let _ = (url, headers);
        Err(io::Error::other("forwarded writes need a Linux host"))
    };
    match sent {
        Ok((status, headers, body)) => {
            log(
                state,
                "DELETE",
                path,
                &format!("{status} forwarded (write)"),
            );
            let relayed: Vec<(&'static str, String)> = RELAYED_HEADERS
                .iter()
                .filter_map(|name| {
                    headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(name))
                        .map(|(_, v)| (*name, v.clone()))
                })
                .collect();
            build_response(state, status, &relayed, body, "bypass")
        }
        Err(_) => {
            log(state, "DELETE", path, "502 upstream unreachable");
            json_error(502, "upstream write failed")
        }
    }
}

#[cfg(test)]
#[path = "github_proxy_tests.rs"]
mod tests;
