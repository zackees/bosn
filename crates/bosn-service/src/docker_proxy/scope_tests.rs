//! #560: a job's whole-host Docker calls reach only the run's own objects.

#[cfg(unix)]
use super::access_tests::inspecting_upstream;
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
    ] {
        let s = settings(Arc::new(NoVolumes));
        let (result, out) = forward(request.as_bytes(), &s);
        result.unwrap();
        assert_eq!(out, request.as_bytes(), "{request}");
    }
}

#[cfg(unix)]
#[test]
fn removing_another_owners_volume_is_refused_and_never_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = settings(Arc::new(NoVolumes));
    s.upstream = inspecting_upstream(
        dir.path(),
        &[(
            "/volumes/bosn-ci-cache-v1",
            r#"{"Name":"bosn-ci-cache-v1","Labels":{}}"#,
        )],
    );
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
        &[(
            "/volumes/act-env",
            r#"{"Name":"act-env","Labels":{"com.zackees.bosn.run":"r-1"}}"#,
        )],
    );
    let request = b"DELETE /v1.47/volumes/act-env HTTP/1.1\r\n\r\n";
    let (result, out) = forward(request, &s);
    result.unwrap();
    assert_eq!(out, request.to_vec());
}
