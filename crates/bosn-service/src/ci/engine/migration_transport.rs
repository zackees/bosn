//! Bounded host side of the continuously leased migration protocol.
use super::process_control::ProcessControl;
use super::{ImportAttempt, InventoryAttempt};
use crate::ci::{
    cache_cohort::Namespace,
    cache_import::{ImportReport, PublicationReceipt},
    cache_maintenance::NamespaceAudit,
    cache_policy::CachePolicy,
    cache_routing::RoutingRecord,
};
use crate::{
    RegistryActor,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};
use bosn_registry::cache_migration::{CacheMigrationIntent, CachePublicationEvidence};
use kernal_api::{ProcessSession, async_engine};
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

#[cfg(all(test, unix))]
mod resume_tests;

pub struct CacheMigrationSession {
    io: ProcessControl,
    namespace: Namespace,
    policy: CachePolicy,
    imported: bool,
    import_evidence: Option<(u64, u64, Option<u64>)>,
    import_started: bool,
    audited: bool,
    receipt: Option<PublicationReceipt>,
    journal: Option<(RegistryActor, String)>,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn recovery_reads_receipt_without_import_and_requires_matching_intent() {
        for mode in [
            "unacknowledged",
            "committed",
            "conflict",
            "ceiling",
            "missing",
        ] {
            crate::ci::lifecycle::tests::with_registry(|registry, state| async move {
                let namespace = Namespace::parse("0123456789abcdef").unwrap();
                let policy = CachePolicy::default();
                let nonce = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
                if mode != "missing" {
                    registry
                        .act_registry(ActRegistryCommand::CacheMigrationBegin(
                            CacheMigrationIntent {
                                namespace: namespace.as_str().into(),
                                nonce: nonce.into(),
                                max_bytes: policy.repository_max_bytes,
                                created_at: 1.0,
                            },
                        ))
                        .await
                        .unwrap();
                    if mode != "unacknowledged" {
                        registry
                            .act_registry(ActRegistryCommand::CacheMigrationPublished {
                                namespace: namespace.as_str().into(),
                                nonce: nonce.into(),
                                at: 2.0,
                                proof: CachePublicationEvidence {
                                    source_fingerprint: "a".repeat(64),
                                    imported_count: 1,
                                    imported_bytes: 80,
                                    retained_source_archive_bytes: 80,
                                },
                            })
                            .await
                            .unwrap();
                    }
                }
                let receipt = serde_json::json!({
                    "schema_version":1,"source":namespace.legacy_path(),"destination":namespace.path(),
                    "source_fingerprint":if mode == "conflict" {"b".repeat(64)} else {"a".repeat(64)},
                    "max_bytes":policy.repository_max_bytes-i64::from(mode == "ceiling"),
                    "retained_source_archive_bytes":80,"imported_count":1,"imported_bytes":80,
                    "receipts":null,"receipts_omitted":1
                }).to_string();
                let marker = state.join("receipt-read");
                let script = "import pathlib,sys\nprint('bosn-migration-ready',flush=True)\noperation=sys.stdin.readline()\nif operation:\n assert operation=='receipt\\n',operation\n pathlib.Path(sys.argv[2]).write_text(operation)\n print(sys.argv[1]+'\\n\\nbosn-migration-end:0',flush=True)\n";
                let engine = bosn_engine::DockerEngine::synthetic_for_test(
                    "python3",
                    ["-c", script, &receipt, marker.to_str().unwrap()],
                );
                let process = engine
                    .spawn_interactive(Duration::from_secs(5))
                    .await
                    .unwrap();
                let mut control = CacheMigrationSession::ready(process, namespace, policy)
                    .await
                    .unwrap();
                let recovered = control.reconcile_recorded(&registry).await;
                assert_eq!(
                    recovered.is_ok(),
                    matches!(mode, "unacknowledged" | "committed"),
                    "{mode}"
                );
                assert_eq!(marker.exists(), mode != "missing");
                assert!(
                    control
                        .import_recorded(&registry)
                        .await
                        .unwrap_err()
                        .contains("already attempted")
                );
                assert!(
                    control.publish().await.unwrap_err().contains("unverified"),
                    "fresh audit was skipped"
                );
            });
        }
    }

    #[test]
    fn durable_intent_precedes_import_and_existing_intent_blocks_a_new_session() {
        crate::ci::lifecycle::tests::with_registry(|registry, state| async move {
            let namespace = Namespace::parse("0123456789abcdef").unwrap();
            let script = r#"
import pathlib, sqlite3, sys
print('bosn-migration-ready',flush=True)
if sys.stdin.readline() == 'bootstrap\n':
    db = sqlite3.connect('file:' + sys.argv[1] + '?mode=ro', uri=True)
    row = db.execute('SELECT value FROM meta WHERE key=?', ('ci.cache-migration.v1:0123456789abcdef',)).fetchone()
    assert row is not None, 'import ran before durable intent'
    print('existing\nbosn-migration-end:0',flush=True)
    assert sys.stdin.readline() == 'import\n'
    pathlib.Path(sys.argv[2]).write_text(row[0])
    print('{}\n\nbosn-migration-end:0',flush=True)
"#;
            for index in 0..2 {
                let marker = state.join(format!("import-{index}"));
                let database = state.join("registry.sqlite3");
                let engine = bosn_engine::DockerEngine::synthetic_for_test(
                    "python3",
                    [
                        "-c",
                        script,
                        database.to_str().unwrap(),
                        marker.to_str().unwrap(),
                    ],
                );
                let process = engine
                    .spawn_interactive(Duration::from_secs(5))
                    .await
                    .unwrap();
                let mut control = CacheMigrationSession::ready(
                    process,
                    namespace.clone(),
                    CachePolicy::default(),
                )
                .await
                .unwrap();
                assert!(control.import_recorded(&registry).await.is_err());
                assert_eq!(marker.exists(), index == 0, "blind retry entered import");
                let reply = registry
                    .act_registry(ActRegistryCommand::CacheMigrationGet {
                        namespace: namespace.as_str().into(),
                    })
                    .await
                    .unwrap();
                let ActRegistryReply::CacheMigration(Some(record)) = reply else {
                    panic!("intent missing")
                };
                assert!(record.publication.is_none());
            }
        });
    }

    #[test]
    fn receipt_must_match_the_import_observed_in_this_session() {
        kernal_api::async_engine::RuntimeBuilder::multi_thread().enable_all().build().unwrap().run(async {
            let namespace = Namespace::parse("0123456789abcdef").unwrap();
            let policy = CachePolicy::default();
            let report = serde_json::json!({
                "schema_version":1,"source":namespace.legacy_path(),"destination":namespace.path(),
                "published":true,"partial":false,"error":null,"pending_stage":null,
                "retained_source_archive_bytes":80,"available_destination_bytes":1000000,
                "required_additional_bytes":1000,"imported_count":1,"imported_bytes":80,
                "skipped_incomplete":0,"skipped_budget":0,"receipts":null,"receipts_omitted":1
            }).to_string();
            for mode in ["matching", "count", "bytes", "retained", "ceiling"] {
                let count = if mode == "count" {2} else {1};
                let bytes = if mode == "bytes" {79} else {80};
                let retained = if mode == "retained" {100} else {80};
                let ceiling = policy.repository_max_bytes - i64::from(mode == "ceiling");
                let receipt = serde_json::json!({
                    "schema_version":1,"source":namespace.legacy_path(),"destination":namespace.path(),
                    "source_fingerprint":"a".repeat(64),"max_bytes":ceiling,
                    "retained_source_archive_bytes":retained,"imported_count":count,
                    "imported_bytes":bytes,"receipts":null,"receipts_omitted":count
                }).to_string();
                let script = "import sys\nprint('bosn-migration-ready',flush=True)\nfor report in sys.argv[1:]:\n sys.stdin.readline()\n print(report+'\\n\\nbosn-migration-end:0',flush=True)\n";
                let engine = bosn_engine::DockerEngine::synthetic_for_test("python3", ["-c", script, &report, &receipt]);
                let process = engine.spawn_interactive(Duration::from_secs(5)).await.unwrap();
                let mut control = CacheMigrationSession::ready(process, namespace.clone(), policy).await.unwrap();
                control.import().await.unwrap().require_warm_publication().unwrap();
                let evidence = control.receipt().await;
                if mode == "matching" { assert!(evidence.is_ok()); }
                else { assert!(evidence.unwrap_err().contains("disagrees"), "accepted {mode}"); }
                assert!(control.publish().await.unwrap_err().contains("unverified"));
            }
        });
    }

    #[test]
    fn malformed_truncated_and_oversized_transport_cannot_publish_or_repeat_import() {
        kernal_api::async_engine::RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                for mode in [
                    "truncated",
                    "oversized",
                    "invalid-exit",
                    "invalid-report",
                    "deadline",
                ] {
                    let script = r#"
import sys
print('bosn-migration-ready', flush=True)
sys.stdin.readline()
mode = sys.argv[1]
if mode == 'oversized': print('x' * 70000, flush=True)
elif mode == 'invalid-exit': print('{}\n\nbosn-migration-end:999', flush=True)
elif mode == 'invalid-report': print('{}\n\nbosn-migration-end:0', flush=True)
"#;
                    let engine = bosn_engine::DockerEngine::synthetic_for_test(
                        "python3",
                        ["-c", script, mode],
                    );
                    let process = engine
                        .spawn_interactive(Duration::from_secs(5))
                        .await
                        .unwrap();
                    let mut control = CacheMigrationSession::ready(
                        process,
                        Namespace::parse("0123456789abcdef").unwrap(),
                        CachePolicy::default(),
                    )
                    .await
                    .unwrap();
                    control.io.expires = Instant::now() + Duration::from_secs(5);
                    if mode == "deadline" {
                        control.io.expires = Instant::now();
                    }
                    assert!(control.import().await.is_err(), "accepted {mode}");
                    let repeated = control.import().await.unwrap_err();
                    assert!(repeated.contains("already attempted"), "{repeated}");
                    let publication = control.publish().await.unwrap_err();
                    assert!(publication.contains("unverified"), "{publication}");
                }
            });
    }
}

impl CacheMigrationSession {
    pub(super) async fn ready(
        session: ProcessSession,
        namespace: Namespace,
        policy: CachePolicy,
    ) -> Result<Self, String> {
        let mut control = Self {
            io: ProcessControl::new(
                session,
                "migration",
                b"bosn-migration-end:",
                Duration::from_secs(150),
            ),
            namespace,
            policy,
            imported: false,
            import_evidence: None,
            import_started: false,
            audited: false,
            receipt: None,
            journal: None,
        };
        match control.line().await?.as_slice() {
            b"bosn-migration-ready" => {}
            b"bosn-migration-busy" => return Err(super::migration_session::BUSY.into()),
            _ => return Err("migration lease acknowledgement is invalid".into()),
        }
        Ok(control)
    }

    fn remaining(&self) -> Result<Duration, String> {
        self.io.remaining()
    }

    async fn send(&self, bytes: &[u8]) -> Result<(), String> {
        self.io.send(bytes).await
    }

    async fn line(&mut self) -> Result<Vec<u8>, String> {
        self.io.line().await
    }

    async fn command(&mut self, command: &[u8]) -> Result<(i32, Vec<u8>), String> {
        self.io.command(command).await
    }

    /// Commit the actor intent before the remote import can mutate storage.
    /// Existing intents refuse a blind retry and require publication recovery.
    pub async fn import_recorded(
        &mut self,
        registry: &RegistryActor,
    ) -> Result<ImportAttempt, String> {
        if self.import_started || self.journal.is_some() {
            return Err(
                "migration import already attempted; reconcile publication before retrying".into(),
            );
        }
        let nonce = crate::ci::wire::new_uuid()
            .await
            .map_err(|error| error.to_string())?;
        let intent = CacheMigrationIntent {
            namespace: self.namespace.as_str().into(),
            nonce: nonce.clone(),
            max_bytes: self.policy.repository_max_bytes,
            created_at: crate::ci::lifecycle::now_seconds(),
        };
        let reply = async_engine::timeout(
            self.remaining()?,
            registry.act_registry(ActRegistryCommand::CacheMigrationBegin(intent)),
        )
        .await
        .map_err(|_| "migration intent commit deadline exceeded")?
        .map_err(|error| error.to_string())?;
        if !matches!(reply, ActRegistryReply::Committed) {
            return Err("migration intent acknowledgement is invalid".into());
        }
        self.journal = Some((registry.clone(), nonce));
        self.bootstrap().await?;
        self.import().await
    }

    async fn bootstrap(&mut self) -> Result<(), String> {
        let (code, status) = self.command(b"bootstrap\n").await?;
        if code != 0 || !matches!(status.as_slice(), b"existing\n" | b"initialized\n") {
            return Err(
                "migration source initialization failed; durable intent requires reconciliation"
                    .into(),
            );
        }
        Ok(())
    }

    /// The continuous exclusive lease proves the prior remote importer ended.
    /// Retry the same intent only after fresh, complete destination absence;
    /// an existing destination always requires receipt reconciliation.
    pub async fn resume_recorded(&mut self, registry: &RegistryActor) -> Result<(), String> {
        if self.import_started || self.journal.is_some() {
            return Err("migration session already attempted an import".into());
        }
        let reply = async_engine::timeout(
            self.remaining()?,
            registry.act_registry(ActRegistryCommand::CacheMigrationGet {
                namespace: self.namespace.as_str().into(),
            }),
        )
        .await
        .map_err(|_| "migration recovery read deadline exceeded")?
        .map_err(|error| error.to_string())?;
        let ActRegistryReply::CacheMigration(Some(record)) = reply else {
            return Err("migration recovery requires a durable intent".into());
        };
        if record.intent.max_bytes != self.policy.repository_max_bytes {
            return Err("migration recovery ceiling differs from frozen intent".into());
        }
        let (code, status) = self.command(b"destination\n").await?;
        if code != 0 {
            return Err("migration destination read failed; publication remains unresolved".into());
        }
        match status.as_slice() {
            b"present\n" => {
                self.reconcile_recorded(registry).await?;
            }
            b"absent\n" if record.publication.is_none() => {
                self.journal = Some((registry.clone(), record.intent.nonce));
                self.bootstrap().await?;
                self.import().await?.require_warm_publication()?;
                self.receipt().await?;
            }
            b"absent\n" => return Err(
                "published cache destination disappeared; refusing to overwrite committed evidence"
                    .into(),
            ),
            _ => return Err("migration destination read is incomplete".into()),
        }
        Ok(())
    }

    async fn import(&mut self) -> Result<ImportAttempt, String> {
        if self.import_started {
            return Err(
                "migration import already attempted; reconcile publication before retrying".into(),
            );
        }
        self.import_started = true;
        self.imported = false;
        self.import_evidence = None;
        self.audited = false;
        self.receipt = None;
        let (exit_code, bytes) = self.command(b"import\n").await?;
        let report = ImportReport::parse(&bytes, &self.namespace, self.policy)?;
        let attempt = ImportAttempt {
            exit_code,
            report,
            diagnostic: self.io.diagnostic(),
        };
        self.imported = attempt.require_warm_publication().is_ok();
        if self.imported {
            self.import_evidence = Some((
                attempt.report.imported_count,
                attempt.report.imported_bytes,
                attempt.report.retained_source_archive_bytes,
            ));
        }
        Ok(attempt)
    }

    /// Recover a prior import by reading its receipt, never by repeating import.
    /// Fresh inventory is still required before durable route publication.
    pub async fn reconcile_recorded(
        &mut self,
        registry: &RegistryActor,
    ) -> Result<&PublicationReceipt, String> {
        if self.import_started || self.journal.is_some() {
            return Err("migration session already attempted an import".into());
        }
        self.import_started = true;
        let reply = async_engine::timeout(
            self.remaining()?,
            registry.act_registry(ActRegistryCommand::CacheMigrationGet {
                namespace: self.namespace.as_str().into(),
            }),
        )
        .await
        .map_err(|_| "migration recovery read deadline exceeded")?
        .map_err(|error| error.to_string())?;
        let ActRegistryReply::CacheMigration(Some(record)) = reply else {
            return Err("migration recovery requires a durable intent".into());
        };
        if record.intent.max_bytes != self.policy.repository_max_bytes {
            return Err("migration recovery ceiling differs from frozen intent".into());
        }
        let receipt = self.read_receipt().await?;
        if receipt.max_bytes != record.intent.max_bytes {
            return Err("migration recovery receipt ceiling differs from frozen intent".into());
        }
        let proof = CachePublicationEvidence {
            source_fingerprint: receipt.source_fingerprint.clone(),
            imported_count: receipt.imported_count,
            imported_bytes: receipt.imported_bytes,
            retained_source_archive_bytes: receipt
                .retained_source_archive_bytes
                .ok_or("migration retained-source evidence missing")?,
        };
        if record
            .publication
            .as_ref()
            .is_some_and(|previous| previous != &proof)
        {
            return Err("migration recovery receipt conflicts with committed evidence".into());
        }
        self.import_evidence = Some((
            receipt.imported_count,
            receipt.imported_bytes,
            receipt.retained_source_archive_bytes,
        ));
        self.imported = true;
        self.audited = false;
        self.journal = Some((registry.clone(), record.intent.nonce));
        self.receipt = Some(receipt);
        self.receipt
            .as_ref()
            .ok_or_else(|| "migration recovery receipt missing".into())
    }

    async fn read_receipt(&mut self) -> Result<PublicationReceipt, String> {
        let (code, bytes) = self.command(b"receipt\n").await?;
        if code != 0 {
            return Err("migration receipt command failed".into());
        }
        PublicationReceipt::parse(&bytes, &self.namespace, self.policy)
    }

    pub async fn receipt(&mut self) -> Result<&PublicationReceipt, String> {
        self.receipt = None;
        let receipt = self.read_receipt().await?;
        if receipt.max_bytes != self.policy.repository_max_bytes
            || self.import_evidence
                != Some((
                    receipt.imported_count,
                    receipt.imported_bytes,
                    receipt.retained_source_archive_bytes,
                ))
        {
            return Err("migration receipt disagrees with the observed import".into());
        }
        self.receipt = Some(receipt);
        self.receipt
            .as_ref()
            .ok_or_else(|| "migration receipt missing".into())
    }

    pub async fn audit(&mut self) -> Result<InventoryAttempt, String> {
        self.audited = false;
        let (exit_code, bytes) = self.command(b"audit\n").await?;
        let report = NamespaceAudit::parse_inventory(&bytes, &self.namespace)?;
        let attempt = InventoryAttempt { exit_code, report };
        self.audited = attempt.require_current_inventory().is_ok()
            && self.import_evidence.is_some_and(|(count, bytes, _)| {
                attempt
                    .report
                    .entry_count
                    .is_some_and(|current| current >= count)
                    && attempt
                        .report
                        .archive_bytes
                        .is_some_and(|current| current >= bytes)
            });
        Ok(attempt)
    }

    /// Publication follows durable evidence; the caller must exclude old writers.
    pub async fn publish(mut self) -> Result<(), String> {
        if !self.imported || !self.audited {
            return Err("migration import or current inventory is unverified".into());
        }
        let receipt = self
            .receipt
            .as_ref()
            .ok_or("migration receipt is unverified")?;
        let record = RoutingRecord::new(&self.namespace, self.policy, &receipt.source_fingerprint)?;
        let (registry, nonce) = self
            .journal
            .as_ref()
            .ok_or("migration journal is unverified")?;
        let proof = CachePublicationEvidence {
            source_fingerprint: receipt.source_fingerprint.clone(),
            imported_count: receipt.imported_count,
            imported_bytes: receipt.imported_bytes,
            retained_source_archive_bytes: receipt
                .retained_source_archive_bytes
                .ok_or("migration retained-source evidence missing")?,
        };
        let reply = async_engine::timeout(
            self.remaining()?,
            registry.act_registry(ActRegistryCommand::CacheMigrationPublished {
                namespace: self.namespace.as_str().into(),
                nonce: nonce.clone(),
                proof,
                at: crate::ci::lifecycle::now_seconds(),
            }),
        )
        .await
        .map_err(|_| "migration publication commit deadline exceeded")?
        .map_err(|error| error.to_string())?;
        if !matches!(reply, ActRegistryReply::Committed) {
            return Err("migration publication commit acknowledgement is invalid".into());
        }
        let mut bytes = b"publish\n".to_vec();
        bytes.extend(record.encode()?);
        bytes.push(b'\n');
        self.send(&bytes).await?;
        if self.line().await? != b"bosn-migration-published" {
            return Err("migration publication acknowledgement is invalid".into());
        }
        let exit = async_engine::timeout(self.remaining()?, self.io.session.wait())
            .await
            .map_err(|_| "migration publication exit deadline exceeded")?
            .map_err(|error| error.to_string())?;
        if !exit.is_success() {
            return Err("migration publication process failed".into());
        }
        Ok(())
    }
}
