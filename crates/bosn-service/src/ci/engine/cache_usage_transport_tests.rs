//! Measurement failures and Docker auto-removal preserve the cache volume.

use super::{CACHE_VOLUME, CacheVolume, DockerActBackend};
use bosn_engine::DockerEngine;
use kernal_api::platform::fs::TemporaryDirectory;

fn fixture(success: bool, cleanup: &str) -> (TemporaryDirectory, DockerActBackend) {
    let dir = TemporaryDirectory::new().unwrap();
    let cache = CacheVolume::machine("11111111-2222-4333-8444-555555555555", 1.0).unwrap();
    let document = serde_json::json!([{
        "Name": cache.name, "Labels": cache.labels, "Scope": "local",
        "Driver": "local", "Options": null
    }]);
    std::fs::write(dir.path().join("volume.json"), document.to_string()).unwrap();
    std::fs::write(dir.path().join("cleanup"), cleanup).unwrap();
    if success {
        std::fs::write(dir.path().join("success"), "").unwrap();
    }
    std::fs::write(
        dir.path().join("docker.sh"),
        r#"
base=$1
shift
printf '%s\n' "$*" >> "$base/commands"
case "$1" in
  volume) cat "$base/volume.json" ;;
  create) printf '%064d\n' 1 ;;
  start)
    if [ -e "$base/success" ]; then
      for class in total tools images actions toolcache actcache; do
        printf '%s 0 0\n' "$class"
      done
    else
      echo 'read failed' >&2; exit 1
    fi ;;
  rm)
    case "$(cat "$base/cleanup")" in
      absent) echo 'Error: No such container: immutable-id' >&2; exit 1 ;;
      denied) echo 'permission denied' >&2; exit 1 ;;
      *) exit 0 ;;
    esac ;;
  *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    let backend = DockerActBackend::new(DockerEngine::synthetic_for_test(
        "sh",
        [
            dir.path().join("docker.sh").into_os_string(),
            dir.path().as_os_str().to_owned(),
        ],
    ));
    (dir, backend)
}

fn measure(backend: &DockerActBackend) -> Result<crate::ci::CacheUsage, String> {
    kernal_api::async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(backend.measure_cache(CACHE_VOLUME))
}

#[test]
fn failed_measurement_reaps_its_immutable_helper_without_removing_the_cache() {
    let (dir, backend) = fixture(false, "removed");
    assert!(measure(&backend).unwrap_err().contains("read failed"));
    let commands = std::fs::read_to_string(dir.path().join("commands")).unwrap();
    assert_eq!(
        commands.lines().last().unwrap(),
        format!("rm -f -v {:064}", 1)
    );
    assert!(commands.contains("create --rm"));
    assert!(!commands.contains("volume rm"));
}

#[test]
fn docker_auto_removal_is_proven_absence_after_a_successful_sample() {
    let (_dir, backend) = fixture(true, "absent");
    let report = measure(&backend).unwrap();
    assert!(!report.partial);
    assert_eq!(report.bytes, Some(0));
}

#[test]
fn failed_cleanup_reports_the_exact_container_that_needs_attention() {
    let (_dir, backend) = fixture(true, "denied");
    let error = measure(&backend).unwrap_err();
    assert!(error.contains("permission denied"));
    assert!(error.contains(&format!("{:064}", 1)));
}
