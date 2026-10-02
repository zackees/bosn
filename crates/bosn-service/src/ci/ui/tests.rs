//! The UI listener's security and backpressure contract, over raw TCP so
//! `Host` and `Origin` can be forged exactly as an attacker would.

use std::{
    io::{Read, Write},
    net::TcpStream,
    sync::Arc,
    time::{Duration, Instant},
};

use super::*;
use crate::ci::CiRequest;
use crate::ci::lifecycle::tests::{FakeBackend, with_registry};

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

/// One raw HTTP/1.1 request (Connection: close) on the blocking lane.
async fn raw(port: u16, request: String) -> Reply {
    async_engine::launch_blocking(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut text = String::new();
        let _ = stream.read_to_string(&mut text);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Reply {
            status,
            headers: head.to_ascii_lowercase(),
            body: body.into(),
        }
    })
    .await
    .unwrap()
}

fn get(_port: u16, path: &str, host: &str, cookie: Option<&str>) -> String {
    let cookie = cookie
        .map(|c| format!("Cookie: {c}\r\n"))
        .unwrap_or_default();
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{cookie}Connection: close\r\n\r\n")
}

fn post(port: u16, path: &str, cookie: &str, origin: Option<&str>, body: &str) -> String {
    let origin = origin
        .map(|o| format!("Origin: {o}\r\n"))
        .unwrap_or_default();
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {cookie}\r\n{origin}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn listener(
    dir: &std::path::Path,
    registry: crate::RegistryActor,
) -> (CiRuntime, UiServer, u16) {
    let ci = CiRuntime::start(dir, registry, Arc::new(FakeBackend::default()), 1);
    let server = start(
        UiConfig {
            enabled: true,
            port: 0,
        },
        ci.clone(),
    )
    .await
    .unwrap()
    .expect("enabled");
    ci.attach_ui(server.handle.clone());
    // The socket itself, not only the advertised origin, is loopback.
    assert!(
        server.local_addr.ip().is_loopback(),
        "{}",
        server.local_addr
    );
    let port = server.local_addr.port();
    assert_eq!(server.handle.origin, format!("http://127.0.0.1:{port}"));
    (ci, server, port)
}

fn fake_record() -> crate::ci::RunRecord {
    use crate::ci::provider::{Mode, Provider, Trigger};
    let request = crate::ci::SubmitRequest {
        staging: "aaaaaaaa-bbbb-4ccc-8ddd-000000000000".into(),
        workspace: "/work/repo".into(),
        provider: Provider::Github,
        engine: "act".into(),
        workflow: ".github/workflows/ci.yml".into(),
        job: None,
        trigger: Trigger::Push,
        mode: Mode::Minimal,
        actor: "human".into(),
        sha: "a".repeat(40),
        branch: None,
        tree_digest: "d".repeat(64),
        dirty: false,
        commit: None,
        base: None,
        origin: None,
        pr_number: None,
        timeout_secs: None,
        secrets: Vec::new(),
    };
    crate::ci::RunRecord::queued(
        "aaaaaaaa-bbbb-4ccc-8ddd-000000000001".into(),
        &request,
        "push",
        b"{}",
    )
}

/// Sign in the way `bosn ui` does: grant over IPC, redeem over HTTP.
async fn sign_in(ci: &CiRuntime, port: u16) -> String {
    let grant: crate::ci::UiGrantReply = serde_json::from_value(
        ci.handle(CiRequest::UiGrant {
            path: Some("/ci".into()),
        })
        .await
        .unwrap(),
    )
    .unwrap();
    let path = grant
        .url
        .split_once(&format!(":{port}"))
        .unwrap()
        .1
        .to_string();
    let host = format!("127.0.0.1:{port}");
    let redeemed = raw(port, get(port, &path, &host, None)).await;
    assert_eq!(redeemed.status, 303, "{}", redeemed.body);
    assert!(redeemed.headers.contains("location: /ci"));
    let cookie = redeemed
        .headers
        .lines()
        .find_map(|l| l.strip_prefix("set-cookie: "))
        .expect("a session cookie")
        .to_string();
    assert!(cookie.contains("httponly") && cookie.contains("samesite=strict"));
    let replay = raw(port, get(port, &path, &host, None)).await;
    assert_eq!(replay.status, 401, "a grant is single-use");
    cookie.split(';').next().unwrap().to_string()
}

#[test]
fn tokens_cookies_host_and_origin_are_enforced() {
    with_registry(|registry, dir| async move {
        let (ci, _server, port) = listener(&dir, registry).await;
        let host = format!("127.0.0.1:{port}");
        assert_eq!(
            raw(port, get(port, "/v1/runs", &host, None)).await.status,
            401,
            "no session"
        );
        assert_eq!(
            raw(port, get(port, "/v1/runs", &host, Some("bosn_ui=forged")))
                .await
                .status,
            401,
            "wrong session"
        );
        assert_eq!(
            raw(port, get(port, "/auth?token=nope", &host, None))
                .await
                .status,
            401,
            "unknown grant"
        );
        let cookie = sign_in(&ci, port).await;
        let ok = raw(port, get(port, "/v1/runs", &host, Some(&cookie))).await;
        assert_eq!(ok.status, 200, "{}", ok.body);
        let listed: crate::ci::ListReply = serde_json::from_str(&ok.body).unwrap();
        assert_eq!(listed.runners.limit, 1);
        let page = raw(port, get(port, "/", &host, Some(&cookie))).await;
        assert_eq!(page.status, 200);
        assert!(
            page.headers
                .contains("content-security-policy: default-src 'self'")
        );
        // DNS rebinding: a foreign Host is refused even with a valid session.
        let rebound = raw(
            port,
            get(
                port,
                "/v1/runs",
                &format!("evil.example:{port}"),
                Some(&cookie),
            ),
        )
        .await;
        assert_eq!(rebound.status, 403);
        // CSRF: writes need this listener's Origin; a missing one is refused.
        let run = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let cancel = format!("/v1/runs/{run}/cancel");
        assert_eq!(
            raw(port, post(port, &cancel, &cookie, None, ""))
                .await
                .status,
            403
        );
        assert_eq!(
            raw(
                port,
                post(port, &cancel, &cookie, Some("http://evil.example"), "")
            )
            .await
            .status,
            403
        );
        let same_origin = raw(
            port,
            post(
                port,
                &cancel,
                &cookie,
                Some(&format!("http://127.0.0.1:{port}")),
                "",
            ),
        )
        .await;
        assert_eq!(
            same_origin.status, 404,
            "reached the typed operation: {}",
            same_origin.body
        );
        let drain = raw(
            port,
            post(
                port,
                "/v1/runners",
                &cookie,
                Some(&format!("http://127.0.0.1:{port}")),
                r#"{"action":"drain"}"#,
            ),
        )
        .await;
        assert_eq!(drain.status, 200, "{}", drain.body);
        assert!(drain.body.contains(r#""drained":true"#));
    });
}

#[test]
fn disabled_means_no_listener_and_grants_are_refused() {
    with_registry(|registry, dir| async move {
        let ci = CiRuntime::start(&dir, registry, Arc::new(FakeBackend::default()), 1);
        assert!(
            start(UiConfig::default(), ci.clone())
                .await
                .unwrap()
                .is_none()
        );
        let error = ci
            .handle(CiRequest::UiGrant { path: None })
            .await
            .unwrap_err();
        assert_eq!(error.code, "refused");
        assert!(error.message.contains("[ui] enabled = true"));
    });
}

/// How long a step that must happen may take before the test calls it hung.
/// A liveness guard for a loaded machine, never a speed assertion (#412).
const HUNG: Duration = Duration::from_secs(60);

/// Subscribe to the live feed and return once the daemon holds the
/// subscription: `events` takes its receiver before the response head is
/// written, so reading the head is the barrier.
async fn subscribed_feed(port: u16, cookie: &str) -> TcpStream {
    let request = get(
        port,
        "/v1/events",
        &format!("127.0.0.1:{port}"),
        Some(cookie),
    );
    async_engine::launch_blocking(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(HUNG)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream
                .read_exact(&mut byte)
                .expect("the feed's response head");
            head.push(byte[0]);
        }
        assert!(
            head.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&head)
        );
        stream
    })
    .await
    .unwrap()
}

/// Events that must overflow a stalled reader: its socket buffers (the
/// kernel's largest send plus receive buffer, and a margin for the server's
/// own write buffer) full of events, plus everything the feed retains. Only
/// then is the reader guaranteed to lag, whatever the scheduler does.
fn events_that_overflow_a_stalled_reader(event: &RunEvent) -> usize {
    let max_buffer = |sysctl: &str| -> usize {
        std::fs::read_to_string(sysctl)
            .ok()
            .and_then(|v| v.split_whitespace().nth(2)?.parse().ok())
            .unwrap_or(32 << 20)
    };
    let buffered = max_buffer("/proc/sys/net/ipv4/tcp_wmem")
        + max_buffer("/proc/sys/net/ipv4/tcp_rmem")
        + (1 << 20);
    buffered / event.to_json().len() + 2 * crate::ci::events::FEED_CAPACITY
}

#[test]
fn a_paused_feed_reader_never_delays_publishing_or_other_clients() {
    with_registry(|registry, dir| async move {
        let (ci, _server, port) = listener(&dir, registry).await;
        let cookie = sign_in(&ci, port).await;
        let host = format!("127.0.0.1:{port}");
        // A reader that subscribes and then never reads.
        let mut paused = subscribed_feed(port, &cookie).await;
        // A second reader that keeps up: it reports when it sees a marker.
        let mut active = subscribed_feed(port, &cookie).await;
        let marker = "aaaaaaaa-bbbb-4ccc-8ddd-0000000000ff";
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut text = String::new();
            let mut buf = [0u8; 65536];
            while let Ok(n) = active.read(&mut buf) {
                if n == 0 {
                    break;
                }
                text.push_str(&String::from_utf8_lossy(&buf[..n]));
                if text.contains(marker) {
                    let _ = seen_tx.send(());
                    break;
                }
                // Keep only a tail, so a split marker is still found.
                if text.len() > 4096 {
                    text.drain(..text.len() - 256);
                }
            }
        });
        // Far more events than a reader retains, while one reader is
        // stalled: publishing returns, because it never waits for readers.
        let fake = fake_record();
        let (published_tx, published_rx) = std::sync::mpsc::channel();
        let feed = ci.feed().clone();
        let flood = fake.clone();
        let events = events_that_overflow_a_stalled_reader(&RunEvent::of(&fake));
        std::thread::spawn(move || {
            for _ in 0..events {
                feed.publish(&flood);
            }
            let _ = published_tx.send(());
        });
        published_rx
            .recv_timeout(HUNG)
            .expect("publishing never waits for a stalled reader");
        // Other clients are served while the paused reader is stalled.
        let reply = raw(port, get(port, "/v1/runners", &host, Some(&cookie))).await;
        assert_eq!(reply.status, 200, "other clients unaffected");
        // The newest event reaches the reader that keeps up, though the
        // paused reader has still not read a byte of the feed.
        let mut marked = fake.clone();
        marked.id = marker.into();
        ci.feed().publish(&marked);
        seen_rx
            .recv_timeout(HUNG)
            .expect("the active reader receives the event despite the stalled reader");
        // The paused reader resumes with a resync rather than a backlog.
        let mut seen = String::new();
        let mut buf = [0u8; 65536];
        while !seen.contains("resync") {
            match paused.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
        assert!(
            seen.contains(r#""type":"resync""#),
            "lagging reader is told to resync"
        );
    });
}

#[test]
fn malformed_oversized_and_slow_clients_stay_within_limits() {
    with_registry(|registry, dir| async move {
        let (ci, _server, port) = listener(&dir, registry).await;
        let cookie = sign_in(&ci, port).await;
        let host = format!("127.0.0.1:{port}");
        // Garbage, an oversized header and an oversized body never panic the
        // listener; each gets a 4xx or a closed connection.
        let huge_header = format!(
            "GET /v1/runs HTTP/1.1\r\nHost: {host}\r\nX-Big: {}\r\nConnection: close\r\n\r\n",
            "a".repeat(64 * 1024)
        );
        let huge_body = post(
            port,
            "/v1/runners",
            &cookie,
            Some(&format!("http://{host}")),
            &"x".repeat(200 * 1024),
        );
        for request in [
            "\x00\x01\x02 not http\r\n\r\n".to_string(),
            "GET /v1/runs HTTP/9.9\r\n\r\n".to_string(),
            huge_header,
            huge_body,
        ] {
            let reply = raw(port, request).await;
            assert!(
                reply.status == 0 || (400..500).contains(&reply.status),
                "{}",
                reply.status
            );
        }
        // Slow-drip clients hold connections but never block a healthy one.
        let drips: Vec<TcpStream> = (0..8)
            .map(|_| {
                let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                s.write_all(b"GET /v1/runs HTTP/1.1\r\nHo").unwrap();
                s
            })
            .collect();
        let started = Instant::now();
        let healthy = raw(port, get(port, "/v1/runners", &host, Some(&cookie))).await;
        assert_eq!(healthy.status, 200);
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(drips);
    });
}

#[test]
fn the_dashboard_drives_the_widget_only_through_queued_allowlisted_commands() {
    with_registry(|registry, dir| async move {
        let (ci, _server, port) = listener(&dir, registry).await;
        let cookie = sign_in(&ci, port).await;
        let origin = format!("http://127.0.0.1:{port}");
        // A widget process registers over the owner-only socket.
        let hello: crate::ci::WidgetReply = serde_json::from_value(
            ci.handle(CiRequest::WidgetHello {
                pid: 42,
                session: "s1".into(),
                explicit: false,
            })
            .await
            .unwrap(),
        )
        .unwrap();
        assert!(hello.allowed);
        assert_eq!(hello.presence, crate::ci::widget::WidgetPresence::Connected);
        let allowed = raw(
            port,
            post(
                port,
                "/v1/widget/open-external",
                &cookie,
                Some(&origin),
                r#"{"url":"https://github.com/zackees/bosn"}"#,
            ),
        )
        .await;
        assert_eq!(allowed.status, 200, "{}", allowed.body);
        let refused = raw(
            port,
            post(
                port,
                "/v1/widget/open-external",
                &cookie,
                Some(&origin),
                r#"{"url":"https://evil.example/x"}"#,
            ),
        )
        .await;
        assert_eq!(refused.status, 400, "non-allowlisted links are refused");
        assert_eq!(
            raw(
                port,
                post(port, "/v1/widget/toggle", &cookie, Some(&origin), "")
            )
            .await
            .status,
            200
        );
        let page = raw(
            port,
            get(
                port,
                "/widget/bubble",
                &format!("127.0.0.1:{port}"),
                Some(&cookie),
            ),
        )
        .await;
        assert_eq!(page.status, 200);
        assert!(page.body.contains("/widget/widget.js"));
        let poll: crate::ci::WidgetReply =
            serde_json::from_value(ci.handle(CiRequest::WidgetPoll { pid: 42 }).await.unwrap())
                .unwrap();
        use crate::ci::widget::WidgetCommand;
        assert_eq!(
            poll.commands,
            [
                WidgetCommand::OpenExternal {
                    url: "https://github.com/zackees/bosn".into()
                },
                WidgetCommand::Toggle,
            ]
        );
        let listed = ci.listing();
        assert_eq!(
            listed.runners.widget,
            crate::ci::widget::WidgetPresence::Connected
        );
    });
}
