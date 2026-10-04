//! Actual published-binary import through Bosn's Docker transport.
use super::*;
use crate::ci::cache_import::PublicationReceipt;
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
            let attempt = backend.import_cache_for_quiescent_source(id, &namespace, policy).await?;
            attempt.require_warm_publication()?;
            if attempt.report.imported_count != 1 || attempt.report.imported_bytes != 80 {
                return Err("unexpected warm import count or bytes".into());
            }
            let receipt = backend.checked("historical receipt", owned(&["exec", id, "/var/lib/docker/bosn-ci/bin/act", "cache", "import-receipt", "--cache-server-path", &namespace.path()]), super::super::CONTROL_DEADLINE).await?;
            let parsed = PublicationReceipt::parse(receipt.as_bytes(), &namespace, policy)?;
            if parsed.imported_bytes != 80 {
                return Err("unexpected historical receipt bytes".into());
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
