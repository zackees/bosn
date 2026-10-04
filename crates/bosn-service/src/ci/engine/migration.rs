//! Bounded import execution. Production admission remains guarded.
#[cfg(all(test, unix))]
#[path = "migration_live_tests.rs"]
mod live_tests;
use super::{DockerActBackend, RunOptions, legacy_lease, owned};
use crate::ci::{
    cache_cohort::Namespace,
    cache_import::{ImportReport, PublicationReceipt},
    cache_policy::CachePolicy,
};
use bosn_registry::{Registry, cache_migration::CachePublicationEvidence};
use std::time::Duration;

/// A command outcome and its publication evidence are independent facts.
#[derive(Debug)]
pub struct ImportAttempt {
    pub exit_code: i32,
    pub report: ImportReport,
    pub diagnostic: String,
}
impl ImportAttempt {
    pub fn require_warm_publication(&self) -> Result<(), String> {
        self.report.require_warm_publication()?;
        if self.exit_code != 0 {
            return Err("import command failed; publication requires reconciliation".into());
        }
        Ok(())
    }
}

impl DockerActBackend {
    /// Recover publication after a lost acknowledgement. The persisted intent
    /// must precede import; a historical receipt does not authorize routing.
    pub async fn reconcile_cache_import_publication(
        &self,
        registry: &mut Registry,
        engine: &str,
        namespace: &Namespace,
        policy: CachePolicy,
        at: f64,
    ) -> Result<(), String> {
        let record = registry
            .cache_migration(namespace.as_str())
            .map_err(|e| e.to_string())?
            .ok_or("cache migration intent missing; publication remains unresolved")?;
        let binary = format!("{}/bin/act", super::ENGINE_WORK);
        let mut args = owned(&[
            "exec",
            engine,
            &binary,
            "cache",
            "import-receipt",
            "--cache-server-path",
        ]);
        args.push(namespace.path());
        let output = self
            .run_bounded(
                args,
                RunOptions::bounded(Duration::from_secs(30), 64 * 1024),
            )
            .await
            .map_err(|e| format!("cache publication recovery remains unresolved: {e}"))?;
        if !output.ok() {
            return Err(format!(
                "cache publication recovery remains unresolved: {}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(256)
                    .collect::<String>()
            ));
        }
        let receipt = PublicationReceipt::parse(&output.stdout, namespace, policy)?;
        if receipt.max_bytes != record.intent.max_bytes {
            return Err("historical import ceiling differs from frozen migration intent".into());
        }
        let proof = CachePublicationEvidence {
            source_fingerprint: receipt.source_fingerprint,
            imported_count: receipt.imported_count,
            imported_bytes: receipt.imported_bytes,
            retained_source_archive_bytes: receipt
                .retained_source_archive_bytes
                .ok_or("unknown retained source")?,
        };
        let mut tx = registry.begin_immediate().map_err(|e| e.to_string())?;
        tx.record_cache_publication(namespace.as_str(), &record.intent.nonce, &proof, at)
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// Call only after persisting migration intent and excluding older peers.
    /// The exclusive lifetime lease excludes participating servers. It cannot
    /// establish exclusion of older writers that do not use that protocol.
    /// Transport errors mean unknown publication, never destination absence.
    pub async fn import_cache_for_quiescent_source(
        &self,
        engine: &str,
        namespace: &Namespace,
        policy: CachePolicy,
    ) -> Result<ImportAttempt, String> {
        let mut args = owned(&["exec", engine]);
        args.extend(legacy_lease::migration_command());
        args.extend(policy.import_args_for_quiescent_source(namespace));
        let result = self
            .run_bounded(
                args,
                RunOptions::bounded(Duration::from_secs(90), 64 * 1024),
            )
            .await
            .map_err(|error| format!("cache import publication is unknown: {error}"))?;
        let diagnostic: String = String::from_utf8_lossy(&result.stderr)
            .chars()
            .take(256)
            .collect();
        // Even nonzero exits can carry published-but-partial reports. Do not
        // discard stdout through `checked`, or infer safe retry from an error.
        let report = ImportReport::parse(&result.stdout, namespace, policy).map_err(|error| {
            format!("cache import has no valid publication evidence: {error}; {diagnostic}")
        })?;
        Ok(ImportAttempt {
            exit_code: result.exit_code,
            report,
            diagnostic,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use bosn_engine::DockerEngine;
    use kernal_api::{async_engine::RuntimeBuilder, platform::fs::TemporaryDirectory};

    fn execute(report: &str, code: i32) -> Result<ImportAttempt, String> {
        let directory = TemporaryDirectory::new().unwrap();
        let script = directory.path().join("docker.py");
        std::fs::write(
            &script,
            "import sys\nprint(sys.argv[1])\nprint('publication diagnostic', file=sys.stderr)\nsys.exit(int(sys.argv[2]))\n",
        ).unwrap();
        let backend = DockerActBackend::new(DockerEngine::synthetic_for_test(
            "python3",
            [
                script.into_os_string(),
                report.into(),
                code.to_string().into(),
            ],
        ));
        let policy = toml::from_str("repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=3600\nunused_age_secs=1800\nmaintenance_interval_secs=60\n").unwrap();
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(backend.import_cache_for_quiescent_source(
                "immutable-engine",
                &Namespace::parse("0123456789abcdef").unwrap(),
                policy,
            ))
    }

    fn report(partial: bool) -> String {
        serde_json::json!({
            "schema_version":1,"source":"/bosn/cache/actcache/0123456789abcdef",
            "destination":"/bosn/cache/actcache/cohort-v1/0123456789abcdef",
            "published":true,"partial":partial,"error":if partial {Some("parent fsync failed")} else {None},
            "retained_source_archive_bytes":80,"available_destination_bytes":1000000000,
            "required_additional_bytes":67108864,"imported_count":1,"imported_bytes":80,
            "skipped_incomplete":0,"skipped_budget":0,"receipts_omitted":0,
            "receipts":[{"source_id":1,"destination_id":1,"bytes":80,"sha256":"a".repeat(64)}]
        }).to_string()
    }

    #[test]
    fn nonzero_exit_preserves_partial_publication_for_reconciliation() {
        let attempt = execute(&report(true), 1).unwrap();
        assert!(attempt.report.published);
        assert!(attempt.report.partial);
        assert_eq!(attempt.exit_code, 1);
        assert!(attempt.diagnostic.contains("publication diagnostic"));
        assert!(attempt.require_warm_publication().is_err());
    }

    #[test]
    fn command_success_and_complete_evidence_are_both_required() {
        execute(&report(false), 0)
            .unwrap()
            .require_warm_publication()
            .unwrap();
        let failed = execute(&report(false), 1).unwrap();
        assert!(failed.report.published);
        assert!(failed.require_warm_publication().is_err());
        let error = execute("", 75).unwrap_err();
        assert!(error.contains("no valid publication evidence"));
        assert!(error.contains("publication diagnostic"));
    }
}
