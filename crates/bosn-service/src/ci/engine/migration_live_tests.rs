//! Actual published-binary import through Bosn's Docker transport.
use super::*;
use crate::ci::cache_import::PublicationReceipt;
use bosn_registry::cache_migration::CacheMigrationIntent;
use std::path::Path;

#[test]
#[ignore = "requires isolated Docker, verified act2.7 and a quiescent HTTP-seeded legacy fixture"]
fn published_binary_import_preserves_source_and_has_typed_historical_receipt() {
    assert!(
        std::env::var("DOCKER_HOST")
            .unwrap()
            .contains("bosn-456-live-v2-engine")
    );
    let binary = std::env::var("BOSN_ACT_RETENTION_TEST_BINARY").unwrap();
    let fixture = std::env::var("BOSN_ACT_IMPORT_SOURCE_TAR").unwrap();
    kernal_api::async_engine::RuntimeBuilder::multi_thread().enable_all().build().unwrap().run(async {
        let backend = DockerActBackend::default();
        let nonce = crate::ci::new_uuid().await.unwrap();
        let name = format!("bosn-import-proof-{nonce}");
        let image = super::super::engine_image();
        let created = backend.checked("import proof create", owned(&[
            "run", "--rm", "-d", "--name", &name, "--label", &format!("io.bosn.test.import={nonce}"),
            "--pull", "never", "--network", "none", "--read-only", "--cap-drop", "ALL",
            "--memory", "128m", "--cpus", "1", "--tmpfs", "/bosn/cache:exec", "--tmpfs", "/var/lib/docker:exec",
            "--entrypoint", "sleep", &image, "300",
        ]), super::super::CONTROL_DEADLINE).await.unwrap();
        let id = created.trim();
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        let result: Result<(), String> = async {
            backend.stream_in("published act", id, Path::new(&binary), "mkdir -p /var/lib/docker/bosn-ci/bin; cat > /var/lib/docker/bosn-ci/bin/act; chmod 755 /var/lib/docker/bosn-ci/bin/act").await?;
            backend.stream_in("quiescent source", id, Path::new(&fixture), "mkdir -p /bosn/cache/actcache; tar -xf - -C /bosn/cache/actcache").await?;
            let source = "/bosn/cache/actcache/0123456789abcdef";
            let snapshot = format!("cd {source}; find . -type f -exec sha256sum {{}} \\; | sort");
            let before = backend.checked("source before", owned(&["exec", id, "sh", "-ec", &snapshot]), super::super::CONTROL_DEADLINE).await?;
            let namespace = Namespace::parse("0123456789abcdef")?;
            let policy: CachePolicy = toml::from_str("repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=3600\nunused_age_secs=1800\nmaintenance_interval_secs=60\n").map_err(|e| e.to_string())?;
            let directory = kernal_api::platform::fs::TemporaryDirectory::new().map_err(|e| e.to_string())?;
            let path = directory.path().join("registry.sqlite3");
            let mut registry = Registry::create_writer(&path, &nonce).map_err(|e| e.to_string())?;
            let mut tx = registry.begin_immediate().map_err(|e| e.to_string())?;
            tx.begin_cache_migration(&CacheMigrationIntent {
                namespace: namespace.as_str().into(), nonce: nonce.clone(), max_bytes: 100, created_at: 1.0,
            }).map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            let missing = backend.maintain_cache_cohort(id, policy).await?;
            if missing.exit_code == 0 || !missing.report.partial
                || missing.report.remaining_completed_bytes.is_some()
                || missing.require_complete().is_ok() {
                return Err("missing cohort must preserve nonzero partial/unknown evidence".into());
            }
            if backend.reconcile_cache_import_publication(&mut registry, id, &namespace, policy, 2.0).await.is_ok() {
                return Err("missing receipt must leave publication unresolved".into());
            }
            let attempt = backend.import_cache_for_quiescent_source(id, &namespace, policy).await?;
            attempt.require_warm_publication()?;
            if attempt.report.imported_count != 1 || attempt.report.imported_bytes != 80 {
                return Err("unexpected warm import count or bytes".into());
            }
            // Discard command acknowledgement before any journal publication.
            drop(attempt);
            drop(registry);
            let mut registry = Registry::open_writer(&path).map_err(|e| e.to_string())?;
            backend.reconcile_cache_import_publication(&mut registry, id, &namespace, policy, 3.0).await?;
            let record = registry.cache_migration(namespace.as_str()).map_err(|e| e.to_string())?.ok_or("intent lost")?;
            if record.publication.as_ref().is_none_or(|proof| proof.imported_bytes != 80) {
                return Err("historical publication was not recovered durably".into());
            }
            backend.reconcile_cache_import_publication(&mut registry, id, &namespace, policy, 4.0).await?;
            let receipt = backend.checked("historical receipt", owned(&["exec", id, "/var/lib/docker/bosn-ci/bin/act", "cache", "import-receipt", "--cache-server-path", &namespace.path()]), super::super::CONTROL_DEADLINE).await?;
            let parsed = PublicationReceipt::parse(receipt.as_bytes(), &namespace, policy)?;
            if parsed.imported_bytes != 80 {
                return Err("unexpected historical receipt bytes".into());
            }
            let fingerprint = inventory(&backend, id, &namespace, 1, 80).await?;
            // No workflow or cache HTTP server is alive. Expire old imported
            // content through an independent bounded maintenance command.
            let expiry: CachePolicy = toml::from_str("repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=1\nunused_age_secs=1\nmaintenance_interval_secs=1\n").map_err(|e| e.to_string())?;
            let maintenance = backend.maintain_cache_cohort(id, expiry).await?;
            maintenance.require_budget_met()?;
            if maintenance.report.remaining_completed_bytes != Some(0) {
                return Err("idle expired cohort archive was not removed".into());
            }
            let stores = maintenance.report.namespaces.as_ref().ok_or("maintenance namespace evidence missing")?;
            let [store] = stores.as_slice() else { return Err("unexpected maintenance namespaces".into()); };
            let eviction = store.retention.as_ref().ok_or("eviction evidence missing")?;
            if eviction.reclaimed_archive_bytes != 80 || eviction.deleted_count != 1 || store.archive_bytes != Some(0) {
                return Err("idle removal did not have exact archive evidence".into());
            }
            if inventory(&backend, id, &namespace, 0, 0).await? == fingerprint {
                return Err("current inventory did not observe removal independently of history".into());
            }
            let repeated = backend.maintain_cache_cohort(id, expiry).await?;
            repeated.require_budget_met()?;
            if repeated.report.namespaces.as_ref().is_none_or(|stores| stores.iter().any(|store|
                store.retention.as_ref().is_none_or(|report| report.reclaimed_archive_bytes != 0 || report.deleted_count != 0))) {
                return Err("repeated maintenance counted reclamation twice".into());
            }
            let after = backend.checked("source after", owned(&["exec", id, "sh", "-ec", &snapshot]), super::super::CONTROL_DEADLINE).await?;
            if before != after {
                return Err("legacy source changed during import".into());
            }
            Ok(())
        }.await;
        let cleanup = backend.checked("import proof cleanup", owned(&["rm", "-f", id]), super::super::CONTROL_DEADLINE).await;
        let absence = backend.run(owned(&["container", "inspect", id]), super::super::CONTROL_DEADLINE).await.unwrap();
        assert!(cleanup.is_ok(), "{cleanup:?}");
        assert!(!absence.ok() && String::from_utf8_lossy(&absence.stderr).to_ascii_lowercase().contains("no such container"), "helper absence unproven: {id}");
        result.unwrap();
    });
}

async fn inventory(
    backend: &DockerActBackend,
    engine: &str,
    namespace: &Namespace,
    count: u64,
    bytes: u64,
) -> Result<String, String> {
    let current = backend.audit_cache_destination(engine, namespace).await?;
    current.require_current_inventory()?;
    if current.report.entry_count != Some(count) || current.report.archive_bytes != Some(bytes) {
        return Err("unexpected current destination inventory".into());
    }
    Ok(current.report.fingerprint)
}
