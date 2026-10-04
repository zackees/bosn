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
    let helper = serde_json::json!([{
        "Id": format!("{:064}", 1), "Name": "/@NAME@",
        "Config": {"Image": super::engine_image(), "Labels": {"io.bosn.cache.measurement": "@NONCE@"}},
        "HostConfig": {"ReadonlyRootfs": true, "Privileged": false, "NetworkMode": "none"},
        "Mounts": [{"Type": "volume", "Name": CACHE_VOLUME, "Destination": "/cache", "RW": false}]
    }]);
    std::fs::write(dir.path().join("helper.json"), helper.to_string()).unwrap();
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
  create)
    shift
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --name) printf '%s' "$2" > "$base/name"; shift ;;
        --label) printf '%s' "${2#*=}" > "$base/nonce"; shift ;;
      esac
      shift
    done
    if [ -e "$base/create-mode" ]; then
      case "$(cat "$base/create-mode")" in
        invalid) printf 'no-id\n'; exit 0 ;;
        *) echo 'create acknowledgement lost' >&2; exit 1 ;;
      esac
    fi
    printf '%064d\n' 1 ;;

  start)
    if [ -e "$base/success" ]; then
      for class in total tools images actions toolcache actcache; do
        printf '%s 0 0\n' "$class"
      done
    else
      echo 'read failed' >&2; exit 1
    fi ;;
  container)
    if [ "$3" = "$(cat "$base/name")" ]; then
      if [ -e "$base/lookup-mode" ]; then
        case "$(cat "$base/lookup-mode")" in
          absent) echo 'Error: No such container: name' >&2; exit 1 ;;
          unreadable) echo 'permission denied' >&2; exit 1 ;;
          malformed) printf '{}\n'; exit 0 ;;
        esac
      fi
      nonce=$(cat "$base/nonce")
      if [ -e "$base/foreign" ]; then nonce=foreign; fi
      sed -e "s/@NAME@/$(cat "$base/name")/g" -e "s/@NONCE@/$nonce/g" "$base/helper.json"
      exit 0
    fi
    case "$(cat "$base/cleanup")" in
      survivor) printf '[{"Id":"survivor"}]\n' ;;
      unreadable) echo 'permission denied' >&2; exit 1 ;;
      *) echo 'Error: No such container: immutable-id' >&2; exit 1 ;;
    esac ;;
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
        format!("container inspect {:064}", 1)
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

#[test]
fn successful_remove_without_proven_absence_is_not_successful_accounting() {
    for mode in ["survivor", "unreadable"] {
        let (_dir, backend) = fixture(true, mode);
        let error = measure(&backend).unwrap_err();
        assert!(error.contains("absence is unproven"), "{error}");
        assert!(error.contains(&format!("{:064}", 1)));
    }
}

#[test]
fn lost_or_invalid_create_acknowledgement_reaps_only_the_verified_helper_id() {
    for mode in ["lost", "invalid"] {
        let (dir, backend) = fixture(true, "removed");
        std::fs::write(dir.path().join("create-mode"), mode).unwrap();
        let error = measure(&backend).unwrap_err();
        assert!(error.contains("recovered"), "{error}");
        let commands = std::fs::read_to_string(dir.path().join("commands")).unwrap();
        assert!(commands.contains(&format!("rm -f -v {:064}", 1)));
        assert!(!commands.contains("rm -f -v bosn-cache-measure-"));
        assert!(!commands.contains("volume rm"));
    }
}

#[test]
fn lost_create_acknowledgement_preserves_a_foreign_helper() {
    let (dir, backend) = fixture(true, "removed");
    std::fs::write(dir.path().join("create-mode"), "lost").unwrap();
    std::fs::write(dir.path().join("foreign"), "").unwrap();
    let error = measure(&backend).unwrap_err();
    assert!(error.contains("needs cleanup"), "{error}");
    assert!(error.contains("identity or isolation"), "{error}");
    let commands = std::fs::read_to_string(dir.path().join("commands")).unwrap();
    assert!(!commands.lines().any(|line| line.starts_with("rm ")));
}

#[test]
fn unresolved_create_side_effects_keep_the_named_cleanup_identity() {
    for mode in ["absent", "unreadable", "malformed"] {
        let (dir, backend) = fixture(true, "removed");
        std::fs::write(dir.path().join("create-mode"), "lost").unwrap();
        std::fs::write(dir.path().join("lookup-mode"), mode).unwrap();
        let error = measure(&backend).unwrap_err();
        assert!(error.contains("needs cleanup"), "{error}");
        let name = std::fs::read_to_string(dir.path().join("name")).unwrap();
        assert!(error.contains(&name), "{error}");
        let commands = std::fs::read_to_string(dir.path().join("commands")).unwrap();
        assert!(!commands.lines().any(|line| line.starts_with("rm ")));
    }
}

#[test]
#[ignore = "requires the isolated bosn-456-live-v2 Docker engine"]
fn real_docker_lost_create_acknowledgement_recovers_the_accounting_helper() {
    assert_eq!(
        std::env::var("DOCKER_HOST").unwrap(),
        "tcp://bosn-456-live-v2-engine:2375"
    );
    let dir = TemporaryDirectory::new().unwrap();
    std::fs::write(
        dir.path().join("docker.sh"),
        r#"
base=$1
shift
printf '%s\n' "$*" >> "$base/commands"
if [ "$1" = create ]; then
  /usr/local/bin/docker "$@" > "$base/created-id" || exit
  echo 'simulated acknowledgement loss after real create' >&2
  exit 1
fi
exec /usr/local/bin/docker "$@"
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
    let error = measure(&backend).unwrap_err();
    assert!(error.contains("recovered"), "{error}");
    let id = std::fs::read_to_string(dir.path().join("created-id")).unwrap();
    let commands = std::fs::read_to_string(dir.path().join("commands")).unwrap();
    assert!(commands.contains(&format!("rm -f -v {}", id.trim())));
    assert!(!commands.contains("volume rm"));
    // A subsequent real sample proves the retained cache is still present
    // and exercises successful start/auto-removal with the new helper identity.
    let real = DockerActBackend::new(DockerEngine::docker());
    let report = measure(&real).unwrap();
    assert!(!report.partial, "{:?}", report.errors);
    assert!(report.allocated_bytes.is_some_and(|bytes| bytes > 0));
}
