//! #560: a job's whole-host Docker calls reach only the run's own objects.

use super::tests::{forward, settings};
use super::*;
use serde_json::Value;

const RUN_FILTER: &str = "filters=%7B%22label%22%3A%5B%22com.zackees.bosn.run%3Dr-1%22%5D%7D";

fn forwarded_target(request: &str) -> String {
    let s = settings(Arc::new(NoVolumes));
    let (result, out) = forward(request.as_bytes(), &s);
    result.unwrap();
    let text = String::from_utf8(out).unwrap();
    text.split(' ').nth(1).unwrap().to_owned()
}

#[test]
fn prunes_and_the_volume_listing_are_scoped_to_the_run() {
    for path in [
        "/v1.47/containers/prune",
        "/v1.47/volumes/prune",
        "/v1.47/networks/prune",
        "/v1.47/images/prune",
    ] {
        let request = format!(
            "POST {path}?filters=%7B%22all%22%3A%5B%22true%22%5D%7D HTTP/1.1\r\nContent-Length: 0\r\n\r\n"
        );
        let target = forwarded_target(&request);
        let (_, pairs) = rewrite::query_pairs(&target);
        let filters: Value = serde_json::from_str(&pairs[0].1).unwrap();
        assert_eq!(filters["label"][0], "com.zackees.bosn.run=r-1", "{path}");
        assert_eq!(
            filters["all"][0], "true",
            "{path}: the caller's filters are kept"
        );
    }
    assert_eq!(
        forwarded_target("GET /v1.47/volumes HTTP/1.1\r\n\r\n"),
        format!("/v1.47/volumes?{RUN_FILTER}")
    );
}

#[test]
fn a_build_cache_prune_matches_no_record() {
    let target =
        forwarded_target("POST /v1.47/build/prune?all=1 HTTP/1.1\r\nContent-Length: 0\r\n\r\n");
    let (_, pairs) = rewrite::query_pairs(&target);
    let filters = &pairs.iter().find(|(k, _)| k == "filters").unwrap().1;
    let filters: Value = serde_json::from_str(filters).unwrap();
    assert_eq!(filters["id"], serde_json::json!(["^$"]));
}

#[test]
fn image_and_network_listings_and_other_calls_pass_through() {
    for request in [
        "GET /v1.47/images/json HTTP/1.1\r\n\r\n",
        "GET /v1.47/networks HTTP/1.1\r\n\r\n",
        "GET /v1.47/volumes/x HTTP/1.1\r\n\r\n",
        "DELETE /v1.47/containers/x?force=1 HTTP/1.1\r\n\r\n",
    ] {
        let s = settings(Arc::new(NoVolumes));
        let (result, out) = forward(request.as_bytes(), &s);
        result.unwrap();
        assert_eq!(out, request.as_bytes(), "{request}");
    }
}

#[test]
fn volume_ownership_is_decided_by_the_run_label() {
    let inspect = |status: &str, body: &str| {
        format!("HTTP/1.0 {status}\r\nContent-Type: application/json\r\n\r\n{body}")
    };
    let own = inspect(
        "200 OK",
        r#"{"Name":"v","Labels":{"com.zackees.bosn.run":"r-1"}}"#,
    );
    let foreign = inspect(
        "200 OK",
        r#"{"Name":"bosn-ci-cache-v1","Labels":{"com.zackees.bosn.kind":"volume"}}"#,
    );
    let other_run = inspect(
        "200 OK",
        r#"{"Name":"v","Labels":{"com.zackees.bosn.run":"r-2"}}"#,
    );
    let unlabelled = inspect("200 OK", r#"{"Name":"v","Labels":null}"#);
    assert!(volume_owner_verdict(own.as_bytes(), "r-1").is_ok());
    assert!(volume_owner_verdict(inspect("404 Not Found", "{}").as_bytes(), "r-1").is_ok());
    for refused in [
        foreign,
        other_run,
        unlabelled,
        inspect("500 Internal Server Error", "{}"),
    ] {
        assert!(
            volume_owner_verdict(refused.as_bytes(), "r-1").is_err(),
            "{refused}"
        );
    }
    assert!(volume_owner_verdict(b"", "r-1").is_err());
}

/// One upstream that answers a single volume inspect with `body`.
#[cfg(unix)]
fn inspecting_upstream(dir: &Path, body: &'static str) -> PathBuf {
    let path = dir.join("up.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let head = read_head(&mut reader).unwrap().unwrap();
        assert_eq!(head.method, "GET");
        let mut writer = stream;
        let reply = format!(
            "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        writer.write_all(reply.as_bytes()).unwrap();
    });
    path
}

#[cfg(unix)]
#[test]
fn removing_another_owners_volume_is_refused_and_never_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = settings(Arc::new(NoVolumes));
    s.upstream = inspecting_upstream(dir.path(), r#"{"Name":"bosn-ci-cache-v1","Labels":{}}"#);
    let (result, out) = forward(
        b"DELETE /v1.47/volumes/bosn-ci-cache-v1 HTTP/1.1\r\n\r\n",
        &s,
    );
    let error = result.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(
        error.to_string().contains("not created by this run"),
        "{error}"
    );
    assert!(out.is_empty(), "nothing reached Docker");
}

#[cfg(unix)]
#[test]
fn removing_the_runs_own_volume_is_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = settings(Arc::new(NoVolumes));
    s.upstream = inspecting_upstream(
        dir.path(),
        r#"{"Name":"act-env","Labels":{"com.zackees.bosn.run":"r-1"}}"#,
    );
    let request = b"DELETE /v1.47/volumes/act-env HTTP/1.1\r\n\r\n";
    let (result, out) = forward(request, &s);
    result.unwrap();
    assert_eq!(out, request.to_vec());
}
