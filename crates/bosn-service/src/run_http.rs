//! Owner-authenticated loopback access to durable task output.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kernal_api::{async_engine, http_server};
use serde_json::json;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::raw_run_log::{self, RawChunk};

pub struct RunHttpServer {
    pub local_addr: SocketAddr,
    _task: async_engine::Task<io::Result<()>>,
}

const MAX_PAGE_BYTES: u64 = 4 * 1024 * 1024;

pub async fn start(state_dir: &Path) -> io::Result<RunHttpServer> {
    let token = random_token().await?;
    let limits = http_server::Limits {
        max_connections: 8,
        max_request_body_bytes: 0,
        max_response_body_bytes: 96 * 1024 * 1024,
        handler_timeout: Duration::from_secs(30),
        ..http_server::Limits::default()
    };
    let server = http_server::Server::bind(
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
        limits,
    )
    .await?
    .with_response_header("x-content-type-options", "nosniff")?
    .with_response_header("cache-control", "no-store")?;
    let local_addr = server.local_addr()?;
    write_private(&state_dir.join("run-http.token"), token.as_bytes())?;
    write_private(
        &state_dir.join("run-http.url"),
        format!("http://127.0.0.1:{}\n", local_addr.port()).as_bytes(),
    )?;
    let state_dir = state_dir.to_path_buf();
    let task = async_engine::launch(server.serve(move |request| {
        let state_dir = state_dir.clone();
        let token = token.clone();
        async move {
            async_engine::launch_blocking(move || {
                respond(&state_dir, &token, local_addr.port(), request)
            })
            .await
            .unwrap_or_default()
        }
    }));
    Ok(RunHttpServer {
        local_addr,
        _task: task,
    })
}

async fn random_token() -> io::Result<String> {
    let bytes = kernal_api::random::SecureRandom::new(1, Duration::from_secs(3))
        .map_err(io::Error::other)?
        .bytes(32)
        .await
        .map_err(io::Error::other)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let filename = path
        .file_name()
        .ok_or_else(|| io::Error::other("missing file name"))?;
    let next = path.with_file_name(format!(
        ".{}.{}.{}.next",
        filename.to_string_lossy(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let mut file = options.open(&next)?;
        std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let mut file = options.open(&next)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    kernal_api::platform::fs::replacement::atomic_replace(&next, path)
}

fn response(status: u16, content_type: &str, body: Vec<u8>) -> http_server::Response {
    http_server::Response::new(status, body)
        .and_then(|r| r.with_header("content-type", content_type))
        .unwrap_or_default()
}

fn text(status: u16, message: &str) -> http_server::Response {
    response(
        status,
        "text/plain; charset=utf-8",
        message.as_bytes().to_vec(),
    )
}

fn header<'a>(request: &'a http_server::Request, name: &str) -> Option<&'a str> {
    request
        .header(name)
        .and_then(|raw| std::str::from_utf8(raw).ok())
}

fn authorized(
    token: &str,
    port: u16,
    request: &http_server::Request,
    query: &[(String, String)],
) -> bool {
    let host = header(request, "host");
    let host_ok = host.is_some_and(|host| {
        host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
    });
    let origin_ok = header(request, "origin").is_none_or(|origin| {
        origin == format!("http://127.0.0.1:{port}") || origin == format!("http://localhost:{port}")
    });
    if !host_ok || !origin_ok || request.method() != "GET" {
        return false;
    }
    let bearer_ok = header(request, "authorization") == Some(&format!("Bearer {token}"));
    let query_tokens: Vec<_> = query.iter().filter(|(key, _)| key == "token").collect();
    bearer_ok || (query_tokens.len() == 1 && query_tokens[0].1 == token)
}

fn respond(
    state_dir: &Path,
    token: &str,
    port: u16,
    request: http_server::Request,
) -> http_server::Response {
    let query: Result<Vec<(String, String)>, _> = request.query_pairs().collect();
    let Ok(query) = query else {
        return text(400, "invalid query");
    };
    if !authorized(token, port, &request, &query) {
        return text(401, "unauthorized");
    }
    let query: Vec<_> = query
        .into_iter()
        .filter(|(key, _)| key != "token")
        .collect();
    let path = request.path();
    if path == "/v1/runs" && query.is_empty() {
        return match raw_run_log::list_runs(state_dir) {
            Ok(runs) => response(
                200,
                "application/json",
                serde_json::to_vec(&runs).unwrap_or_default(),
            ),
            Err(_) => text(500, "run listing failed"),
        };
    }
    let Some(tail) = path.strip_prefix("/v1/runs/") else {
        return text(404, "not found");
    };
    let Some((run_id, operation)) = tail.split_once('/') else {
        return text(404, "not found");
    };
    if !crate::ci::wire::valid_uuid(run_id) {
        return text(404, "not found");
    }
    if operation == "stream" {
        return stream_page(state_dir, run_id, &query, header(&request, "accept"));
    }
    if let Some(channel) = operation.strip_prefix("logs/") {
        return raw_page(
            state_dir,
            run_id,
            channel,
            &query,
            header(&request, "range"),
        );
    }
    text(404, "not found")
}

fn stream_page(
    state_dir: &Path,
    run_id: &str,
    query: &[(String, String)],
    accept: Option<&str>,
) -> http_server::Response {
    let mut from_seq = 0;
    let mut streams = "stdout,stderr";
    let mut saw_seq = false;
    let mut saw_streams = false;
    for (key, value) in query {
        match key.as_str() {
            "from_seq" if !saw_seq => match value.parse::<u64>() {
                Ok(value) => from_seq = value,
                Err(_) => return text(400, "invalid from_seq"),
            },
            "streams"
                if !saw_streams
                    && matches!(value.as_str(), "stdout" | "stderr" | "stdout,stderr") =>
            {
                streams = value;
            }
            _ => return text(400, "invalid query"),
        }
        saw_seq |= key == "from_seq";
        saw_streams |= key == "streams";
    }
    let chunks = match raw_run_log::read_since(state_dir, run_id, from_seq, 256) {
        Ok(chunks) => chunks,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return text(404, "run not found"),
        Err(_) => return text(500, "run replay failed"),
    };
    let binary = accept.is_some_and(|value| value.contains("application/vnd.bosn.stream"));
    if !binary
        && accept.is_some_and(|value| !value.contains("application/x-ndjson") && value != "*/*")
    {
        return text(406, "unsupported Accept");
    }
    let mut body = Vec::new();
    let next_seq = chunks.last().map_or(from_seq, |chunk| chunk.index.seq);
    for chunk in chunks {
        if streams == "stdout,stderr" || streams == chunk.index.stream {
            if binary {
                let Ok(data_len) = u32::try_from(chunk.bytes.len()) else {
                    return text(500, "run encoding failed");
                };
                body.push(if chunk.index.stream == "stdout" { 1 } else { 2 });
                body.extend_from_slice(&[0; 3]);
                body.extend_from_slice(&data_len.to_be_bytes());
                body.extend_from_slice(&chunk.index.seq.to_be_bytes());
                body.extend_from_slice(&chunk.bytes);
            } else {
                let event = chunk_json(&chunk);
                if serde_json::to_writer(&mut body, &event).is_err() {
                    return text(500, "run encoding failed");
                }
                body.push(b'\n');
            }
        }
    }
    response(
        200,
        if binary {
            "application/vnd.bosn.stream"
        } else {
            "application/x-ndjson"
        },
        body,
    )
    .with_header("x-next-seq", &next_seq.to_string())
    .unwrap_or_default()
}

fn chunk_json(chunk: &RawChunk) -> serde_json::Value {
    let mut value = chunk_metadata(chunk);
    value["data_b64"] = json!(STANDARD.encode(&chunk.bytes));
    value
}

fn chunk_metadata(chunk: &RawChunk) -> serde_json::Value {
    let timestamp =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(chunk.index.ts_unix_ms) * 1_000_000)
            .ok()
            .and_then(|time| time.format(&Rfc3339).ok())
            .unwrap_or_default();
    json!({
        "seq": chunk.index.seq,
        "ts": timestamp,
        "stream": chunk.index.stream,
        "job_id": chunk.index.job_id,
        "step_id": chunk.index.step_id,
    })
}

fn raw_page(
    state_dir: &Path,
    run_id: &str,
    channel: &str,
    query: &[(String, String)],
    range: Option<&str>,
) -> http_server::Response {
    if !matches!(channel, "stdout" | "stderr") {
        return text(404, "not found");
    }
    if query.len() > 1 || query.first().is_some_and(|(key, _)| key != "offset") {
        return text(400, "invalid query");
    }
    if range.is_some() && !query.is_empty() {
        return text(400, "offset and Range are mutually exclusive");
    }
    let path: PathBuf = state_dir
        .join("runs")
        .join(run_id)
        .join(format!("{channel}.log"));
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return text(404, "run not found"),
        Err(_) => return text(500, "run read failed"),
    };
    let Ok(size) = file.metadata().map(|metadata| metadata.len()) else {
        return text(500, "run read failed");
    };
    let selection = match (range, query.first()) {
        (Some(range), _) => parse_range(range, size),
        (None, Some((_, value))) => value.parse::<u64>().ok().map(|start| (start, size)),
        (None, None) => Some((0, size)),
    };
    let Some((offset, requested_end)) = selection else {
        return text(400, "invalid Range or offset");
    };
    if offset > size || (range.is_some() && offset >= size) {
        return text(416, "offset beyond end")
            .with_header("content-range", &format!("bytes */{size}"))
            .unwrap_or_default();
    }
    if offset == size {
        return response(200, "application/octet-stream", Vec::new());
    }
    let len = requested_end.saturating_sub(offset).min(MAX_PAGE_BYTES) as usize;
    let mut body = vec![0; len];
    if file.seek(SeekFrom::Start(offset)).is_err() || file.read_exact(&mut body).is_err() {
        return text(500, "run read failed");
    }
    let end = offset.saturating_add(len as u64).saturating_sub(1);
    response(206, "application/octet-stream", body)
        .with_header("content-range", &format!("bytes {offset}-{end}/{size}"))
        .and_then(|response| response.with_header("accept-ranges", "bytes"))
        .unwrap_or_default()
}

/// End is exclusive. A single standard byte range is accepted; every
/// response remains capped at four MiB even when the requested range is wider.
fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 {
            return None;
        }
        return Some((size.saturating_sub(suffix), size));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        size
    } else {
        end.parse::<u64>().ok()?.saturating_add(1).min(size)
    };
    (end > start).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosn_engine::EngineEvent;
    use std::net::TcpStream;

    #[test]
    fn one_byte_range_and_suffix_range_select_exact_offsets() {
        assert_eq!(parse_range("bytes=2-4", 10), Some((2, 5)));
        assert_eq!(parse_range("bytes=2-", 10), Some((2, 10)));
        assert_eq!(parse_range("bytes=-3", 10), Some((7, 10)));
        assert_eq!(parse_range("bytes=0-0", 10), Some((0, 1)));
        assert_eq!(parse_range("bytes=-0", 10), None);
        assert_eq!(parse_range("bytes=1-0", 10), None);
    }

    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(future)
    }

    async fn request(port: u16, path: &str, host: &str, auth: &str, origin: &str) -> Vec<u8> {
        let raw = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\n{auth}{origin}Connection: close\r\n\r\n"
        );
        async_engine::launch_blocking(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            let mut reply = Vec::new();
            stream.read_to_end(&mut reply).unwrap();
            reply
        })
        .await
        .unwrap()
    }

    fn body(reply: &[u8]) -> &[u8] {
        let split = reply
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        &reply[split + 4..]
    }

    async fn assert_binary_and_raw(
        port: u16,
        run_id: &str,
        host: &str,
        auth: &str,
        token: &str,
        route: &str,
    ) {
        let binary = request(
            port,
            route,
            host,
            &format!("{auth}Accept: application/vnd.bosn.stream\r\n"),
            "",
        )
        .await;
        assert!(binary.starts_with(b"HTTP/1.1 200"));
        let frames = body(&binary);
        assert_eq!(
            &frames[..16],
            &[1, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        assert_eq!(&frames[16..18], &[0xff, b'a']);
        assert_eq!(
            &frames[18..34],
            &[2, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 2]
        );
        assert_eq!(&frames[34..], &[0xfe, b'b']);
        let stdout = request(
            port,
            &format!("/v1/runs/{run_id}/logs/stdout?offset=0"),
            host,
            auth,
            "",
        )
        .await;
        assert!(stdout.starts_with(b"HTTP/1.1 206"));
        assert_eq!(body(&stdout), [0xff, b'a']);
        let stderr = request(
            port,
            &format!("/v1/runs/{run_id}/logs/stderr?token={token}"),
            host,
            "",
            "",
        )
        .await;
        assert!(stderr.starts_with(b"HTTP/1.1 206"));
        assert_eq!(body(&stderr), [0xfe, b'b']);
    }

    #[test]
    fn authenticated_replay_preserves_raw_channels_and_rejects_bad_host() {
        run(async {
            let tmp = tempfile::tempdir().unwrap();
            let run_id = "00000000-0000-0000-0000-000000000042";
            let mut log = raw_run_log::RawRunLog::create(tmp.path(), run_id).unwrap();
            log.write_metadata(run_id, 123).unwrap();
            log.append(&EngineEvent::Stdout(vec![0xff, b'a'])).unwrap();
            log.append(&EngineEvent::Stderr(vec![0xfe, b'b'])).unwrap();
            drop(log);
            let server = start(tmp.path()).await.unwrap();
            assert!(server.local_addr.ip().is_loopback());
            let port = server.local_addr.port();
            let token = std::fs::read_to_string(tmp.path().join("run-http.token")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for name in ["run-http.token", "run-http.url"] {
                    assert_eq!(
                        std::fs::metadata(tmp.path().join(name))
                            .unwrap()
                            .permissions()
                            .mode()
                            & 0o777,
                        0o600
                    );
                }
            }
            let host = format!("127.0.0.1:{port}");
            let auth = format!("Authorization: Bearer {token}\r\n");
            let route = format!("/v1/runs/{run_id}/stream?from_seq=0");
            let denied = request(port, &route, &host, "", "").await;
            assert!(denied.starts_with(b"HTTP/1.1 401"));
            let rebinding = request(port, &route, "evil.example", &auth, "").await;
            assert!(rebinding.starts_with(b"HTTP/1.1 401"));
            let cross_origin = request(
                port,
                &route,
                &host,
                &auth,
                "Origin: http://evil.example\r\n",
            )
            .await;
            assert!(cross_origin.starts_with(b"HTTP/1.1 401"));
            let listed = request(port, "/v1/runs", &host, &auth, "").await;
            assert!(listed.starts_with(b"HTTP/1.1 200"));
            let runs: serde_json::Value = serde_json::from_slice(body(&listed)).unwrap();
            assert_eq!(runs[0]["job_id"], 123);
            let replay = request(
                port,
                &route,
                &host,
                &format!("{auth}Accept: application/x-ndjson\r\n"),
                "",
            )
            .await;
            assert!(replay.starts_with(b"HTTP/1.1 200"));
            let rows: Vec<serde_json::Value> = body(&replay)
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).unwrap())
                .collect();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0]["seq"], 1);
            assert_eq!(rows[0]["stream"], "stdout");
            assert_eq!(rows[0]["data_b64"], "/2E=");
            assert_eq!(rows[1]["stream"], "stderr");
            assert_eq!(rows[1]["data_b64"], "/mI=");
            assert_binary_and_raw(port, run_id, &host, &auth, &token, &route).await;
            drop(server);
            let restarted = start(tmp.path()).await.unwrap();
            let new_token = std::fs::read_to_string(tmp.path().join("run-http.token")).unwrap();
            assert_ne!(new_token, token);
            let resumed = request(
                restarted.local_addr.port(),
                &format!("/v1/runs/{run_id}/stream?from_seq=1"),
                &format!("127.0.0.1:{}", restarted.local_addr.port()),
                &format!("Authorization: Bearer {new_token}\r\nAccept: application/x-ndjson\r\n"),
                "",
            )
            .await;
            let rows: Vec<serde_json::Value> = body(&resumed)
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).unwrap())
                .collect();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["seq"], 2);
        });
    }

    #[test]
    fn filtered_page_exposes_cursor_even_when_its_body_is_empty() {
        run(async {
            let tmp = tempfile::tempdir().unwrap();
            let run_id = "00000000-0000-0000-0000-000000000044";
            let mut log = raw_run_log::RawRunLog::create(tmp.path(), run_id).unwrap();
            log.write_metadata(run_id, 44).unwrap();
            for _ in 0..256 {
                log.append(&EngineEvent::Stdout(vec![b'x'])).unwrap();
            }
            log.append(&EngineEvent::Stderr(vec![b'y'])).unwrap();
            drop(log);
            let server = start(tmp.path()).await.unwrap();
            let port = server.local_addr.port();
            let host = format!("127.0.0.1:{port}");
            let token = std::fs::read_to_string(tmp.path().join("run-http.token")).unwrap();
            let auth = format!("Authorization: Bearer {token}\r\nAccept: application/x-ndjson\r\n");
            let first = request(
                port,
                &format!("/v1/runs/{run_id}/stream?from_seq=0&streams=stderr"),
                &host,
                &auth,
                "",
            )
            .await;
            assert!(first.starts_with(b"HTTP/1.1 200"));
            assert!(String::from_utf8_lossy(&first).contains("x-next-seq: 256"));
            assert!(body(&first).is_empty());
            let second = request(
                port,
                &format!("/v1/runs/{run_id}/stream?from_seq=256&streams=stderr"),
                &host,
                &auth,
                "",
            )
            .await;
            assert!(String::from_utf8_lossy(&second).contains("x-next-seq: 257"));
            let event: serde_json::Value = serde_json::from_slice(body(&second)).unwrap();
            assert_eq!(event["seq"], 257);
            assert_eq!(event["stream"], "stderr");
        });
    }
}
