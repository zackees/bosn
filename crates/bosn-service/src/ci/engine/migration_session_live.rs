//! Actual interactive Docker transport and pinned act import/publication.
use super::*;
use crate::ci::cache_routing::RoutingRecord;
use crate::ci::engine::ActEngineBackend;
use std::path::Path;

#[test]
#[ignore = "requires Docker and a copied closed legacy fixture plus pinned act binary"]
fn pinned_act_migration_session_publishes_after_durable_evidence_and_retires_helper() {
    run_migration(
        Some(std::env::var("BOSN_ACT_IMPORT_SOURCE_TAR").unwrap()),
        Scenario::Manual,
    );
}

#[test]
#[ignore = "requires Docker and the pinned act binary"]
fn pinned_act_fresh_cache_initializes_and_enrolls_without_a_legacy_store() {
    run_migration(None, Scenario::Manual);
}

#[test]
#[ignore = "requires Docker and the pinned act binary"]
fn normal_cache_admission_enrolls_missing_namespace_and_reuses_publication() {
    run_migration(None, Scenario::Automatic);
}

#[test]
#[ignore = "requires Docker and the pinned act binary"]
fn normal_cache_admission_recovers_intent_committed_before_source_bootstrap() {
    run_migration(None, Scenario::Interrupted);
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Scenario {
    Manual,
    Automatic,
    Interrupted,
}

async fn create_fixture(backend: &DockerActBackend, nonce: &str) -> String {
    let image = crate::ci::engine::engine_image();
    let name = format!("bosn-545-migration-{nonce}");
    backend
        .checked(
            "migration fixture",
            owned(&[
                "run",
                "--rm",
                "-d",
                "--name",
                &name,
                "--label",
                &format!("io.bosn.test.migration={nonce}"),
                "--pull",
                "never",
                "--network",
                "none",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--memory",
                "128m",
                "--cpus",
                "1",
                "--tmpfs",
                "/bosn/cache:exec",
                "--tmpfs",
                "/var/lib/docker:exec",
                "--entrypoint",
                "sleep",
                &image,
                "300",
            ]),
            Duration::from_secs(30),
        )
        .await
        .unwrap()
}

async fn automatic_admission(
    backend: &DockerActBackend,
    registry: &crate::RegistryActor,
    id: &str,
    namespace: &Namespace,
    policy: CachePolicy,
) -> Result<(), String> {
    let planned = crate::ci::engine::ActInvocation {
        event: "push".into(),
        workflow: "workflow.yml".into(),
        workflow_overlaid: false,
        job: None,
        cache_route: crate::ci::cache_cohort::CacheRoute::Legacy(namespace.clone()),
        cache_policy: policy,
        auto_retention: true,
        secrets: Default::default(),
        params: Default::default(),
    };
    let (first, second) = kernal_api::async_engine::join(
        backend.prepare_cache_route(registry, id, &planned),
        backend.prepare_cache_route(registry, id, &planned),
    )
    .await;
    for admitted in [
        first?,
        second?,
        backend.prepare_cache_route(registry, id, &planned).await?,
    ] {
        if !matches!(admitted, crate::ci::cache_cohort::CacheRoute::Cohort { .. }) {
            return Err("normal CI admission kept unbounded legacy cache routing".into());
        }
    }
    Ok(())
}

fn run_migration(fixture: Option<String>, scenario: Scenario) {
    assert_eq!(std::env::var("BOSN_TEST_ISOLATED").unwrap(), "1");
    let binary = std::env::var("BOSN_ACT_RETENTION_TEST_BINARY").unwrap();
    crate::ci::lifecycle::tests::with_registry(|registry, _state| async move {
        let backend = DockerActBackend::default();
        let nonce = crate::ci::new_uuid().await.unwrap();
        let created = create_fixture(&backend, &nonce).await;
        let id = created.trim();
        let result: Result<(), String> = async {
            backend.stream_in("pinned act", id, Path::new(&binary),
                "mkdir -p /var/lib/docker/bosn-ci/bin; cat > /var/lib/docker/bosn-ci/bin/act; chmod 755 /var/lib/docker/bosn-ci/bin/act").await?;
            let artifact = crate::ci::engine::act_artifact("amd64").ok_or("missing act pin")?;
            let verify = format!("echo '{}  /var/lib/docker/bosn-ci/bin/act' | sha256sum -c -", artifact.binary_sha256);
            backend.checked("verify act pin", owned(&["exec", id, "sh", "-ec", &verify]), Duration::from_secs(15)).await?;
            if let Some(fixture) = &fixture {
                backend.stream_in("closed legacy copy", id, Path::new(fixture),
                    "mkdir -p /bosn/cache/actcache; tar -xf - -C /bosn/cache/actcache").await?;
            }
            let namespace = Namespace::parse("b468095b93ccded4")?;
            let policy = CachePolicy::default();
            if scenario == Scenario::Interrupted {
                registry.act_registry(crate::act_registry::ActRegistryCommand::CacheMigrationBegin(
                    bosn_registry::cache_migration::CacheMigrationIntent {
                        namespace: namespace.as_str().into(), nonce: nonce.clone(),
                        max_bytes: policy.repository_max_bytes, created_at: crate::ci::lifecycle::now_seconds(),
                    }
                )).await.map_err(|error| error.to_string())?;
            }
            if scenario != Scenario::Manual {
                automatic_admission(&backend, &registry, id, &namespace, policy).await?;
            } else {
            backend.agree_cache_policy(id, policy).await?;
            let mut migration = backend.open_cache_migration(id, &namespace, policy).await?;
            migration.import_recorded(&registry).await?.require_warm_publication()?;
            migration.receipt().await?;
            migration.audit().await?.require_current_inventory()?;
            let competing = backend.run(owned(&["exec", id, "sh", "-ec",
                "exec 6>>/bosn/cache/actcache/.bosn-maintenance-v1.lock; flock -x -n 6 || exit 75"]), Duration::from_secs(10)).await?;
            if competing.exit_code != 75 { return Err("maintenance entered leased migration".into()); }
            migration.publish().await?;
            }
            let route = backend.checked("read published route", owned(&["exec", id, "cat", &namespace.routing_record_path()]), Duration::from_secs(10)).await?;
            RoutingRecord::parse(route.as_bytes(), &namespace, policy)?;
            if !matches!(backend.published_cache_route(id, &namespace, policy).await?, crate::ci::cache_cohort::CacheRoute::Cohort { .. }) {
                return Err("execution did not select the published cohort route".into());
            }
            let reply = registry.act_registry(crate::act_registry::ActRegistryCommand::CacheMigrationGet {
                namespace: namespace.as_str().into(),
            }).await.map_err(|error| error.to_string())?;
            let crate::act_registry::ActRegistryReply::CacheMigration(Some(record)) = reply else {
                return Err("durable migration record absent".into());
            };
            if record.publication.is_none() { return Err("route preceded durable publication".into()); }
            Ok(())
        }.await;
        let cleanup = backend
            .checked(
                "retire migration fixture",
                owned(&["rm", "-f", id]),
                Duration::from_secs(30),
            )
            .await;
        let absence = backend
            .run(
                owned(&["container", "inspect", id]),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert!(cleanup.is_ok(), "{cleanup:?}");
        assert!(
            !absence.ok()
                && String::from_utf8_lossy(&absence.stderr)
                    .to_lowercase()
                    .contains("no such container")
        );
        result.unwrap();
    });
}
