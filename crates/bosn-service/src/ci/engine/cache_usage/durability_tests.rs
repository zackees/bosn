use super::super::cache_usage_transport_tests::fixture;
use super::*;
use crate::ci::lifecycle::tests::{OWNER, with_registry};
use bosn_registry::{
    Registry,
    cache_helper::{CacheHelperIntent, CacheHelperState},
};

const NONCE: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const ID: &str = "0000000000000000000000000000000000000000000000000000000000000001";

fn intent() -> CacheHelperIntent {
    CacheHelperIntent {
        registry_id: OWNER.into(),
        nonce: NONCE.into(),
        image: engine_image(),
        volume: super::super::CACHE_VOLUME.into(),
        created_at: 1.0,
        role: None,
    }
}

fn seed_observation(dir: &std::path::Path, value: &CacheHelperIntent) {
    std::fs::write(dir.join("name"), value.name()).unwrap();
    std::fs::write(dir.join("nonce"), &value.nonce).unwrap();
    let labels = bosn_core::ResourceLabels::new(
        &value.registry_id,
        bosn_core::ResourceKind::Container,
        "ci-cache-measurement",
        &value.nonce,
        bosn_core::Scope::Spec,
        "machine",
        &value.created_at.to_string(),
        Some(bosn_core::Retention::Warm),
    )
    .unwrap();
    let mut labels: std::collections::BTreeMap<String, String> = labels
        .to_map()
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect();
    labels.insert("io.bosn.cache.measurement".into(), "@NONCE@".into());
    let document = serde_json::json!([{"Id": ID, "Name": "/@NAME@",
        "Config": {"Image": value.image, "Labels": labels},
        "HostConfig": {"ReadonlyRootfs": true, "Privileged": false, "NetworkMode": "none"},
        "Mounts": [{"Type": "volume", "Name": value.volume, "Destination": "/cache", "RW": false}]}]);
    std::fs::write(dir.join("helper.json"), document.to_string()).unwrap();
}

#[test]
fn uncertain_absence_stays_pending_and_a_later_create_is_reaped() {
    with_registry(|registry, state| async move {
        let value = intent();
        journal::Tracker::new(&registry, NONCE)
            .begin(value.clone())
            .await
            .unwrap();
        let (dir, backend) = fixture(true, "removed");
        seed_observation(dir.path(), &value);
        std::fs::write(dir.path().join("lookup-mode"), "absent").unwrap();
        let deferred = backend
            .retry_measurements(&registry, OWNER, None)
            .await
            .unwrap();
        assert!(deferred.deferred.is_some());
        assert!(deferred.removed.is_none());
        let reader = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
        assert_eq!(
            reader.cache_helper(NONCE).unwrap().unwrap().state,
            CacheHelperState::Pending
        );
        // A later pass, rather than absence alone, observes the create side effect.
        std::fs::write(dir.path().join("lookup-mode"), "ready").unwrap();
        let recovered = backend
            .retry_measurements(&registry, OWNER, None)
            .await
            .unwrap();
        assert_eq!(recovered.removed.as_deref(), Some(value.name().as_str()));
        let record = reader.cache_helper(NONCE).unwrap().unwrap();
        assert_eq!(record.state, CacheHelperState::Removed);
        assert_eq!(record.container_id.as_deref(), Some(ID));
        let commands = std::fs::read_to_string(dir.path().join("commands")).unwrap();
        assert!(commands.contains(&format!("rm -f -v {ID}")));
        assert!(!commands.contains("volume rm"));
    });
}

#[test]
fn cleanup_skips_active_helpers_and_retries_after_the_claim_is_released() {
    with_registry(|registry, _| async move {
        let value = intent();
        journal::Tracker::new(&registry, NONCE)
            .begin(value.clone())
            .await
            .unwrap();
        let (dir, backend) = fixture(true, "removed");
        seed_observation(dir.path(), &value);
        let active = journal::ActiveHelper::claim(&backend, NONCE);
        let skipped = backend
            .retry_measurements(&registry, OWNER, None)
            .await
            .unwrap();
        assert!(skipped.removed.is_none());
        assert!(!dir.path().join("commands").exists());
        drop(active);
        assert!(
            backend
                .retry_measurements(&registry, OWNER, None)
                .await
                .unwrap()
                .removed
                .is_some()
        );
    });
}

#[test]
fn registered_id_absence_finishes_cleanup_but_a_wrong_owner_cannot() {
    with_registry(|registry, state| async move {
        let value = intent();
        let tracker = journal::Tracker::new(&registry, NONCE);
        tracker.begin(value).await.unwrap();
        tracker.register(ID).await.unwrap();
        let (dir, backend) = fixture(true, "absent");
        std::fs::write(dir.path().join("name"), "unrelated").unwrap();
        assert!(
            backend
                .retry_measurements(&registry, NONCE, None)
                .await
                .is_err()
        );
        assert!(!dir.path().join("commands").exists());
        let recovered = backend
            .retry_measurements(&registry, OWNER, None)
            .await
            .unwrap();
        assert!(recovered.removed.is_some());
        let reader = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
        assert_eq!(
            reader.cache_helper(NONCE).unwrap().unwrap().state,
            CacheHelperState::Removed
        );
        let commands = std::fs::read_to_string(dir.path().join("commands")).unwrap();
        assert!(!commands.lines().any(|line| line.starts_with("rm ")));
    });
}

#[test]
fn a_foreign_labelled_helper_is_preserved_with_its_pending_intent() {
    with_registry(|registry, state| async move {
        let mut value = intent();
        journal::Tracker::new(&registry, NONCE)
            .begin(value.clone())
            .await
            .unwrap();
        value.registry_id = NONCE.into();
        let (dir, backend) = fixture(true, "removed");
        seed_observation(dir.path(), &value);
        let pass = backend
            .retry_measurements(&registry, OWNER, None)
            .await
            .unwrap();
        assert!(pass.deferred.is_some());
        assert!(pass.removed.is_none());
        let reader = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
        assert_eq!(
            reader.cache_helper(NONCE).unwrap().unwrap().state,
            CacheHelperState::Pending
        );
        let commands = std::fs::read_to_string(dir.path().join("commands")).unwrap();
        assert!(!commands.lines().any(|line| line.starts_with("rm ")));
    });
}

#[test]
#[ignore = "requires the isolated bosn-456-live-v2 Docker engine"]
fn real_orphaned_helper_is_recovered_from_its_durable_intent_by_a_new_backend() {
    real_orphaned_helper(false, false);
}

#[test]
#[ignore = "requires the isolated bosn-456-live-v2 Docker engine"]
fn real_orphaned_maintenance_helper_recovers_after_lost_create_acknowledgement() {
    real_orphaned_helper(true, false);
}

#[test]
#[ignore = "requires the isolated bosn-456-live-v2 Docker engine"]
fn cancelled_maintenance_create_recovers_from_durable_intent_without_acknowledgement() {
    real_orphaned_helper(true, true);
}

fn real_orphaned_helper(maintenance: bool, cancel_create: bool) {
    assert_eq!(
        std::env::var("DOCKER_HOST").unwrap(),
        "tcp://bosn-456-live-v2-engine:2375"
    );
    with_registry(|registry, state| async move {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        std::fs::write(
            dir.path().join("docker.sh"),
            r#"
base=$1
shift
if [ "$1" = create ]; then
  /usr/local/bin/docker "$@" > "$base/created-id" || exit
  if [ -f "$base/cancel-create" ]; then sleep 60; fi
  echo 'simulated lost create acknowledgement' >&2
  exit 1
fi
if [ "$1" = container ]; then
  echo 'simulated temporary inspection outage' >&2
  exit 1
fi
exec /usr/local/bin/docker "$@"
"#,
        )
        .unwrap();
        if cancel_create {
            std::fs::write(dir.path().join("cancel-create"), "").unwrap();
        }
        let original = DockerActBackend::new(bosn_engine::DockerEngine::synthetic_for_test(
            "sh",
            [
                dir.path().join("docker.sh").into_os_string(),
                dir.path().as_os_str().to_owned(),
            ],
        ));
        let error = if maintenance {
            let policy = toml::from_str(
                "repository_max_bytes=1073741824\naggregate_max_bytes=2147483648\nmax_age_secs=2592000\nunused_age_secs=604800\nmaintenance_interval_secs=300\n",
            ).unwrap();
            if cancel_create {
                cancel_created_helper(&original, &registry, policy, dir.path()).await
            } else {
                original
                    .maintain_cache_with_helper(&registry, OWNER, policy)
                    .await
                    .unwrap_err()
            }
        } else {
            original
                .measure_cache_tracked(super::super::CACHE_VOLUME, Some((&registry, OWNER)))
                .await
                .unwrap_err()
        };
        assert!(error.contains("needs cleanup"), "{error}");
        let page = registry
            .act_registry(crate::act_registry::ActRegistryCommand::HelperPending {
                after_nonce: None,
                limit: 64,
            })
            .await
            .unwrap();
        let crate::act_registry::ActRegistryReply::Helpers(page) = page else {
            panic!("helper page expected");
        };
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].state, CacheHelperState::Pending);
        if cancel_create {
            assert!(page.items[0].container_id.is_none());
            assert!(original.active_helpers.lock().unwrap().is_empty());
        }
        let nonce = page.items[0].intent.nonce.clone();
        let created_id = std::fs::read_to_string(dir.path().join("created-id")).unwrap();
        let restarted = DockerActBackend::new(bosn_engine::DockerEngine::docker());
        let pass = restarted
            .retry_measurements(&registry, OWNER, None)
            .await
            .unwrap();
        assert!(pass.removed.is_some(), "{:?}", pass.deferred);
        let reader = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
        let record = reader.cache_helper(&nonce).unwrap().unwrap();
        assert_eq!(record.state, CacheHelperState::Removed);
        assert_eq!(record.intent.role.is_some(), maintenance);
        assert_eq!(record.container_id.as_deref(), Some(created_id.trim()));
        restarted
            .confirm_measurement_absent(created_id.trim())
            .await
            .unwrap();
        let sample = restarted
            .measure_cache_tracked(super::super::CACHE_VOLUME, Some((&registry, OWNER)))
            .await
            .unwrap();
        assert!(!sample.partial, "{:?}", sample.errors);
        assert!(sample.allocated_bytes.is_some_and(|bytes| bytes > 0));
    });
}

async fn cancel_created_helper(
    original: &DockerActBackend,
    registry: &crate::RegistryActor,
    policy: crate::ci::cache_policy::CachePolicy,
    directory: &std::path::Path,
) -> String {
    let stop = kernal_api::async_engine::CancellationSource::new();
    let cancelled_at = std::sync::Mutex::new(None);
    let token = stop.token();
    let attempt = kernal_api::async_engine::cancellable(
        &token,
        original.maintain_cache_with_helper(registry, OWNER, policy),
    );
    let observer = async {
        let created =
            kernal_api::async_engine::timeout(std::time::Duration::from_secs(15), async {
                loop {
                    if std::fs::read_to_string(directory.join("created-id"))
                        .is_ok_and(|id| id.trim().len() == 64)
                    {
                        break;
                    }
                    kernal_api::async_engine::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await;
        *cancelled_at.lock().unwrap() = Some(std::time::Instant::now());
        stop.cancel();
        created.expect("real Docker creation must precede cancellation");
    };
    let (result, ()) = kernal_api::async_engine::join(attempt, observer).await;
    assert!(result.is_err(), "create acknowledgement must not arrive");
    let latency = cancelled_at.lock().unwrap().unwrap().elapsed();
    assert!(latency < std::time::Duration::from_secs(5), "{latency:?}");
    eprintln!("cancelled create returned in {latency:?}");
    "cancelled create needs cleanup".to_string()
}
