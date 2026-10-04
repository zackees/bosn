//! Bounded import execution. Production admission remains guarded.
use super::{DockerActBackend, RunOptions, legacy_lease, owned};
use crate::ci::{cache_cohort::Namespace, cache_import::ImportReport, cache_policy::CachePolicy};
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
            .docker
            .with_args(args)
            .capture_async(RunOptions::bounded(Duration::from_secs(90), 64 * 1024))
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
