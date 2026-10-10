//! #547: requests that address one object reach only the run's own objects,
//! and every container lands in the run's cgroup parent.

use super::access::{Access, Kind, Owner, owner_of, verdict};
use super::tests::{forward, settings};
use super::*;
use serde_json::Value;

/// An upstream that answers each inspect `GET <path>` from `objects`
/// (`path -> body`), and 404 for anything else. It serves until the test
/// process ends.
#[cfg(unix)]
pub(super) fn inspecting_upstream(dir: &Path, objects: &[(&str, &str)]) -> PathBuf {
    let path = dir.join("up.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let objects: BTreeMap<String, String> = objects
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let Ok(Some(head)) = read_head(&mut reader) else {
                continue;
            };
            assert_eq!(head.method, "GET", "the proxy only inspects");
            let reply = match objects.get(&head.target) {
                Some(body) => format!(
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
                None => "HTTP/1.0 404 Not Found\r\nContent-Length: 2\r\n\r\n{}".to_owned(),
            };
            let mut writer = stream;
            let _ = writer.write_all(reply.as_bytes());
        }
    });
    path
}

const OWN_CONTAINER: &str = r#"{"Id":"aaa","Config":{"Labels":{"com.zackees.bosn.run":"r-1"}}}"#;
const OTHER_CONTAINER: &str = r#"{"Id":"bbb","Config":{"Labels":{"com.zackees.bosn.run":"r-2"}}}"#;
const BOSN_CONTAINER: &str = r#"{"Id":"ccc","Config":{"Labels":null}}"#;

#[cfg(unix)]
fn engine(dir: &Path) -> ProxySettings {
    let mut s = settings(Arc::new(NoVolumes));
    s.upstream = inspecting_upstream(
        dir,
        &[
            ("/containers/aaa/json", OWN_CONTAINER),
            ("/containers/job-a/json", OWN_CONTAINER),
            ("/containers/bbb/json", OTHER_CONTAINER),
            ("/containers/ccc/json", BOSN_CONTAINER),
            ("/exec/e1/json", r#"{"ContainerID":"aaa"}"#),
            ("/exec/e2/json", r#"{"ContainerID":"bbb"}"#),
            (
                "/networks/net-a",
                r#"{"Labels":{"com.zackees.bosn.run":"r-1"}}"#,
            ),
            (
                "/networks/net-b",
                r#"{"Labels":{"com.zackees.bosn.run":"r-2"}}"#,
            ),
            ("/networks/shared", r#"{"Labels":{}}"#),
            (
                "/volumes/act-env",
                r#"{"Labels":{"com.zackees.bosn.run":"r-1"}}"#,
            ),
            ("/volumes/bosn-ci-cache-v1", r#"{"Labels":{}}"#),
            ("/images/node%3A20/json", r#"{"Config":{"Labels":null}}"#),
        ],
    );
    s
}

/// The status the proxy answers `request` with, or `None` if it forwarded it.
#[cfg(unix)]
fn answer(s: &ProxySettings, request: &str) -> Option<u16> {
    let (result, out) = forward(request.as_bytes(), s);
    match result {
        Ok(()) => {
            assert_eq!(out, request.as_bytes(), "forwarded untouched: {request}");
            None
        }
        Err(error) => {
            assert!(out.is_empty(), "nothing reached Docker: {request}");
            Some(access::refusal_status(&error))
        }
    }
}

#[cfg(unix)]
#[test]
fn another_runs_containers_cannot_be_inspected_killed_or_removed() {
    let dir = tempfile::tempdir().unwrap();
    let s = engine(dir.path());
    for request in [
        "GET /v1.47/containers/bbb/json HTTP/1.1\r\n\r\n",
        "DELETE /v1.47/containers/bbb?force=1 HTTP/1.1\r\n\r\n",
        "POST /v1.47/containers/bbb/kill HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
        "POST /v1.47/containers/bbb/exec HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
        "GET /v1.47/containers/bbb/archive?path=%2F HTTP/1.1\r\n\r\n",
        "POST /v1.47/exec/e2/start HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
        "POST /v1.47/commit?container=bbb HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
        // Bosn's own (unlabelled) containers are hidden too.
        "DELETE /containers/ccc?force=1 HTTP/1.1\r\n\r\n",
    ] {
        assert_eq!(answer(&s, request), Some(404), "{request}");
    }
}

#[cfg(unix)]
#[test]
fn the_runs_own_and_unknown_objects_are_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let s = engine(dir.path());
    for request in [
        "GET /v1.47/containers/aaa/json HTTP/1.1\r\n\r\n",
        "DELETE /v1.47/containers/job-a?force=1 HTTP/1.1\r\n\r\n",
        "POST /v1.47/exec/e1/start HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
        "POST /v1.47/networks/net-a/connect HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
        "DELETE /v1.47/volumes/act-env HTTP/1.1\r\n\r\n",
        // Docker answers 404 for these itself.
        "DELETE /v1.47/containers/gone HTTP/1.1\r\n\r\n",
        "DELETE /v1.47/volumes/gone HTTP/1.1\r\n\r\n",
        // Shared objects stay readable.
        "GET /v1.47/networks/shared HTTP/1.1\r\n\r\n",
        "GET /v1.47/volumes/bosn-ci-cache-v1 HTTP/1.1\r\n\r\n",
        // Collection routes are not objects.
        "GET /v1.47/networks HTTP/1.1\r\n\r\n",
        "GET /v1.47/images/json HTTP/1.1\r\n\r\n",
        "GET /_ping HTTP/1.1\r\n\r\n",
    ] {
        assert_eq!(answer(&s, request), None, "{request}");
    }
}

#[cfg(unix)]
#[test]
fn other_runs_networks_are_hidden_and_shared_objects_are_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let s = engine(dir.path());
    for (request, status) in [
        ("GET /v1.47/networks/net-b HTTP/1.1\r\n\r\n", 404),
        ("DELETE /v1.47/networks/net-b HTTP/1.1\r\n\r\n", 404),
        ("DELETE /v1.47/networks/shared HTTP/1.1\r\n\r\n", 403),
        (
            "DELETE /v1.47/volumes/bosn-ci-cache-v1 HTTP/1.1\r\n\r\n",
            403,
        ),
        (
            "DELETE /v1.47/images/node%3A20?force=1 HTTP/1.1\r\n\r\n",
            403,
        ),
    ] {
        assert_eq!(answer(&s, request), Some(status), "{request}");
    }
}

#[test]
fn an_unreachable_upstream_fails_closed() {
    let s = settings(Arc::new(NoVolumes));
    let (result, out) = forward(b"DELETE /containers/x HTTP/1.1\r\n\r\n", &s);
    assert_eq!(access::refusal_status(&result.unwrap_err()), 500);
    assert!(out.is_empty());
}

#[test]
fn object_routes_are_recognized() {
    let at = |method, target| access::addressed(method, target);
    let container = |id: &str, write| Access {
        kind: Kind::Container,
        id: id.into(),
        write,
    };
    assert_eq!(
        at("GET", "/v1.47/containers/a%2Fb/json"),
        Some(container("a/b", false))
    );
    assert_eq!(
        at("HEAD", "/containers/x/archive"),
        Some(container("x", false))
    );
    assert_eq!(
        at("POST", "/commit?container=x&repo=r"),
        Some(container("x", true))
    );
    assert_eq!(
        at("DELETE", "/v1.47/images/ghcr.io/o/i:1").map(|a| (a.kind, a.id)),
        Some((Kind::Image, "ghcr.io/o/i:1".into()))
    );
    for collection in [
        "/containers/json",
        "/containers/create",
        "/networks/create",
        "/volumes/create",
        "/volumes",
        "/images/x/json",
        "/build",
        "/_ping",
    ] {
        assert_eq!(at("GET", collection), None, "{collection}");
    }
    assert_eq!(at("POST", "/containers/prune"), None);
    assert_eq!(
        at("POST", "/images/x/tag"),
        None,
        "image writes but deletes pass"
    );
}

#[test]
fn ownership_is_read_from_the_inspect() {
    let reply = |status: &str, body: &str| format!("HTTP/1.0 {status}\r\n\r\n{body}");
    assert_eq!(
        owner_of(Kind::Container, reply("200 OK", OWN_CONTAINER).as_bytes()).unwrap(),
        Owner::Run("r-1".into())
    );
    assert_eq!(
        owner_of(Kind::Image, reply("200 OK", "{}").as_bytes()).unwrap(),
        Owner::Unlabelled
    );
    assert_eq!(
        owner_of(Kind::Volume, reply("404 Not Found", "{}").as_bytes()).unwrap(),
        Owner::Missing
    );
    assert!(owner_of(Kind::Network, reply("500 Oops", "{}").as_bytes()).is_err());
    assert!(owner_of(Kind::Network, reply("200 OK", "[").as_bytes()).is_err());
    assert!(owner_of(Kind::Exec, reply("200 OK", "{}").as_bytes()).is_err());
    assert!(owner_of(Kind::Container, b"").is_err());
    let write = Access {
        kind: Kind::Volume,
        id: "v".into(),
        write: true,
    };
    assert!(verdict(&write, &Owner::Missing, "r-1").is_ok());
    assert!(verdict(&write, &Owner::Run("r-1".into()), "r-1").is_ok());
    assert!(verdict(&write, &Owner::Unlabelled, "r-1").is_err());
}

#[test]
fn container_creates_name_the_objects_they_reach_into() {
    let body = serde_json::json!({
        "HostConfig": {
            "VolumesFrom": ["src:ro"],
            "Links": ["/db:database"],
            "NetworkMode": "net-b",
            "PidMode": "container:p",
            "IpcMode": "shareable"
        },
        "NetworkingConfig": {"EndpointsConfig": {"bridge": {}, "net-c": {"Aliases": ["x"]}}}
    });
    let refs = access::create_references(body.to_string().as_bytes()).unwrap();
    let named: Vec<(Kind, &str)> = refs.iter().map(|a| (a.kind, a.id.as_str())).collect();
    assert_eq!(
        named,
        [
            (Kind::Container, "src"),
            (Kind::Container, "db"),
            (Kind::Container, "p"),
            (Kind::Network, "net-c"),
            (Kind::Network, "net-b"),
        ]
    );
    assert!(refs.iter().all(|a| !a.write));
    let joined = serde_json::json!({"HostConfig": {"NetworkMode": "container:aaa"}});
    let refs = access::create_references(joined.to_string().as_bytes()).unwrap();
    assert_eq!(refs[0].kind, Kind::Container);
    assert!(access::create_references(b"{}").unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn a_create_reaching_into_another_run_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let s = engine(dir.path());
    for body in [
        r#"{"HostConfig":{"VolumesFrom":["bbb"]}}"#,
        r#"{"HostConfig":{"NetworkMode":"container:bbb"}}"#,
        r#"{"HostConfig":{"NetworkMode":"net-b"}}"#,
    ] {
        let request = format!(
            "POST /containers/create HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert_eq!(answer(&s, &request), Some(403), "{body}");
    }
    let own = r#"{"HostConfig":{"VolumesFrom":["aaa"],"NetworkMode":"net-a"}}"#;
    let request = format!(
        "POST /containers/create HTTP/1.1\r\nContent-Length: {}\r\n\r\n{own}",
        own.len()
    );
    let (result, out) = forward(request.as_bytes(), &s);
    result.unwrap();
    assert!(!out.is_empty());
}

#[test]
fn every_container_is_pinned_to_the_runs_cgroup_parent() {
    let mut s = settings(Arc::new(NoVolumes));
    s.cgroup_parent = Some("/run-a".into());
    for body in [
        r#"{}"#,
        r#"{"HostConfig":null,"Labels":null}"#,
        // act's --container-options, a workflow container.options or a
        // service container asking for another parent.
        r#"{"HostConfig":{"CgroupParent":"/"}}"#,
        r#"{"HostConfig":{"CgroupParent":"/run-b"}}"#,
    ] {
        let out: Value =
            serde_json::from_slice(&rewrite_create("container", body.as_bytes(), &s).unwrap())
                .unwrap();
        assert_eq!(out["HostConfig"]["CgroupParent"], "/run-a", "{body}");
        assert_eq!(out["Labels"][LABEL_CGROUP_PARENT], "/run-a", "{body}");
        assert_eq!(out["Labels"]["com.zackees.bosn.run"], "r-1", "{body}");
    }
    let network: Value =
        serde_json::from_slice(&rewrite_create("network", b"{}", &s).unwrap()).unwrap();
    assert!(network.get("HostConfig").is_none(), "only containers");
    // Without the setting the caller's choice is kept.
    s.cgroup_parent = None;
    let out: Value = serde_json::from_slice(
        &rewrite_create("container", br#"{"HostConfig":{"CgroupParent":"/x"}}"#, &s).unwrap(),
    )
    .unwrap();
    assert_eq!(out["HostConfig"]["CgroupParent"], "/x");
    assert!(out["Labels"].get(LABEL_CGROUP_PARENT).is_none());
}
