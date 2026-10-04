//! Additive source identity: legacy receipts remain readable and stay cold.
use super::*;

#[test]
fn git_tree_wire_roundtrips_and_legacy_receipts_stay_absent() {
    let mut submitted = request("01234567-0123-0123-0123-0123456789ab", 'a');
    assert!(submitted.validate().is_ok());
    let legacy = serde_json::to_vec(&submitted).unwrap();
    let decoded: SubmitRequest = serde_json::from_slice(&legacy).unwrap();
    assert_eq!(decoded.git_tree, None);
    submitted.git_tree = Some("e".repeat(40));
    assert!(submitted.validate().is_ok());
    let decoded: SubmitRequest =
        serde_json::from_slice(&serde_json::to_vec(&submitted).unwrap()).unwrap();
    assert_eq!(decoded, submitted);
    let record = RunRecord::queued("run".into(), &submitted, "push", b"{}");
    assert_eq!(record.git_tree, submitted.git_tree);
    let decoded: RunRecord = serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
    assert_eq!(decoded, record);
    let legacy_record = sample_record("old");
    let decoded: RunRecord =
        serde_json::from_slice(&serde_json::to_vec(&legacy_record).unwrap()).unwrap();
    assert_eq!(decoded.git_tree, None);
    assert_eq!(decoded.schema_version, 1);
    for tree in ["", "--help", "G", &"a".repeat(64)] {
        submitted.git_tree = Some(tree.into());
        assert!(submitted.validate().is_err());
    }
}

#[test]
fn submitted_git_tree_is_checked_against_staged_effective_commit() {
    with_registry(|registry, dir| async move {
        let runtime = CiRuntime::start(
            &dir,
            registry,
            Arc::new(FakeBackend::with(Faults::default())),
            1,
        );
        for valid in [false, true] {
            let staging_id = new_uuid().await.unwrap();
            let source = runtime.staging_dir(&staging_id).join("source");
            std::fs::create_dir_all(&source).unwrap();
            snapshot::tests::sh(
                &source,
                "git init -q -b main . && printf 'source' > source.rs && git add . && git commit -qm init",
            );
            let commit = std::fs::read_to_string(source.join(".git/refs/heads/main"))
                .unwrap()
                .trim()
                .to_string();
            let tree = snapshot::effective_git_tree(&source, &commit).unwrap();
            let mut submitted = request(&staging_id, 'a');
            submitted.sha = commit;
            submitted.git_tree = Some(if valid { tree.clone() } else { "e".repeat(40) });
            let reply = runtime
                .handle(CiRequest::Submit {
                    request: Box::new(submitted),
                })
                .await;
            if valid {
                let reply: SubmitReply = serde_json::from_value(reply.unwrap()).unwrap();
                assert_eq!(
                    runtime.record(&reply.run).unwrap().git_tree.as_deref(),
                    Some(tree.as_str())
                );
                wait_done(&runtime, &reply.run).await;
            } else {
                assert_eq!(
                    reply.unwrap_err().message,
                    "staged Git tree identity mismatch"
                );
                assert!(
                    !source.exists(),
                    "refused staging is cleaned, no run queued"
                );
            }
        }
    });
}
