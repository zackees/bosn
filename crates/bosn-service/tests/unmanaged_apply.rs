//! The removal path, exercised against a synthetic engine.
//!
//! Nothing here touches a real Docker: a shell script stands in for the CLI, so the pass can
//! be driven to remove, refuse, and fail without a live engine or a user's artifacts.

use std::path::Path;
use std::time::Duration;

use bosn_core::CensusConfig;
use bosn_engine::{DockerEngine, RunOptions};
use bosn_service::unmanaged::unmanaged_gc_apply;

/// Write a stand-in `docker` and return an engine that runs it.
fn synthetic(dir: &Path, script: &str) -> DockerEngine {
    let path = dir.join("fake-docker.sh");
    std::fs::write(&path, script).expect("write script");
    DockerEngine::synthetic_for_test("/bin/sh", [path.to_string_lossy().into_owned()])
}

/// A document with one stopped container created long ago and nothing else.
const CENSUS: &str = r#"{"Images":[],"Containers":[{"ID":"aaaabbbbcccc","CreatedAt":"2020-01-01 00:00:00 +0000 UTC","Labels":"","Size":"10MB","State":"exited"}],"Volumes":[],"BuildCache":[]}"#;

#[test]
fn an_unreadable_census_removes_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("removed");
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  system) echo 'this is not the document you are looking for' ;;
  rm|rmi|volume) touch "{marker}" ;;
esac
exit 0
"#,
        marker = marker.display()
    );
    let engine = synthetic(dir.path(), &script);
    let outcome = unmanaged_gc_apply(&engine, None, CensusConfig::default(), &[]);
    assert!(outcome.refused.is_some(), "an incomplete census must refuse");
    assert_eq!(outcome.removed, 0);
    assert!(
        !marker.exists(),
        "nothing may be removed from an unreadable census"
    );
}

#[test]
fn an_eligible_container_is_removed_by_its_identity() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("removed");
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  system) cat <<'JSON'
{census}
JSON
  ;;
  image) : ;;
  volume) : ;;
  rm) echo "$2" > "{marker}" ;;
  *) : ;;
esac
exit 0
"#,
        census = CENSUS,
        marker = marker.display()
    );
    let engine = synthetic(dir.path(), &script);
    let outcome = unmanaged_gc_apply(&engine, None, CensusConfig::default(), &[]);
    assert!(outcome.refused.is_none(), "refused: {:?}", outcome.refused);
    assert_eq!(outcome.removed, 1);
    assert_eq!(outcome.failed, 0);
    let removed = std::fs::read_to_string(&marker).expect("a removal was recorded");
    assert_eq!(
        removed.trim(),
        "aaaabbbbcccc",
        "removed by the engine's identity, not a name or tag"
    );
}

#[test]
fn a_refused_removal_does_not_abort_the_pass() {
    let dir = tempfile::tempdir().expect("temp dir");
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  system) cat <<'JSON'
{census}
JSON
  ;;
  rm) echo "Error response from daemon: removal refused" >&2; exit 1 ;;
  *) : ;;
esac
exit 0
"#,
        census = CENSUS
    );
    let engine = synthetic(dir.path(), &script);
    let outcome = unmanaged_gc_apply(&engine, None, CensusConfig::default(), &[]);
    // Docker refusing one removal is already fail-closed: the artifact stays and the pass
    // reports it rather than pretending the sweep succeeded.
    assert_eq!(outcome.removed, 0);
    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.failures.len(), 1);
    assert!(outcome.failures[0].contains("aaaabbbbcccc"));
    let _ = RunOptions::bounded(Duration::from_secs(1), 1024);
}
