//! Offline checks of the probe's evidence parsing and fake transports.

use super::*;

#[test]
pub(super) fn resource_sample_preserves_byte_inode_and_memory_axes_and_refuses_bad_data() {
    let raw = b"df_kib\nFilesystem 1024-blocks Used Available Capacity Mounted on\ntmpfs 20971520 1024 20970496 1% /var/lib/docker\ndf_inodes\nFilesystem Inodes IUsed IFree IUse% Mounted on\ntmpfs 1000 800 200 80% /var/lib/docker\nmemory_current\n4096\nmemory_peak\n8192\n";
    let metrics = resource_values(raw).unwrap();
    assert_eq!(metrics["tmpfs_total_bytes"], 20u64 << 30);
    assert_eq!(metrics["tmpfs_used_bytes"], 1 << 20);
    assert_eq!(metrics["inodes_used"], 800);
    assert_eq!(metrics["memory_peak"], 8192);
    for replacement in ["/foreign", "18446744073709551615"] {
        let text = std::str::from_utf8(raw).unwrap().replace(
            if replacement == "/foreign" {
                "/var/lib/docker"
            } else {
                "20971520"
            },
            replacement,
        );
        assert!(resource_values(text.as_bytes()).is_err());
    }
    assert!(resource_values(b"partial observation").is_err());
    assert_eq!(limits().storage_bytes, 20 << 30);
    assert_eq!(limits().memory_bytes, 28 << 30);
}

#[test]
pub(super) fn observer_failure_cannot_be_promoted_to_resource_measurement_or_runtime_success() {
    let failed = resource_receipt(
        Ok(bosn_engine::CommandResult {
            exit_code: 7,
            stdout: b"invented counters".to_vec(),
            stderr: b"df failed".to_vec(),
        }),
        "owned-id",
        Duration::from_secs(1),
    );
    assert_eq!(failed["command_exit"], 7);
    assert!(failed["metrics"].is_null());
    assert!(failed["observer_error"].is_string());
    assert!(failed["execution_success"].is_null());
    let unavailable =
        resource_receipt(Err("daemon unavailable".into()), "owned-id", Duration::ZERO);
    assert!(unavailable["command_exit"].is_null());
    assert!(unavailable["metrics"].is_null());
    assert_eq!(unavailable["observer_error"], "daemon unavailable");
}

#[test]
pub(super) fn nested_diagnostics_refuse_foreign_identity_and_preserve_failure() {
    let id = "a".repeat(64);
    let rows = serde_json::to_vec(&json!({"ID":id,"State":"created"})).unwrap();
    assert_eq!(
        nested_rows(&rows).unwrap(),
        vec![(id.clone(), "created".into())]
    );
    for bad in ["a".repeat(63), "A".repeat(64), "../foreign".into()] {
        assert!(
            nested_rows(&serde_json::to_vec(&json!({"ID":bad,"State":"created"})).unwrap())
                .is_err()
        );
    }
    let duplicated = [rows.clone(), b"\n".to_vec(), rows].concat();
    assert!(nested_rows(&duplicated).is_err());
    let too_many = (0..9)
        .map(|i| {
            serde_json::to_string(&json!({"ID":format!("{i:064x}"),"State":"created"})).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(nested_rows(too_many.as_bytes()).is_err());
    let output = bosn_engine::CommandResult {
        exit_code: 0,
        stdout: serde_json::to_vec(&json!([{"Id":"b".repeat(64),"Config":{"Env":["PATH=/bin"]}}]))
            .unwrap(),
        stderr: vec![],
    };
    let receipt = nested_receipt(Ok(output), &id, Duration::ZERO);
    assert!(receipt.get("observer_error").is_some());
    assert!(receipt.get("inspection").is_none());
    assert!(receipt.get("execution_success").is_none());
    let failure = nested_receipt(Err("capture deadline exceeded".into()), &id, Duration::ZERO);
    assert_eq!(failure["observer_error"], "capture deadline exceeded");
}

#[test]
pub(super) fn nested_diagnostic_fake_transport_retains_once_per_state_without_docker() {
    let root = std::env::temp_dir().join(format!(
        "bosn-nested-diagnostic-fixture-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    private_dir(&root).unwrap();
    let id = "c".repeat(64);
    let script = format!(
        "case \"$4\" in ps) printf '%s\n' '{{\"ID\":\"{id}\",\"State\":\"created\"}}';; inspect) printf '%s\n' '[{{\"Id\":\"{id}\",\"Config\":{{\"Env\":[\"PATH=/usr/bin:/bin\"]}},\"State\":{{\"Status\":\"created\"}},\"Mounts\":[]}}]';; *) exit 93;; esac"
    );
    let engine = DockerEngine::synthetic_for_test("/bin/sh", ["-c", &script, "fixture"]);
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..2 {
                sample_nested(
                    &engine,
                    "owned-outer",
                    &root,
                    &mut seen,
                    Duration::ZERO,
                    Duration::from_secs(2),
                )
                .await
                .unwrap();
            }
            assert_eq!(seen.len(), 1);
            let exited_script = script.replace("created", "exited");
            let exited =
                DockerEngine::synthetic_for_test("/bin/sh", ["-c", &exited_script, "fixture"]);
            sample_nested(
                &exited,
                "owned-outer",
                &root,
                &mut seen,
                Duration::ZERO,
                Duration::from_secs(2),
            )
            .await
            .unwrap();
            assert_eq!(seen.len(), 2);
            let forbidden = DockerEngine::synthetic_for_test(
                "/nonexistent-do-not-spawn",
                std::iter::empty::<String>(),
            );
            sample_nested(
                &forbidden,
                "owned-outer",
                &root,
                &mut seen,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await
            .unwrap();

            let receipt: Value = serde_json::from_slice(
                &bounded_file(&root.join("nested-00.json"), 1 << 20).unwrap(),
            )
            .unwrap();
            assert_eq!(
                receipt["inspection"][0]["Config"]["Env"][0],
                "PATH=/usr/bin:/bin"
            );
            assert!(root.join("nested-01.json").exists());
            assert!(!root.join("nested-02.json").exists());
        });
}

#[test]
pub(super) fn tail_archive_fake_transport_checks_exact_paths_and_preserves_binary_tar() {
    let root = std::env::temp_dir().join(format!(
        "bosn-tail-diagnostic-fixture-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    private_dir(&root).unwrap();
    let id = "d".repeat(64);
    std::os::unix::fs::symlink("/not-followed-loader-target", root.join("fixture-link")).unwrap();
    let allowed = FAILED_RUNNER_PATHS
        .iter()
        .map(|(_, p)| format!("{id}:{p}"))
        .collect::<Vec<_>>()
        .join("|");
    let script = format!(
        "[ \"$1\" = exec ] && [ \"$2\" = owned-outer ] && [ \"$3\" = docker ] && [ \"$4\" = cp ] && [ \"$6\" = - ] && [ -z \"$7\" ] || exit 94; case \"$5\" in {allowed}) printf '%s\\n' \"$5\" >> \"$FIXTURE_DIR/order\"; /usr/bin/tar --format=ustar -cf - -C \"$FIXTURE_DIR\" fixture-link;; *) exit 95;; esac"
    );
    let engine = DockerEngine::synthetic_for_test("/bin/sh", ["-c", &script, "fixture"])
        .env("FIXTURE_DIR", root.as_os_str());
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            capture_tail_archives(
                &engine,
                "owned-outer",
                &id,
                &root.join("archives"),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap();
        });
    for (label, _) in FAILED_RUNNER_PATHS {
        let bytes =
            bounded_file(&root.join("archives").join(format!("{label}.tar")), 1 << 20).unwrap();
        assert!(bytes.len() >= 1024);
        assert_eq!(bytes[156], b'2');
        assert!(bytes[157..257].starts_with(b"/not-followed-loader-target"));
        let receipt: Value = serde_json::from_slice(
            &bounded_file(&root.join("archives").join(format!("{label}.json")), 4096).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["command_exit"], 0);
        assert_eq!(receipt["sha256"], digest(&bytes));
        assert_eq!(receipt["bytes"], bytes.len());
        assert!(receipt.get("execution_success").is_none());
    }
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let failed = DockerEngine::synthetic_for_test(
                "/bin/sh",
                ["-c", "printf missing >&2; exit 17", "fixture"],
            );
            capture_tail_archives(
                &failed,
                "owned-outer",
                &id,
                &root.join("failed"),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap();
            let receipt: Value = serde_json::from_slice(
                &bounded_file(&root.join("failed/usr-bin-tail.json"), 4096).unwrap(),
            )
            .unwrap();
            assert_eq!(receipt["command_exit"], 17);
            assert!(receipt.get("observer_error").is_some());
            assert!(receipt.get("execution_success").is_none());
            let oversized = DockerEngine::synthetic_for_test(
                "/bin/sh",
                ["-c", "head -c 1048577 /dev/zero", "fixture"],
            );
            capture_tail_archives(
                &oversized,
                "owned-outer",
                &id,
                &root.join("oversized"),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap();
            let overflow: Value = serde_json::from_slice(
                &bounded_file(&root.join("oversized/loader-target.json"), 4096).unwrap(),
            )
            .unwrap();
            assert_eq!(overflow["captured"], false);
            assert!(overflow.get("observer_error").is_some());
            assert!(!root.join("oversized/loader-target.tar").exists());
            let forbidden = DockerEngine::synthetic_for_test(
                "/nonexistent-do-not-spawn",
                std::iter::empty::<String>(),
            );
            capture_tail_archives(
                &forbidden,
                "owned-outer",
                &id,
                &root.join("exhausted"),
                Instant::now(),
            )
            .await
            .unwrap();
            assert_eq!(
                std::fs::read_dir(root.join("exhausted")).unwrap().count(),
                6
            );
            let skipped: Value = serde_json::from_slice(
                &bounded_file(&root.join("exhausted/loader-target.json"), 4096).unwrap(),
            )
            .unwrap();
            assert_eq!(skipped["captured"], false);
            assert!(!root.join("exhausted/loader-target.tar").exists());
        });
    assert_eq!(
        std::fs::read_to_string(root.join("order"))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        FAILED_RUNNER_PATHS
            .iter()
            .map(|(_, p)| format!("{id}:{p}"))
            .collect::<Vec<_>>()
    );
    let good = json!({"inspection":[{"Image":RUNNER,"ImageManifestDescriptor":{"digest":RUNNER},"State":{"ExitCode":127,"Error":"exec: \"tail\": executable file not found in $PATH"}}]});
    assert!(failed_pinned_runner(&good));
    for field in ["Image", "ImageManifestDescriptor", "State"] {
        let mut foreign = good.clone();
        foreign["inspection"][0][field] = json!(null);
        assert!(!failed_pinned_runner(&foreign));
    }
}
