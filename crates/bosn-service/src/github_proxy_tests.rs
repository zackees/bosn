use super::*;

use kernal_api::async_engine::RuntimeBuilder;

const CANARY: &str = "gho_CANARYproxy0123456789abcdefXYZ";

/// What the fake upstream saw: (method, path+query, authorization, if-none-match).
type Seen = Arc<Mutex<Vec<(String, String, Option<String>, Option<String>)>>>;

async fn fake_upstream(seen: Seen) -> (String, async_engine::Task<io::Result<()>>) {
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
            let inm = header_str(&request, "if-none-match");
            seen.lock().unwrap().push((
                request.method().to_owned(),
                request.target().to_owned(),
                header_str(&request, "authorization"),
                inm.clone(),
            ));
            if inm.as_deref() == Some("\"v1\"") {
                return http_server::Response::new(304, Vec::new()).unwrap();
            }
            http_server::Response::new(200, br#"{"tag_name":"v1"}"#.to_vec())
                .unwrap()
                .with_header("etag", "\"v1\"")
                .unwrap()
                .with_header("content-type", "application/json")
                .unwrap()
                .with_header(
                    "link",
                    &format!("<http://127.0.0.1:{port}/repos/o/r/tags?page=2>; rel=\"next\""),
                )
                .unwrap()
        }
    }));
    (format!("http://127.0.0.1:{port}"), task)
}

async fn call(url: &str, method: http::Method, auth: Option<&str>) -> (u16, String, Vec<u8>) {
    let client = http::Client::new(http::Limits::default()).unwrap();
    let mut headers = vec![];
    if let Some(auth) = auth {
        headers.push(("authorization", auth));
    }
    let response = client
        .execute(http::Request {
            method,
            url,
            headers: &headers,
            body: if method == http::Method::Post {
                b"{}"
            } else {
                &[]
            },
        })
        .await
        .unwrap();
    let status = response.status();
    let cache = response
        .header("x-bosn-cache")
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .unwrap_or_default()
        + "|"
        + &response
            .header("link")
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .unwrap_or_default();
    (status, cache, response.into_bytes().await.unwrap())
}

fn runtime() -> async_engine::Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn proxy_attaches_the_credential_upstream_only_and_refuses_writes() {
    runtime().run(async {
        let seen: Seen = Arc::default();
        let (upstream, _up) = fake_upstream(seen.clone()).await;
        let (logs, mut log_rx) = async_engine::channel::<String>(256);
        let proxy = GithubApiProxy::start(
            &upstream,
            GithubCredential::for_test(CANARY),
            Arc::default(),
            Some(logs),
        )
        .await
        .unwrap();
        assert!(proxy.url().starts_with("http://127.0.0.1:"));
        assert!(!proxy.url().contains(CANARY));
        let base = proxy.url().to_owned();

        // A read goes upstream with the daemon's credential, replacing
        // whatever the client sent; the response never carries it.
        let (status, meta, body) = call(
            &format!("{base}/repos/o/r/releases/latest?per_page=1"),
            http::Method::Get,
            Some("Bearer dummy-from-container"),
        )
        .await;
        assert_eq!(status, 200);
        assert!(!String::from_utf8_lossy(&body).contains(CANARY));
        assert!(!meta.contains(CANARY));
        // Pagination links are rewritten to point back through the proxy.
        assert!(
            meta.contains(&format!("<{base}/repos/o/r/tags?page=2>")),
            "{meta}"
        );

        // Writes and non-allowlisted endpoints are refused without any
        // upstream contact.
        let (status, _, body) = call(
            &format!("{base}/repos/o/r/releases"),
            http::Method::Post,
            None,
        )
        .await;
        assert_eq!(status, 403);
        assert!(String::from_utf8_lossy(&body).contains("read-only"));
        let (status, _, _) = call(&format!("{base}/user"), http::Method::Get, None).await;
        assert_eq!(status, 403);
        let (status, _, _) = call(
            &format!("{base}/repos/o/r/actions/secrets"),
            http::Method::Get,
            None,
        )
        .await;
        assert_eq!(status, 403);

        // Without the nonce the listener serves nothing.
        let root = base.rsplit_once('/').unwrap().0;
        let (status, _, _) = call(
            &format!("{root}/repos/o/r/releases/latest"),
            http::Method::Get,
            None,
        )
        .await;
        assert_eq!(status, 404);
        let (status, _, _) = call(
            &format!("{root}/wrongnonce/repos/o/r/releases/latest"),
            http::Method::Get,
            None,
        )
        .await;
        assert_eq!(status, 404);

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].0, "GET");
        assert_eq!(seen[0].1, "/repos/o/r/releases/latest?per_page=1");
        assert_eq!(
            seen[0].2.as_deref(),
            Some(format!("Bearer {CANARY}").as_str())
        );

        drop(proxy);
        let mut lines = Vec::new();
        while let Ok(line) = log_rx.try_recv() {
            lines.push(line);
        }
        let joined = lines.join("\n");
        assert!(
            joined.contains("GET /repos/o/r/releases/latest -> 200"),
            "{joined}"
        );
        assert!(
            joined.contains("POST /repos/o/r/releases -> 403 refused"),
            "{joined}"
        );
        assert!(!joined.contains(CANARY));
        assert!(!joined.contains("per_page"), "query strings are not logged");
    });
}

#[test]
fn etag_cache_revalidates_with_if_none_match_and_serves_the_cached_body() {
    runtime().run(async {
        let seen: Seen = Arc::default();
        let (upstream, _up) = fake_upstream(seen.clone()).await;
        let cache: Arc<ResponseCache> = Arc::default();
        let first = GithubApiProxy::start(
            &upstream,
            GithubCredential::for_test(CANARY),
            cache.clone(),
            None,
        )
        .await
        .unwrap();
        let url = format!("{}/repos/o/r/releases/latest", first.url());
        let (status, meta, body) = call(&url, http::Method::Get, None).await;
        assert_eq!((status, meta.split('|').next().unwrap()), (200, "miss"));
        assert_eq!(body, br#"{"tag_name":"v1"}"#);
        drop(first);
        // A later task's proxy shares the cache: the upstream answers 304
        // (free against the quota) and the client still gets the body.
        let second = GithubApiProxy::start(
            &upstream,
            GithubCredential::for_test(CANARY),
            cache.clone(),
            None,
        )
        .await
        .unwrap();
        let url = format!("{}/repos/o/r/releases/latest", second.url());
        let (status, meta, body) = call(&url, http::Method::Get, None).await;
        assert_eq!((status, meta.split('|').next().unwrap()), (200, "hit"));
        assert_eq!(body, br#"{"tag_name":"v1"}"#);
        // A different credential never sees the other one's cache entry.
        let anon = GithubApiProxy::start(&upstream, GithubCredential::anonymous(), cache, None)
            .await
            .unwrap();
        let url = format!("{}/repos/o/r/releases/latest", anon.url());
        let (status, meta, _) = call(&url, http::Method::Get, None).await;
        assert_eq!((status, meta.split('|').next().unwrap()), (200, "miss"));
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].3, None);
        assert_eq!(seen[1].3.as_deref(), Some("\"v1\""));
        // Anonymous requests carry no Authorization at all.
        assert_eq!(seen[2].2, None);
        assert_eq!(seen[2].3, None);
    });
}

#[cfg(unix)]
#[test]
fn credential_comes_from_gh_then_stored_secret_then_anonymous() {
    use std::os::unix::fs::PermissionsExt;
    runtime().run(async {
        let state = tempfile::tempdir().unwrap();
        let missing: OsString = state.path().join("no-such-gh").into();
        // No gh, no stored secret: anonymous.
        let credential = resolve_credential(state.path(), &missing).await.unwrap();
        assert_eq!(credential.source(), CredentialSource::Anonymous);
        assert!(credential.secret_value().is_none());
        // No gh, stored secret: used.
        secrets::write_secret(state.path(), "github_token", CANARY.as_bytes()).unwrap();
        let credential = resolve_credential(state.path(), &missing).await.unwrap();
        assert_eq!(credential.source(), CredentialSource::StoredSecret);
        // A working gh wins over the stored secret.
        let gh = state.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\necho gho_FROMGHcli0123456789abcdef\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let credential = resolve_credential(state.path(), &gh.clone().into_os_string())
            .await
            .unwrap();
        assert_eq!(credential.source(), CredentialSource::GhCli);
        assert_eq!(
            credential.secret_value(),
            Some("gho_FROMGHcli0123456789abcdef")
        );
        // A failing gh (logged out) falls through.
        std::fs::write(&gh, "#!/bin/sh\necho 'not logged in' >&2\nexit 1\n").unwrap();
        let credential = resolve_credential(state.path(), &gh.into_os_string())
            .await
            .unwrap();
        assert_eq!(credential.source(), CredentialSource::StoredSecret);
    });
}

#[test]
fn allowlist_accepts_reads_and_refuses_writes_and_other_endpoints() {
    for ok in [
        "/rate_limit",
        "/repos/zackees/soldr",
        "/repos/zackees/soldr/releases/latest",
        "/repos/zackees/soldr/releases/tags/v0.9.25",
        "/repos/zackees/soldr/releases/assets/123",
        "/repos/actions/python-versions/git/trees/main",
        "/repos/actions/python-versions/git/blobs/abc",
        "/repos/o/r/git/ref/heads/main",
        "/repos/o/r/tags",
        "/repos/o/r/commits/main",
        "/repos/o/r/contents/a/b.json",
        "/repos/o/r/zipball/abc",
    ] {
        assert_eq!(check_request("GET", ok), Ok(()), "{ok}");
        assert_eq!(check_request("HEAD", ok), Ok(()), "{ok}");
    }
    for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS", "get"] {
        assert!(
            check_request(method, "/repos/o/r/releases").is_err(),
            "{method}"
        );
    }
    for bad in [
        "/user",
        "/user/repos",
        "/graphql",
        "/orgs/o/repos",
        "/repos/o/r/actions/secrets",
        "/repos/o/r/hooks",
        "/repos/o/r/collaborators",
        "/repos/o/r/issues",
        "/repos/o/r/pulls",
        "/repos/o/r/keys",
        "/repos/o/r/actions/runs",
        "/repos/o/r/../../user",
        "/repos/o/r/releases/%2e%2e/%2E%2E/user",
        "/repos/o/r/releases/..%2Fuser",
        "/repos//r/releases",
        "/repos/o/r//releases",
        "/authorizations",
        "/installation/token",
        "",
        "/",
    ] {
        assert!(check_request("GET", bad).is_err(), "{bad}");
    }
}

#[test]
fn credential_debug_never_prints_the_value() {
    let credential = GithubCredential::for_test("gho_CANARYdebug0123456789abcdef");
    let text = format!("{credential:?}");
    assert!(!text.contains("CANARY"), "{text}");
    assert!(text.contains("***"));
    assert!(!credential.fingerprint().contains("CANARY"));
}

#[test]
fn implausible_token_text_is_not_used() {
    assert!(plausible_token("gho_0123456789abcdefghij\n").is_some());
    assert!(plausible_token("short").is_none());
    assert!(plausible_token("not a token: gh error message here").is_none());
}
