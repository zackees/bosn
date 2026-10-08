//! Recovery cannot turn a failed read or committed destination loss into import.
use super::*;

const SCRIPT: &str = r#"
import pathlib,sys
mode, marker = sys.argv[1], pathlib.Path(sys.argv[2])
print('bosn-migration-ready', flush=True)
for operation in sys.stdin:
    with marker.open('a') as file: file.write(operation)
    if operation == 'destination\n':
        status = {'present':'present', 'committed-absent':'absent', 'partial':'abs', 'failed':'absent'}[mode]
        print(status + '\nbosn-migration-end:' + ('1' if mode == 'failed' else '0'), flush=True)
    elif operation == 'receipt\n':
        print('{}\nbosn-migration-end:0', flush=True)
    else:
        raise AssertionError('unverified retry: ' + operation)
"#;

#[test]
fn unsafe_destination_evidence_never_repeats_import_or_changes_the_intent() {
    for mode in [
        "present",
        "committed-absent",
        "partial",
        "failed",
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
                            max_bytes: policy.repository_max_bytes - i64::from(mode == "ceiling"),
                            created_at: 1.0,
                        },
                    ))
                    .await
                    .unwrap();
                if mode == "committed-absent" {
                    registry
                        .act_registry(ActRegistryCommand::CacheMigrationPublished {
                            namespace: namespace.as_str().into(),
                            nonce: nonce.into(),
                            at: 2.0,
                            proof: CachePublicationEvidence {
                                source_fingerprint: "a".repeat(64),
                                imported_count: 0,
                                imported_bytes: 0,
                                retained_source_archive_bytes: 0,
                            },
                        })
                        .await
                        .unwrap();
                }
            }
            let marker = state.join("recovery-operations");

            let engine = bosn_engine::DockerEngine::synthetic_for_test(
                "python3",
                ["-c", SCRIPT, mode, marker.to_str().unwrap()],
            );
            let process = engine
                .spawn_interactive(Duration::from_secs(5))
                .await
                .unwrap();
            let mut migration = CacheMigrationSession::ready(process, namespace.clone(), policy)
                .await
                .unwrap();
            assert!(
                migration.resume_recorded(&registry).await.is_err(),
                "{mode}"
            );
            assert!(
                migration
                    .publish()
                    .await
                    .unwrap_err()
                    .contains("unverified")
            );
            let operations = std::fs::read_to_string(marker).unwrap_or_default();
            assert_eq!(
                operations,
                match mode {
                    "missing" | "ceiling" => "",
                    "present" => "destination\nreceipt\n",
                    _ => "destination\n",
                },
                "{mode}"
            );
            let reply = registry
                .act_registry(ActRegistryCommand::CacheMigrationGet {
                    namespace: namespace.as_str().into(),
                })
                .await
                .unwrap();
            let ActRegistryReply::CacheMigration(record) = reply else {
                panic!("typed journal required")
            };
            if mode == "missing" {
                assert!(record.is_none());
            } else {
                let record = record.unwrap();
                assert_eq!(record.intent.nonce, nonce);
                assert_eq!(record.publication.is_some(), mode == "committed-absent");
            }
        });
    }
}
