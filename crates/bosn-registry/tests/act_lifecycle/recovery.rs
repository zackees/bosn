use super::*;

#[test]
fn tool_recovery_intent_survives_restart_and_refuses_changed_claim_or_lifetime() {
    let dir = TemporaryDirectory::new().unwrap();
    let path = dir.path().join("registry.sqlite3");
    let mut registry = Registry::create_writer(&path, OWNER).unwrap();
    let mut i = intent();
    let profile = i.creation_profile.as_mut().unwrap();
    profile.tmpfs_policy = ActEngineTmpfsPolicy::NamedDiskStorageRunTmpNoexecV2;
    profile.cache_volume = Some(ActEngineCacheVolume {
        name: "bosn-ci-cache-v1".into(),
        target: "/bosn/cache".into(),
    });
    profile.cache_coordination = Some(ActCacheCoordination::SharedLegacyLeaseV1);
    profile.tool_generation = Some(ActToolGenerationBinding {
        id: "a".repeat(64),
        max_payload_bytes: 100,
        overlay_recipe_sha256: "b".repeat(64),
    });
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_act_engine(&i).unwrap();
    tx.register_act_engine(RUN, &observed(&i), 2.0).unwrap();
    tx.claim_act_execution(&i, &observed(&i), CLAIM, 4.25)
        .unwrap();
    tx.commit().unwrap();
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .begin_act_tool_recovery(RUN, "wrong", 4, 3604)
            .is_err()
    );
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.begin_act_tool_recovery(RUN, CLAIM, 4, 3604).unwrap();
        // No commit: uncertain external reservations must never rely on this.
    }
    assert!(
        registry
            .act_engine(RUN)
            .unwrap()
            .unwrap()
            .tool_recovery
            .is_none()
    );
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .begin_act_tool_recovery(RUN, CLAIM, 4, 86405)
            .is_err()
    );
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.begin_act_tool_recovery(RUN, CLAIM, 4, 3604).unwrap();
        tx.commit().unwrap();
    }
    drop(registry);
    let mut registry = Registry::open_writer(&path).unwrap();
    let record = registry.act_engine(RUN).unwrap().unwrap();
    let frozen = record.tool_recovery.unwrap();
    assert_eq!(frozen.source_volume, i.storage_volume_name().unwrap());
    assert_eq!(frozen.engine_id, observed(&i).engine_id);
    assert_eq!(frozen.generation, "a".repeat(64));
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_act_tool_recovery(RUN, CLAIM, 4, 3604).unwrap();
    tx.commit().unwrap();
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .begin_act_tool_recovery(RUN, CLAIM, 4, 3605)
            .is_err()
    );
    assert_eq!(
        registry.act_engine(RUN).unwrap().unwrap().tool_recovery,
        Some(frozen)
    );
}
