use super::*;

#[test]
fn named_storage_requires_its_own_exact_absence_receipt_after_partial_create() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    let mut i = intent();
    i.creation_profile.as_mut().unwrap().tmpfs_policy =
        ActEngineTmpfsPolicy::NamedDiskStorageRunTmpNoexecV2;
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_act_engine(&i).unwrap();
    tx.request_act_cleanup(RUN, ActRunOutcome::Failed, 2.0)
        .unwrap();
    tx.commit().unwrap();
    for volume in [None, Some("bosn-ci-cache-v1".to_string())] {
        assert!(
            registry
                .begin_immediate()
                .unwrap()
                .finalize_act_cleanup(
                    RUN,
                    &ActEngineRemovalProof {
                        name: i.engine_name(),
                        engine_id: None,
                        storage_volume: volume,
                    },
                    3.0
                )
                .is_err(),
            "container absence alone must not retire named disk storage"
        );
    }
    assert_eq!(
        registry.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::CleanupRequired
    );
    let mut tx = registry.begin_immediate().unwrap();
    tx.finalize_act_cleanup(
        RUN,
        &ActEngineRemovalProof {
            name: i.engine_name(),
            engine_id: None,
            storage_volume: i.storage_volume_name(),
        },
        3.0,
    )
    .unwrap();
    tx.commit().unwrap();
    drop(registry);
    let reopened = Registry::open_writer(dir.path().join("r")).unwrap();
    assert_eq!(
        reopened
            .act_engine(RUN)
            .unwrap()
            .unwrap()
            .removal
            .unwrap()
            .storage_volume,
        i.storage_volume_name()
    );
}
