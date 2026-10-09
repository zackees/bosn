//! #548: only Docker's own "no such object" answer proves absence. Any other failed
//! inspect (an unreachable daemon, a refused socket) must refuse, because absence
//! finalizes the registry row and the live object would be untracked for good.

use super::*;
use kernal_api::async_engine::RuntimeBuilder;

fn engine_failing_with(stderr: &str) -> DockerEngine {
    DockerEngine::synthetic_for_test(
        "/bin/sh",
        [
            "-c".to_owned(),
            format!("printf '%s\\n' '{stderr}' >&2; exit 1"),
            "fake-docker".to_owned(),
        ],
    )
}

const UNREACHABLE: &str = "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?";

fn setup_candidate() -> bosn_registry::SetupGcCandidate {
    bosn_registry::SetupGcCandidate {
        id: "1".into(),
        name: "bosn-setup-v2-abc".into(),
        generation: "sha256:abc".into(),
    }
}

fn volume_candidate() -> bosn_registry::ManifestVolumeGcCandidate {
    bosn_registry::ManifestVolumeGcCandidate {
        id: "1".into(),
        name: "bosn-v-stack-abc".into(),
        generation: "sha256:abc".into(),
    }
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(future)
}

#[test]
fn unreachable_daemon_is_an_inspection_failure_not_absence() {
    let engine = engine_failing_with(UNREACHABLE);
    assert!(block_on(inspect_setup_gc_container(&engine, &setup_candidate())).is_err());
    assert!(block_on(inspect_manifest_volume_gc(&engine, &volume_candidate())).is_err());
}

#[test]
fn docker_no_such_object_is_absence() {
    let container =
        engine_failing_with("Error response from daemon: No such container: bosn-setup-v2-abc");
    assert_eq!(
        block_on(inspect_setup_gc_container(&container, &setup_candidate())).unwrap(),
        None
    );
    let volume =
        engine_failing_with("Error response from daemon: get bosn-v-stack-abc: no such volume");
    assert_eq!(
        block_on(inspect_manifest_volume_gc(&volume, &volume_candidate())).unwrap(),
        None
    );
}
